#[cfg(all(test, target_family = "unix"))]
use std::cell::RefCell;
#[cfg(target_family = "unix")]
use std::collections::{HashMap, HashSet};
#[cfg(target_family = "unix")]
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread;
use std::time::Duration;

use crate::backend::Backend;
use crate::input_protocol::format_input_frame_header;
#[cfg(target_family = "windows")]
use crate::ipc::{IPC_PIPE_FROM_WORKER_ENV, IPC_PIPE_TO_WORKER_ENV};
#[cfg(target_family = "unix")]
use crate::ipc::{IPC_READ_FD_ENV, IPC_WRITE_FD_ENV};
use crate::ipc::{
    IpcEchoEvent, IpcHandle, IpcServer, IpcWaitError, ServerIpcConnection,
    ServerToWorkerIpcMessage, WorkerToServerIpcMessage,
};
#[cfg(any(target_family = "unix", target_family = "windows"))]
use crate::ipc::{IpcHandlers, IpcPlotImage};
#[cfg(test)]
use crate::output_capture::OutputRange;
use crate::output_capture::{
    OUTPUT_RING_CAPACITY_BYTES, OutputBuffer, OutputEventKind, OutputTextSpan, OutputTimeline,
    ensure_output_ring, reset_last_reply_marker_offset, reset_output_ring,
    set_last_reply_marker_offset, update_last_reply_marker_offset_max,
};
use crate::output_timeline::{EchoCollapseMode, collapse_echo_with_attribution};
use crate::oversized_output::OversizedOutputMode;
use crate::pager::{self, Pager};
use crate::pending_output_tape::{FormattedPendingOutput, PendingOutputTape, PendingSidebandKind};
use crate::sandbox::{
    R_SESSION_TMPDIR_ENV, SandboxState, SandboxStateUpdate, prepare_worker_command,
};
use crate::sandbox_cli::{
    MISSING_INHERITED_SANDBOX_STATE_MESSAGE, SandboxCliPlan,
    is_missing_inherited_sandbox_state_error, resolve_effective_sandbox_state_with_defaults,
    sandbox_plan_requests_inherited_state,
};
use crate::worker_protocol::{
    ContentOrigin, TextStream, WORKER_MODE_ARG, WorkerContent, WorkerErrorCode, WorkerReply,
};

#[cfg(target_family = "unix")]
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
#[cfg(target_family = "unix")]
use std::os::unix::process::CommandExt;
#[cfg(target_family = "windows")]
use std::os::windows::io::AsRawHandle;
#[cfg(target_family = "windows")]
use std::os::windows::process::CommandExt;
#[cfg(target_family = "unix")]
use sysinfo::{Pid, ProcessesToUpdate, System};
#[cfg(target_family = "windows")]
use windows_sys::Win32::Foundation::{ERROR_BROKEN_PIPE, ERROR_HANDLE_EOF};
#[cfg(target_family = "windows")]
use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
#[cfg(target_family = "windows")]
use windows_sys::Win32::System::Pipes::PeekNamedPipe;
#[cfg(target_family = "windows")]
use windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;

#[cfg(all(test, target_family = "unix"))]
thread_local! {
    static TEST_UNIX_KILL_RECORDER: RefCell<Option<Vec<(i32, i32)>>> = const { RefCell::new(None) };
}

#[cfg(target_family = "unix")]
fn raw_unix_kill(target: i32, signal: i32) -> i32 {
    #[cfg(test)]
    if let Ok(Some(result)) = TEST_UNIX_KILL_RECORDER.try_with(|recorder| {
        let mut recorder = recorder.borrow_mut();
        recorder.as_mut().map(|calls| {
            calls.push((target, signal));
            0
        })
    }) {
        return result;
    }

    unsafe { libc::kill(target, signal) }
}

#[derive(Debug, Clone)]
struct GuardrailEvent {
    message: String,
    was_busy: bool,
}

#[derive(Clone)]
struct GuardrailShared {
    event: Arc<Mutex<Option<GuardrailEvent>>>,
    busy: Arc<AtomicBool>,
}

#[derive(Clone)]
struct LiveOutputCapture {
    pending_output_tape: Option<PendingOutputTape>,
    output_timeline: OutputTimeline,
}

impl LiveOutputCapture {
    fn new(
        oversized_output: OversizedOutputMode,
        pending_output_tape: PendingOutputTape,
        output_timeline: OutputTimeline,
    ) -> Self {
        Self {
            pending_output_tape: matches!(oversized_output, OversizedOutputMode::Files)
                .then_some(pending_output_tape),
            output_timeline,
        }
    }

    fn append_text(&self, bytes: &[u8], stream: TextStream) {
        match stream {
            TextStream::Stdout => {
                self.output_timeline
                    .append_text(bytes, false, ContentOrigin::Worker);
                if let Some(tape) = &self.pending_output_tape {
                    tape.append_stdout_bytes(bytes);
                }
            }
            TextStream::Stderr => {
                self.output_timeline
                    .append_text(bytes, true, ContentOrigin::Worker);
                if let Some(tape) = &self.pending_output_tape {
                    tape.append_stderr_bytes(bytes);
                }
            }
        }
    }

    fn append_image(&self, image: IpcPlotImage) {
        self.output_timeline.append_image(
            image.id.clone(),
            image.mime_type.clone(),
            image.data.clone(),
            image.is_new,
            image.readline_results_seen,
        );
        if let Some(tape) = &self.pending_output_tape {
            tape.append_image(
                image.id,
                image.mime_type,
                image.data,
                image.is_new,
                image.readline_results_seen,
            );
        }
    }

    fn append_sideband(&self, kind: PendingSidebandKind) {
        if let Some(tape) = &self.pending_output_tape {
            tape.append_sideband(kind);
        }
    }
}

#[cfg(target_family = "unix")]
const WORKER_MEM_GUARDRAIL_RATIO: f64 = 0.75;
#[cfg(target_family = "unix")]
const WORKER_MEM_GUARDRAIL_ACTIVE_INTERVAL: Duration = Duration::from_secs(10);
#[cfg(target_family = "unix")]
const WORKER_MEM_GUARDRAIL_IDLE_INTERVAL: Duration = Duration::from_secs(60);
#[cfg(target_os = "linux")]
const LINUX_BWRAP_FALLBACK_NOTICE: &str =
    "[repl] Linux bubblewrap sandbox unavailable; continuing without bwrap\n";

#[derive(Debug)]
pub enum WorkerError {
    Io(std::io::Error),
    Protocol(String),
    Timeout(Duration),
    Sandbox(String),
    Guardrail(String),
}

trait BackendDriver: Send {
    fn prepare_input_payload(&self, text: &str) -> Vec<u8>;
    fn on_input_start(&mut self, text: &str, ipc: &ServerIpcConnection);
    fn wait_for_completion(
        &mut self,
        timeout: Duration,
        ipc: ServerIpcConnection,
    ) -> Result<CompletionInfo, WorkerError>;
    fn interrupt(&mut self, process: &mut WorkerProcess) -> Result<(), WorkerError>;
    fn refresh_backend_info(
        &mut self,
        ipc: ServerIpcConnection,
        timeout: Duration,
    ) -> Result<(), WorkerError>;
}

struct RBackendDriver;

impl RBackendDriver {
    fn new() -> Self {
        Self
    }
}

fn driver_on_input_start(_text: &str, ipc: &ServerIpcConnection) {
    ipc.begin_request();
}

const REQUEST_END_FALLBACK_WAIT: Duration = Duration::from_millis(20);
fn driver_wait_for_completion(
    timeout: Duration,
    ipc: ServerIpcConnection,
) -> Result<CompletionInfo, WorkerError> {
    if timeout.is_zero() {
        return Err(WorkerError::Timeout(timeout));
    }
    let start = std::time::Instant::now();
    let deadline = start + timeout;
    loop {
        let now = std::time::Instant::now();
        if now >= deadline {
            return Err(WorkerError::Timeout(timeout));
        }
        let remaining = deadline.saturating_duration_since(now);
        let slice = remaining.min(Duration::from_millis(50));
        match ipc.wait_for_request_end(slice) {
            Ok(()) => {
                return Ok(completion_info_from_ipc(&ipc, false));
            }
            Err(IpcWaitError::Timeout) => {
                if ipc.waiting_for_next_input(REQUEST_END_FALLBACK_WAIT)
                    && ipc.try_take_request_end()
                {
                    return Ok(completion_info_from_ipc(&ipc, false));
                }
                continue;
            }
            Err(IpcWaitError::SessionEnd) => {
                return Ok(completion_info_from_ipc(&ipc, true));
            }
            Err(IpcWaitError::Disconnected) => {
                return Err(WorkerError::Protocol(
                    "ipc disconnected while waiting for request completion".to_string(),
                ));
            }
        }
    }
}

fn driver_interrupt(process: &mut WorkerProcess) -> Result<(), WorkerError> {
    if let Some(ipc) = process.ipc.get() {
        let _ = ipc.send(ServerToWorkerIpcMessage::Interrupt);
    }
    process.send_interrupt()
}

fn driver_refresh_backend_info(
    ipc: ServerIpcConnection,
    timeout: Duration,
    timeout_is_ok: bool,
) -> Result<(), WorkerError> {
    match ipc.wait_for_backend_info(timeout) {
        Ok(WorkerToServerIpcMessage::BackendInfo { .. }) => Ok(()),
        Ok(_) => Err(WorkerError::Protocol(
            "unexpected ipc message while waiting for backend info".to_string(),
        )),
        Err(IpcWaitError::Timeout) => {
            if timeout_is_ok {
                Ok(())
            } else {
                Err(WorkerError::Protocol(
                    "timed out waiting for backend info".to_string(),
                ))
            }
        }
        Err(IpcWaitError::Disconnected) => Err(WorkerError::Protocol(
            "ipc disconnected while waiting for backend info".to_string(),
        )),
        Err(IpcWaitError::SessionEnd) => Err(WorkerError::Protocol(
            "worker session ended before backend info".to_string(),
        )),
    }
}

impl BackendDriver for RBackendDriver {
    fn prepare_input_payload(&self, text: &str) -> Vec<u8> {
        let header = format_input_frame_header(text.len());
        let mut payload = Vec::with_capacity(header.len() + text.len());
        payload.extend_from_slice(header.as_bytes());
        payload.extend_from_slice(text.as_bytes());
        payload
    }

    fn on_input_start(&mut self, text: &str, ipc: &ServerIpcConnection) {
        driver_on_input_start(text, ipc);
    }

    fn wait_for_completion(
        &mut self,
        timeout: Duration,
        ipc: ServerIpcConnection,
    ) -> Result<CompletionInfo, WorkerError> {
        driver_wait_for_completion(timeout, ipc)
    }

    fn interrupt(&mut self, process: &mut WorkerProcess) -> Result<(), WorkerError> {
        if let Some(ipc) = process.ipc.get() {
            let _ = ipc.send(ServerToWorkerIpcMessage::Interrupt);
        }
        process.send_r_interrupt()
    }

    fn refresh_backend_info(
        &mut self,
        ipc: ServerIpcConnection,
        timeout: Duration,
    ) -> Result<(), WorkerError> {
        driver_refresh_backend_info(ipc, timeout, true)
    }
}

struct PythonBackendDriver;

impl PythonBackendDriver {
    fn new() -> Self {
        Self
    }
}

impl BackendDriver for PythonBackendDriver {
    fn prepare_input_payload(&self, text: &str) -> Vec<u8> {
        let mut data = text.to_string();
        if !data.ends_with('\n') && !data.ends_with('\r') {
            data.push('\n');
        }
        data.into_bytes()
    }

    fn on_input_start(&mut self, text: &str, ipc: &ServerIpcConnection) {
        driver_on_input_start(text, ipc);
        let _ = ipc.send(ServerToWorkerIpcMessage::StdinWrite {
            // Python-side IPC only needs a request-start signal; avoid duplicating large stdin
            // payloads on the control channel so interrupt/session-end messages stay responsive.
            text: String::new(),
        });
    }

    fn wait_for_completion(
        &mut self,
        timeout: Duration,
        ipc: ServerIpcConnection,
    ) -> Result<CompletionInfo, WorkerError> {
        driver_wait_for_completion(timeout, ipc)
    }

    fn interrupt(&mut self, process: &mut WorkerProcess) -> Result<(), WorkerError> {
        driver_interrupt(process)
    }

    fn refresh_backend_info(
        &mut self,
        ipc: ServerIpcConnection,
        timeout: Duration,
    ) -> Result<(), WorkerError> {
        driver_refresh_backend_info(ipc, timeout, false)
    }
}
impl std::fmt::Display for WorkerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkerError::Io(err) => write!(f, "worker io error: {err}"),
            WorkerError::Protocol(message) => write!(f, "worker protocol error: {message}"),
            WorkerError::Timeout(duration) => write!(
                f,
                "worker response timed out after {} ms",
                duration.as_millis()
            ),
            WorkerError::Sandbox(message) => write!(f, "worker sandbox error: {message}"),
            WorkerError::Guardrail(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for WorkerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            WorkerError::Io(err) => Some(err),
            _ => None,
        }
    }
}

const BACKEND_INFO_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(target_family = "windows")]
const WINDOWS_IPC_CONNECT_MAX_WAIT: Duration = Duration::from_secs(120);
const COMPLETION_METADATA_SETTLE_MAX: Duration = Duration::from_millis(30);
const COMPLETION_METADATA_SETTLE_POLL: Duration = Duration::from_millis(5);
const COMPLETION_METADATA_STABLE: Duration = Duration::from_millis(10);
const OUTPUT_READER_QUIESCE_GRACE: Duration = Duration::from_millis(120);

fn collect_completion_metadata(ipc: &ServerIpcConnection) -> (Option<String>, Vec<String>) {
    let mut prompt = ipc.try_take_prompt().filter(|value| !value.is_empty());
    let mut prompt_variants = ipc.take_prompt_history();
    let mut echo_event_count = ipc.pending_echo_event_count();
    let mut saw_late_echo_event = false;

    let start = std::time::Instant::now();
    let mut stable_for = Duration::from_millis(0);
    while start.elapsed() < COMPLETION_METADATA_SETTLE_MAX {
        thread::sleep(COMPLETION_METADATA_SETTLE_POLL);
        let next_prompt = ipc.try_take_prompt().filter(|value| !value.is_empty());
        let mut next_prompt_variants = ipc.take_prompt_history();
        let next_echo_event_count = ipc.pending_echo_event_count();
        if next_echo_event_count > echo_event_count {
            saw_late_echo_event = true;
        }
        let changed = next_prompt.is_some()
            || !next_prompt_variants.is_empty()
            || next_echo_event_count != echo_event_count;

        if let Some(value) = next_prompt {
            prompt = Some(value);
        }
        prompt_variants.append(&mut next_prompt_variants);
        echo_event_count = next_echo_event_count;

        if changed {
            stable_for = Duration::from_millis(0);
        } else {
            stable_for = stable_for.saturating_add(COMPLETION_METADATA_SETTLE_POLL);
            if !saw_late_echo_event && stable_for >= COMPLETION_METADATA_STABLE {
                break;
            }
        }
    }

    if prompt.is_none() {
        prompt = prompt_variants
            .iter()
            .rev()
            .find(|value| !value.is_empty())
            .cloned();
    }

    (prompt, prompt_variants)
}

impl From<std::io::Error> for WorkerError {
    fn from(err: std::io::Error) -> Self {
        WorkerError::Io(err)
    }
}

struct InputContext {
    prefix_contents: Vec<WorkerContent>,
    prefix_is_error: bool,
    start_offset: u64,
    prefix_bytes: u64,
    input_echo: Option<String>,
}

struct ReplyWithOffset {
    reply: WorkerReply,
    end_offset: u64,
}

struct RequestState {
    timeout: Duration,
    started_at: std::time::Instant,
}

struct SnapshotWithImages {
    contents: Vec<WorkerContent>,
    pages_left: u64,
    buffer: Option<pager::PagerBuffer>,
    last_range: Option<(u64, u64)>,
}

struct CompletionSnapshot {
    snapshot: SnapshotWithImages,
    saw_stderr: bool,
}

struct CompletionInfo {
    prompt: Option<String>,
    prompt_variants: Option<Vec<String>>,
    echo_events: Vec<IpcEchoEvent>,
    protocol_warnings: Vec<String>,
    session_end_seen: bool,
}

fn completion_info_from_ipc(ipc: &ServerIpcConnection, session_end_seen: bool) -> CompletionInfo {
    let (prompt, prompt_variants) = if session_end_seen {
        (None, None)
    } else {
        let (prompt, prompt_variants) = collect_completion_metadata(ipc);
        (prompt, Some(prompt_variants))
    };

    CompletionInfo {
        prompt,
        prompt_variants,
        echo_events: ipc.take_echo_events(),
        protocol_warnings: ipc.take_protocol_warnings(),
        session_end_seen,
    }
}

fn merge_completion_info_from_ipc(
    completion: &mut CompletionInfo,
    ipc: &ServerIpcConnection,
    session_end_seen: bool,
) {
    let late = completion_info_from_ipc(ipc, session_end_seen);
    if late.session_end_seen {
        completion.session_end_seen = true;
        completion.prompt = None;
        completion.prompt_variants = None;
    } else {
        if let Some(prompt) = late.prompt {
            completion.prompt = Some(prompt);
        }
        if let Some(late_variants) = late.prompt_variants {
            let variants = completion.prompt_variants.get_or_insert_with(Vec::new);
            for variant in late_variants {
                if !variants.iter().any(|existing| existing == &variant) {
                    variants.push(variant);
                }
            }
        }
    }
    completion.echo_events.extend(late.echo_events);
    completion.protocol_warnings.extend(late.protocol_warnings);
}

#[derive(Clone, Copy)]
enum WriteStdinControlAction {
    Interrupt,
    Restart,
}

fn split_write_stdin_control_prefix(input: &str) -> Option<(WriteStdinControlAction, &str)> {
    let first = input.chars().next()?;
    let action = match first {
        '\u{3}' => WriteStdinControlAction::Interrupt,
        '\u{4}' => WriteStdinControlAction::Restart,
        _ => return None,
    };

    let tail = &input[first.len_utf8()..];
    let tail = if let Some(rest) = tail.strip_prefix("\r\n") {
        rest
    } else if let Some(rest) = tail.strip_prefix('\n') {
        rest
    } else if let Some(rest) = tail.strip_prefix('\r') {
        rest
    } else {
        tail
    };
    Some((action, tail))
}

fn worker_context_event_payload(
    backend: Backend,
    sandbox_state: &SandboxState,
) -> serde_json::Value {
    let sandbox_policy = serde_json::to_value(&sandbox_state.sandbox_policy)
        .unwrap_or_else(|err| serde_json::json!({ "serialize_error": err.to_string() }));
    serde_json::json!({
        "backend": format!("{backend:?}"),
        "sandbox_policy": sandbox_policy,
        "sandbox_cwd": sandbox_state.sandbox_cwd.to_string_lossy().to_string(),
        "session_temp_dir": sandbox_state.session_temp_dir.to_string_lossy().to_string(),
        "use_linux_sandbox_bwrap": sandbox_state.use_linux_sandbox_bwrap,
        "managed_network_policy": {
            "allowed_domains": sandbox_state.managed_network_policy.allowed_domains.clone(),
            "denied_domains": sandbox_state.managed_network_policy.denied_domains.clone(),
            "allow_local_binding": sandbox_state.managed_network_policy.allow_local_binding,
        },
    })
}

pub struct WorkerManager {
    exe_path: PathBuf,
    backend: Backend,
    process: Option<WorkerProcess>,
    sandbox_plan: SandboxCliPlan,
    awaiting_initial_sandbox_state_update: bool,
    inherited_sandbox_state: Option<SandboxState>,
    sandbox_defaults: SandboxState,
    sandbox_state: SandboxState,
    #[cfg(target_os = "windows")]
    windows_sandbox_launch: Option<crate::windows_sandbox::PreparedSandboxLaunch>,
    oversized_output: OversizedOutputMode,
    pending_output_tape: PendingOutputTape,
    output: OutputBuffer,
    pager: Pager,
    output_timeline: OutputTimeline,
    driver: Box<dyn BackendDriver>,
    pending_request: bool,
    pending_request_started_at: Option<std::time::Instant>,
    pending_request_input: Option<String>,
    session_end_seen: bool,
    settled_pending_completion: Option<CompletionInfo>,
    last_detached_prefix_item_count: usize,
    pager_prompt: Option<String>,
    last_prompt: Option<String>,
    last_spawn: Option<std::time::Instant>,
    spawn_count: u64,
    guardrail: GuardrailShared,
}

impl WorkerManager {
    pub fn new(
        backend: Backend,
        sandbox_plan: SandboxCliPlan,
        oversized_output: OversizedOutputMode,
    ) -> Result<Self, WorkerError> {
        let exe_path = std::env::current_exe()?;
        let sandbox_defaults = crate::sandbox::sandbox_state_defaults_with_environment();
        let mut inherited_state = sandbox_defaults.clone();
        let mut inherited_update_received = false;
        if let Some(update) = crate::sandbox::initial_sandbox_state_update() {
            inherited_state.apply_update(update);
            inherited_update_received = true;
        }
        let inherited = if inherited_update_received {
            Some(&inherited_state)
        } else {
            None
        };
        let (sandbox_state, awaiting_initial_sandbox_state_update) =
            match resolve_effective_sandbox_state_with_defaults(
                &sandbox_plan,
                inherited,
                &sandbox_defaults,
            ) {
                Ok(state) => (state, false),
                Err(err)
                    if sandbox_plan_requests_inherited_state(&sandbox_plan)
                        && is_missing_inherited_sandbox_state_error(&err) =>
                {
                    // Allow MCP initialize to complete; first tool call will fail fast
                    // unless the client sends codex/sandbox-state/update.
                    (sandbox_defaults.clone(), true)
                }
                Err(err) => return Err(WorkerError::Sandbox(err)),
            };
        crate::event_log::log_lazy("worker_manager_created", || {
            worker_context_event_payload(backend, &sandbox_state)
        });
        if !awaiting_initial_sandbox_state_update {
            crate::sandbox::log_initial_sandbox_policy(&sandbox_state.sandbox_policy);
        }
        let output_timeline = {
            let output_ring = ensure_output_ring(OUTPUT_RING_CAPACITY_BYTES);
            reset_output_ring();
            reset_last_reply_marker_offset();
            OutputTimeline::new(output_ring)
        };
        Ok(Self {
            exe_path,
            backend,
            process: None,
            sandbox_plan,
            awaiting_initial_sandbox_state_update,
            inherited_sandbox_state: inherited_update_received.then_some(inherited_state),
            sandbox_defaults,
            sandbox_state,
            #[cfg(target_os = "windows")]
            windows_sandbox_launch: None,
            oversized_output,
            pending_output_tape: PendingOutputTape::new(),
            output: OutputBuffer::default(),
            pager: Pager::default(),
            output_timeline,
            driver: match backend {
                Backend::R => Box::new(RBackendDriver::new()),
                Backend::Python => Box::new(PythonBackendDriver::new()),
            },
            pending_request: false,
            pending_request_started_at: None,
            pending_request_input: None,
            session_end_seen: false,
            settled_pending_completion: None,
            last_detached_prefix_item_count: 0,
            pager_prompt: None,
            last_prompt: None,
            last_spawn: None,
            spawn_count: 0,
            guardrail: GuardrailShared {
                event: Arc::new(Mutex::new(None)),
                busy: Arc::new(AtomicBool::new(false)),
            },
        })
    }

    pub fn warm_start(&mut self) -> Result<(), WorkerError> {
        if self.awaiting_initial_sandbox_state_update {
            return Ok(());
        }
        self.ensure_process()
    }

    /// Exposes whether a timed-out logical request still owns future empty-input polls.
    pub fn pending_request(&self) -> bool {
        self.pending_request
    }

    pub fn detached_prefix_item_count(&self) -> usize {
        self.last_detached_prefix_item_count
    }

    fn reset_preserving_detached_prefix_item_count(&mut self) -> Result<(), WorkerError> {
        let detached_prefix_item_count = self.last_detached_prefix_item_count;
        let result = self.reset();
        self.last_detached_prefix_item_count = detached_prefix_item_count;
        result
    }

    fn reset_with_pager_preserving_detached_prefix_item_count(
        &mut self,
        preserve_pager: bool,
    ) -> Result<(), WorkerError> {
        let detached_prefix_item_count = self.last_detached_prefix_item_count;
        let result = self.reset_with_pager(preserve_pager);
        self.last_detached_prefix_item_count = detached_prefix_item_count;
        result
    }

    pub fn write_stdin(
        &mut self,
        text: String,
        worker_timeout: Duration,
        server_timeout: Duration,
        page_bytes_override: Option<u64>,
        echo_input: bool,
    ) -> Result<WorkerReply, WorkerError> {
        match self.oversized_output {
            OversizedOutputMode::Files => {
                self.write_stdin_files(text, worker_timeout, server_timeout)
            }
            OversizedOutputMode::Pager => self.write_stdin_pager(
                text,
                worker_timeout,
                server_timeout,
                page_bytes_override,
                echo_input,
            ),
        }
    }

    /// Entry point for the public `repl` tool in default files mode.
    fn write_stdin_files(
        &mut self,
        text: String,
        worker_timeout: Duration,
        server_timeout: Duration,
    ) -> Result<WorkerReply, WorkerError> {
        self.last_detached_prefix_item_count = 0;
        if let Some((control, remaining)) = split_write_stdin_control_prefix(&text) {
            self.clear_guardrail_busy_event();
            let control_reply = match control {
                WriteStdinControlAction::Interrupt => self.interrupt(worker_timeout),
                WriteStdinControlAction::Restart => self.restart(worker_timeout),
            }?;
            if remaining.is_empty() {
                return Ok(control_reply);
            }
            let control_prefix_item_count = prefixed_worker_reply_item_count(&control_reply);
            let remaining_reply =
                self.write_stdin_files(remaining.to_string(), worker_timeout, server_timeout)?;
            self.last_detached_prefix_item_count += control_prefix_item_count;
            return Ok(prefix_worker_reply(control_reply, remaining_reply));
        }

        if self.guardrail_busy_event_pending() {
            // Don't execute new input; the previous request was aborted.
            let event = self
                .guardrail
                .event
                .lock()
                .expect("guardrail event mutex poisoned")
                .take()
                .expect("guardrail event should be present");
            self.guardrail.busy.store(false, Ordering::Relaxed);
            let input_context = self.prepare_input_context_files();
            let err = WorkerError::Guardrail(event.message);
            let reply = self.build_reply_from_worker_error_files(&err, input_context);
            let _ = self.reset_preserving_detached_prefix_item_count();
            let reply = self.finalize_reply(reply);
            self.maybe_reset_after_session_end();
            return Ok(reply);
        }

        if let Err(err) = self.ensure_process() {
            let input_context = self.prepare_input_context_files();
            let reply = self.build_reply_from_worker_error_files(&err, input_context);
            let reply = self.finalize_reply(reply);
            self.maybe_reset_after_session_end();
            return Ok(reply);
        }
        self.maybe_emit_guardrail_notice();
        self.resolve_timeout_marker();
        if text.is_empty() {
            if self.pending_request
                || self.pending_output_tape.has_pending()
                || self.settled_pending_completion.is_some()
            {
                let reply = self.poll_pending_output_files(worker_timeout)?;
                let reply = self.finalize_reply(reply);
                self.maybe_reset_after_session_end();
                return Ok(reply);
            }
            let reply = self.build_idle_poll_reply_files();
            let reply = self.finalize_reply(reply);
            self.maybe_reset_after_session_end();
            return Ok(reply);
        }
        if !text.is_empty() && self.pending_request {
            self.resolve_timeout_marker_with_wait(Duration::from_millis(25));
        }
        if !text.is_empty() && self.pending_request {
            let mut reply = self.poll_pending_output_files(worker_timeout)?;
            let detached_prefix_item_count = match &reply.reply {
                WorkerReply::Output { contents, .. } => contents.len(),
            };
            self.last_detached_prefix_item_count = detached_prefix_item_count;
            mark_busy_follow_up_reply(&mut reply.reply);
            let reply = self.finalize_reply(reply);
            self.maybe_reset_after_session_end();
            return Ok(reply);
        }

        let input_context = self.prepare_input_context_files();

        let request = match self.send_worker_request(text, worker_timeout, server_timeout) {
            Ok(result) => result,
            Err(err) => {
                self.guardrail.busy.store(false, Ordering::Relaxed);
                let reply = self.build_reply_from_worker_error_files(&err, input_context);
                let _ = self.reset_preserving_detached_prefix_item_count();
                return Ok(self.finalize_reply(reply));
            }
        };
        let reply = self.build_reply_from_request_files(request, input_context)?;
        let reply = self.finalize_reply(reply);
        self.maybe_reset_after_session_end();
        Ok(reply)
    }

    fn write_stdin_pager(
        &mut self,
        text: String,
        worker_timeout: Duration,
        server_timeout: Duration,
        page_bytes_override: Option<u64>,
        echo_input: bool,
    ) -> Result<WorkerReply, WorkerError> {
        self.last_detached_prefix_item_count = 0;
        if let Some((control, remaining)) = split_write_stdin_control_prefix(&text) {
            self.clear_guardrail_busy_event();
            let control_reply = match control {
                WriteStdinControlAction::Interrupt => self.interrupt(worker_timeout),
                WriteStdinControlAction::Restart => self.restart(worker_timeout),
            }?;
            if remaining.is_empty() {
                return Ok(control_reply);
            }
            let control_prefix_item_count = prefixed_worker_reply_item_count(&control_reply);
            let remaining_reply = self.write_stdin_pager(
                remaining.to_string(),
                worker_timeout,
                server_timeout,
                page_bytes_override,
                echo_input,
            )?;
            self.last_detached_prefix_item_count += control_prefix_item_count;
            return Ok(prefix_worker_reply(control_reply, remaining_reply));
        }

        if self.guardrail_busy_event_pending() {
            let event = self
                .guardrail
                .event
                .lock()
                .expect("guardrail event mutex poisoned")
                .take()
                .expect("guardrail event should be present");
            self.guardrail.busy.store(false, Ordering::Relaxed);
            let page_bytes = pager::resolve_page_bytes(page_bytes_override);
            let input_context = self.prepare_input_context_pager(&text, echo_input);
            let err = WorkerError::Guardrail(event.message);
            let reply = self.build_reply_from_worker_error_pager(&err, input_context, page_bytes);
            let preserve_pager = self.pager.is_active();
            let _ = self.reset_with_pager_preserving_detached_prefix_item_count(preserve_pager);
            let reply = self.finalize_reply(reply);
            self.maybe_reset_after_session_end();
            return Ok(reply);
        }

        let page_bytes = pager::resolve_page_bytes(page_bytes_override);
        let empty_input = text.is_empty();
        if !empty_input && self.pager.is_active() {
            let trimmed = text.trim();
            if trimmed.is_empty() || trimmed.starts_with(':') {
                if let Some(reply) = self.handle_pager_command(&text) {
                    let reply = self.finalize_reply(reply);
                    self.maybe_reset_after_session_end();
                    return Ok(reply);
                }
            } else {
                self.pager.dismiss();
                self.pager_prompt = None;
            }
        }

        if empty_input {
            self.output.start_capture();
            self.maybe_emit_guardrail_notice();
            self.resolve_timeout_marker();
            if self.pending_request
                || self.output.has_pending_output()
                || self.settled_pending_completion.is_some()
            {
                let reply = self.poll_pending_output_pager(worker_timeout, page_bytes)?;
                let reply = self.finalize_reply(reply);
                self.maybe_reset_after_session_end();
                return Ok(reply);
            }
            if self.pager.is_active()
                && let Some(reply) = self.handle_pager_command(&text)
            {
                let reply = self.finalize_reply(reply);
                self.maybe_reset_after_session_end();
                return Ok(reply);
            }
        }

        if let Err(err) = self.ensure_process() {
            let input_context = self.prepare_input_context_pager(&text, echo_input);
            let reply = self.build_reply_from_worker_error_pager(&err, input_context, page_bytes);
            let reply = self.finalize_reply(reply);
            self.maybe_reset_after_session_end();
            return Ok(reply);
        }
        if !empty_input {
            self.output.start_capture();
            self.maybe_emit_guardrail_notice();
            self.resolve_timeout_marker();
        }
        if empty_input {
            let reply = self.build_idle_poll_reply_pager();
            let reply = self.finalize_reply(reply);
            self.maybe_reset_after_session_end();
            return Ok(reply);
        }
        if self.pending_request {
            self.resolve_timeout_marker_with_wait(Duration::from_millis(25));
        }
        if self.pending_request {
            let mut reply = self.poll_pending_output_pager(worker_timeout, page_bytes)?;
            let detached_prefix_item_count = match &reply.reply {
                WorkerReply::Output { contents, .. } => contents.len(),
            };
            self.last_detached_prefix_item_count = detached_prefix_item_count;
            mark_busy_follow_up_reply(&mut reply.reply);
            let reply = self.finalize_reply(reply);
            self.maybe_reset_after_session_end();
            return Ok(reply);
        }

        let input_context = self.prepare_input_context_pager(&text, echo_input);

        let request = match self.send_worker_request(text, worker_timeout, server_timeout) {
            Ok(result) => result,
            Err(err) => {
                self.guardrail.busy.store(false, Ordering::Relaxed);
                let reply =
                    self.build_reply_from_worker_error_pager(&err, input_context, page_bytes);
                let preserve_pager = self.pager.is_active();
                let _ = self.reset_with_pager_preserving_detached_prefix_item_count(preserve_pager);
                return Ok(self.finalize_reply(reply));
            }
        };
        let reply = self.build_reply_from_request_pager(request, input_context, page_bytes)?;
        let reply = self.finalize_reply(reply);
        self.maybe_reset_after_session_end();
        Ok(reply)
    }

    fn handle_pager_command(&mut self, text: &str) -> Option<ReplyWithOffset> {
        if !self.pager.is_active() {
            return None;
        }
        self.pager.refresh_from_output(&self.output);
        let mut reply = self.pager.handle_command(text);
        let pager_active = self.pager.is_active();
        let WorkerReply::Output {
            contents, prompt, ..
        } = &mut reply;
        let resolved_prompt = if pager_active {
            None
        } else {
            self.pager_prompt.take()
        };
        if pager_active {
            *prompt = None;
        } else {
            self.remember_prompt(resolved_prompt.clone());
            if resolved_prompt.is_none() {
                contents.push(WorkerContent::server_stderr(
                    "[repl] protocol error: missing prompt after pager dismiss",
                ));
            }
            append_prompt_if_missing(contents, resolved_prompt.clone());
            *prompt = resolved_prompt;
        }
        let end_offset = self.output.end_offset().unwrap_or(0);
        Some(ReplyWithOffset { reply, end_offset })
    }

    /// Serves empty-input polls and busy follow-up replies for a timed-out request.
    /// Each poll only returns newly available output, but the server may keep appending it to one transcript file.
    fn poll_pending_output_files(
        &mut self,
        timeout: Duration,
    ) -> Result<ReplyWithOffset, WorkerError> {
        let poll_start = std::time::Instant::now();
        let mut timed_out = false;
        let mut completed_request = false;
        let mut completion = CompletionInfo {
            prompt: None,
            prompt_variants: None,
            echo_events: Vec::new(),
            protocol_warnings: Vec::new(),
            session_end_seen: false,
        };

        if self.pending_request {
            match self.wait_for_request_completion(timeout) {
                Ok(info) => {
                    self.clear_pending_request_state();
                    if info.session_end_seen {
                        self.note_session_end(true);
                    }
                    completion = info;
                    completed_request = true;
                }
                Err(WorkerError::Timeout(_)) => {
                    let worker_exited = match self.process.as_mut() {
                        Some(process) => !process.is_running()?,
                        None => true,
                    };
                    if worker_exited {
                        self.note_session_end(true);
                        self.clear_pending_request_state();
                        completion.session_end_seen = true;
                        completed_request = true;
                    } else {
                        timed_out = true;
                    }
                }
                Err(err) => return Err(err),
            }
        }
        if !timed_out
            && !completed_request
            && let Some(info) = self.settled_pending_completion.take()
        {
            completion = info;
        }
        let FormattedPendingOutput {
            mut contents,
            saw_stderr,
        } = if timed_out {
            self.drain_formatted_output()
        } else {
            self.drain_final_formatted_output()
        };
        let is_error = saw_stderr;

        if timed_out {
            let elapsed = self
                .pending_request_started_at
                .map(|start| start.elapsed())
                .unwrap_or_else(|| poll_start.elapsed());
            contents.push(timeout_status_content(elapsed));
        }

        let session_end = completion.session_end_seen;
        let resolved_prompt = normalize_prompt(completion.prompt.clone());
        let resolved_prompt = if session_end || timed_out {
            None
        } else {
            resolved_prompt
        };
        self.remember_prompt(resolved_prompt.clone());
        let trim_enabled = !timed_out && should_trim_echo_prefix(&completion.echo_events);
        let echo_transcript = (!timed_out)
            .then(|| echo_transcript_from_events(&completion.echo_events))
            .flatten();
        trim_echo_then_append_protocol_warnings(
            &mut contents,
            echo_transcript.as_deref(),
            trim_enabled,
            should_drop_echo_only_contents(&completion.echo_events),
            &completion.protocol_warnings,
        );
        if !timed_out {
            let _ = trim_matching_echo_event_suffix_from_contents(
                &mut contents,
                &completion.echo_events,
            );
        }
        if !timed_out && !session_end && contents.is_empty() {
            contents.push(idle_status_content());
        }
        if !timed_out && !session_end {
            if let Some(prompt_text) = resolved_prompt.as_deref() {
                strip_prompt_from_contents(&mut contents, prompt_text);
            }
            append_prompt_if_missing(&mut contents, resolved_prompt.clone());
        }

        Ok(ReplyWithOffset {
            reply: WorkerReply::Output {
                contents,
                is_error,
                error_code: timed_out.then_some(WorkerErrorCode::Timeout),
                prompt: (!session_end).then_some(()).and(resolved_prompt),
                prompt_variants: completion.prompt_variants.clone(),
            },
            end_offset: 0,
        })
    }

    fn poll_pending_output_pager(
        &mut self,
        timeout: Duration,
        page_bytes: u64,
    ) -> Result<ReplyWithOffset, WorkerError> {
        let poll_start = std::time::Instant::now();
        let start_offset = self.output.current_offset().unwrap_or(0);
        let mut end_offset = self.output.end_offset().unwrap_or(start_offset);
        let mut timed_out = false;
        let mut completed_request = false;
        let mut completion = CompletionInfo {
            prompt: None,
            prompt_variants: None,
            echo_events: Vec::new(),
            protocol_warnings: Vec::new(),
            session_end_seen: false,
        };

        if self.pending_request {
            match self.wait_for_request_completion(timeout) {
                Ok(info) => {
                    self.clear_pending_request_state();
                    if info.session_end_seen {
                        self.note_session_end(true);
                    }
                    completion = info;
                    completed_request = true;
                    end_offset = self.output.end_offset().unwrap_or(end_offset);
                }
                Err(WorkerError::Timeout(_)) => {
                    end_offset = self.output.end_offset().unwrap_or(end_offset);
                    let worker_exited = match self.process.as_mut() {
                        Some(process) => !process.is_running()?,
                        None => true,
                    };
                    if worker_exited {
                        self.note_session_end(true);
                        self.clear_pending_request_state();
                        completion.session_end_seen = true;
                        completed_request = true;
                    } else {
                        timed_out = true;
                    }
                }
                Err(err) => return Err(err),
            }
        }
        if !completed_request && let Some(info) = self.settled_pending_completion.take() {
            completion = info;
            completed_request = true;
            end_offset = self.output.end_offset().unwrap_or(end_offset);
        }

        if end_offset < start_offset {
            end_offset = start_offset;
        }

        let (saw_stderr, snapshot) = if completed_request {
            let completed = snapshot_after_completion(
                &self.output,
                start_offset,
                end_offset,
                page_bytes,
                &completion,
            );
            (completed.saw_stderr, completed.snapshot)
        } else {
            let saw_stderr = self
                .output
                .saw_stderr_in_range(start_offset.min(end_offset), end_offset);
            let snapshot = snapshot_page_with_images(&self.output, end_offset, page_bytes);
            (saw_stderr, snapshot)
        };
        let is_error = saw_stderr;
        let page_is_error = saw_stderr;
        let SnapshotWithImages {
            mut contents,
            pages_left,
            buffer,
            last_range,
        } = snapshot;

        if timed_out {
            let elapsed = self
                .pending_request_started_at
                .map(|start| start.elapsed())
                .unwrap_or_else(|| poll_start.elapsed());
            contents.push(timeout_status_content(elapsed));
        }
        let trim_enabled = !timed_out && should_trim_echo_prefix(&completion.echo_events);
        let echo_transcript = (!timed_out)
            .then(|| echo_transcript_from_events(&completion.echo_events))
            .flatten();
        trim_echo_then_append_protocol_warnings(
            &mut contents,
            echo_transcript.as_deref(),
            trim_enabled,
            should_drop_echo_only_contents(&completion.echo_events),
            &completion.protocol_warnings,
        );
        pager::maybe_activate_and_append_footer(
            &mut self.pager,
            &mut contents,
            pages_left,
            page_is_error,
            buffer,
            last_range,
        );

        let session_end = completion.session_end_seen;
        let resolved_prompt = normalize_prompt(completion.prompt.clone());
        let resolved_prompt = if session_end || timed_out {
            None
        } else {
            resolved_prompt
        };
        self.remember_prompt(resolved_prompt.clone());
        if self.pager.is_active() && !session_end {
            self.pager_prompt = resolved_prompt.clone();
        }
        if !timed_out && !session_end {
            if let Some(prompt_text) = resolved_prompt.as_deref() {
                strip_prompt_from_contents(&mut contents, prompt_text);
            }
            if !self.pager.is_active() {
                append_prompt_if_missing(&mut contents, resolved_prompt.clone());
            }
        }

        Ok(ReplyWithOffset {
            reply: WorkerReply::Output {
                contents,
                is_error,
                error_code: timed_out.then_some(WorkerErrorCode::Timeout),
                prompt: (!self.pager.is_active() && !session_end)
                    .then_some(())
                    .and(resolved_prompt),
                prompt_variants: completion.prompt_variants.clone(),
            },
            end_offset,
        })
    }

    /// Drains detached output that arrived before the next accepted request so it can be prefixed
    /// into that request's visible reply.
    fn prepare_input_context_files(&mut self) -> InputContext {
        let settled_completion = self.settled_pending_completion.take();
        // A new accepted request seals the detached prefix. Flush any incomplete UTF-8 tail now
        // so it stays with the detached transcript instead of merging into fresh request output.
        let FormattedPendingOutput {
            mut contents,
            saw_stderr,
        } = self.drain_sealed_formatted_output();
        if let Some(completion) = settled_completion.as_ref() {
            let trim_enabled = should_trim_echo_prefix(&completion.echo_events);
            let echo_transcript = echo_transcript_from_events(&completion.echo_events);
            trim_echo_then_append_protocol_warnings(
                &mut contents,
                echo_transcript.as_deref(),
                trim_enabled,
                should_drop_echo_only_contents(&completion.echo_events),
                &completion.protocol_warnings,
            );
            let _ = trim_matching_echo_event_suffix_from_contents(
                &mut contents,
                &completion.echo_events,
            );
        }
        InputContext {
            prefix_contents: contents,
            prefix_is_error: saw_stderr,
            start_offset: 0,
            prefix_bytes: 0,
            input_echo: None,
        }
    }

    fn prepare_input_context_pager(&mut self, text: &str, echo_input: bool) -> InputContext {
        self.output.start_capture();

        let had_pending_output = self.output.has_pending_output();
        let saw_background_output = self.output.pending_output_since_last_reply();
        let prompt_hint = self.current_prompt_hint();
        self.remember_prompt(prompt_hint.clone());

        let mut input_echo = echo_input
            .then(|| text.to_string())
            .and_then(|value| pager::build_input_echo(&value));
        let settled_completion = self.settled_pending_completion.take();

        let mut prefix_contents = Vec::new();
        let mut prefix_bytes: u64 = 0;
        let mut prefix_is_error = false;

        if had_pending_output || settled_completion.is_some() {
            let pending_end = self.output.end_offset().unwrap_or(0);
            let pending_start = self.output.current_offset().unwrap_or(pending_end);
            let pending_bytes = pending_end.saturating_sub(pending_start);

            if let Some(completion) = settled_completion {
                let FormattedPendingOutput {
                    contents,
                    saw_stderr,
                } = take_range_from_ring_after_completion(
                    &self.output,
                    pending_start,
                    pending_end,
                    &completion,
                );
                prefix_is_error = saw_stderr;
                prefix_contents = contents;
            } else {
                prefix_is_error = self
                    .output
                    .saw_stderr_in_range(pending_start.min(pending_end), pending_end);
                prefix_contents = pager::take_range_from_ring(&self.output, pending_end);
            }
            prefix_bytes = pending_bytes;
        }

        let start_offset = self.output.end_offset().unwrap_or(0);
        if input_echo.is_none() && (echo_input || saw_background_output || had_pending_output) {
            input_echo = pager::build_input_echo(text);
        }

        InputContext {
            prefix_contents,
            prefix_is_error,
            start_offset,
            prefix_bytes,
            input_echo,
        }
    }

    fn send_worker_request(
        &mut self,
        text: String,
        worker_timeout: Duration,
        server_timeout: Duration,
    ) -> Result<RequestState, WorkerError> {
        let text = normalize_input_newlines(&text);
        let started_at = std::time::Instant::now();
        if matches!(self.oversized_output, OversizedOutputMode::Files) {
            let prompt = self.current_prompt_hint();
            self.remember_prompt(prompt.clone());
            self.pending_request_input = Some(text.clone());
        }
        let ipc = self
            .process
            .as_ref()
            .and_then(|process| process.ipc.get())
            .ok_or_else(|| WorkerError::Protocol("worker ipc unavailable".to_string()))?;
        self.driver.on_input_start(&text, &ipc);
        if server_timeout.is_zero() {
            return Err(WorkerError::Timeout(server_timeout));
        }
        let payload = self.driver.prepare_input_payload(&text);
        self.settled_pending_completion = None;
        self.guardrail.busy.store(true, Ordering::Relaxed);
        self.process
            .as_mut()
            .expect("worker process should be available")
            .write_stdin_payload(payload, server_timeout)?;
        Ok(RequestState {
            timeout: worker_timeout,
            started_at,
        })
    }

    fn build_reply_from_worker_error_files(
        &mut self,
        err: &WorkerError,
        context: InputContext,
    ) -> ReplyWithOffset {
        self.last_detached_prefix_item_count = context.prefix_contents.len();
        let mut contents = context.prefix_contents;
        let formatted = self.drain_sealed_formatted_output();
        contents.extend(formatted.contents);
        contents.push(WorkerContent::server_stderr(format!("worker error: {err}")));
        ReplyWithOffset {
            reply: WorkerReply::Output {
                contents,
                is_error: true,
                error_code: worker_error_code(err),
                prompt: None,
                prompt_variants: None,
            },
            end_offset: 0,
        }
    }

    fn build_reply_from_worker_error_pager(
        &mut self,
        err: &WorkerError,
        context: InputContext,
        page_bytes: u64,
    ) -> ReplyWithOffset {
        self.last_detached_prefix_item_count = context.prefix_contents.len();
        let end_offset = self.output.end_offset().unwrap_or(context.start_offset);
        let first_page_budget = page_bytes.saturating_sub(context.prefix_bytes);
        let mut contents = context.prefix_contents;
        if let Some(echo) = context.input_echo {
            contents.push(WorkerContent::stdout(echo));
        }
        let SnapshotWithImages {
            contents: mut page_contents,
            pages_left,
            buffer,
            last_range,
        } = snapshot_page_with_images(&self.output, end_offset, first_page_budget);
        contents.append(&mut page_contents);
        pager::maybe_activate_and_append_footer(
            &mut self.pager,
            &mut contents,
            pages_left,
            true,
            buffer,
            last_range,
        );
        contents.push(WorkerContent::server_stderr(format!("worker error: {err}")));
        ReplyWithOffset {
            reply: WorkerReply::Output {
                contents,
                is_error: true,
                error_code: worker_error_code(err),
                prompt: None,
                prompt_variants: None,
            },
            end_offset,
        }
    }

    fn build_reply_from_request_files(
        &mut self,
        request: RequestState,
        context: InputContext,
    ) -> Result<ReplyWithOffset, WorkerError> {
        self.last_detached_prefix_item_count = context.prefix_contents.len();
        match self.wait_for_request_completion(request.timeout) {
            Ok(completion) => {
                let mut session_end = completion.session_end_seen;
                if !session_end
                    && let Some(process) = self.process.as_mut()
                    && !process.is_running()?
                {
                    session_end = true;
                }
                if session_end {
                    self.note_session_end(true);
                }
                let mut contents = context.prefix_contents;
                let formatted = self.drain_final_formatted_output();
                let is_error = context.prefix_is_error || formatted.saw_stderr;
                contents.extend(formatted.contents);
                let resolved_prompt = if session_end {
                    None
                } else {
                    normalize_prompt(completion.prompt.clone())
                };
                self.remember_prompt(resolved_prompt.clone());
                let trim_enabled = should_trim_echo_prefix(&completion.echo_events);
                let echo_transcript = echo_transcript_from_events(&completion.echo_events);
                trim_echo_then_append_protocol_warnings(
                    &mut contents,
                    echo_transcript.as_deref(),
                    trim_enabled,
                    should_drop_echo_only_contents(&completion.echo_events),
                    &completion.protocol_warnings,
                );
                let _ = trim_matching_echo_event_suffix_from_contents(
                    &mut contents,
                    &completion.echo_events,
                );
                if !session_end {
                    if let Some(prompt_text) = resolved_prompt.as_deref() {
                        strip_prompt_from_contents(&mut contents, prompt_text);
                    }
                    append_prompt_if_missing(&mut contents, resolved_prompt.clone());
                }
                self.guardrail.busy.store(false, Ordering::Relaxed);
                Ok(ReplyWithOffset {
                    reply: WorkerReply::Output {
                        contents,
                        is_error,
                        error_code: None,
                        prompt: (!session_end).then_some(()).and(resolved_prompt),
                        prompt_variants: completion.prompt_variants.clone(),
                    },
                    end_offset: 0,
                })
            }
            Err(WorkerError::Timeout(_)) => {
                if let Some(process) = self.process.as_mut() {
                    match process.is_running() {
                        Ok(true) => {}
                        Ok(false) => {
                            return Err(WorkerError::Protocol(
                                "worker connection closed unexpectedly".to_string(),
                            ));
                        }
                        Err(err) => {
                            return Err(err);
                        }
                    }
                }

                if self.should_settle_multiline_r_timeout() {
                    self.settle_output_after_timeout();
                }
                self.pending_request = true;
                self.pending_request_started_at = Some(request.started_at);
                let mut contents = context.prefix_contents;
                let formatted = self.drain_formatted_output();
                contents.extend(formatted.contents);

                contents.push(timeout_status_content(request.started_at.elapsed()));

                let is_error = context.prefix_is_error || formatted.saw_stderr;

                Ok(ReplyWithOffset {
                    reply: WorkerReply::Output {
                        contents,
                        is_error,
                        error_code: Some(WorkerErrorCode::Timeout),
                        prompt: None,
                        prompt_variants: None,
                    },
                    end_offset: 0,
                })
            }
            Err(err) => {
                let reply = self.build_reply_from_worker_error_files(&err, context);
                let _ = self.reset_preserving_detached_prefix_item_count();
                Ok(reply)
            }
        }
    }

    fn build_reply_from_request_pager(
        &mut self,
        request: RequestState,
        context: InputContext,
        page_bytes: u64,
    ) -> Result<ReplyWithOffset, WorkerError> {
        self.last_detached_prefix_item_count = context.prefix_contents.len();
        match self.wait_for_request_completion(request.timeout) {
            Ok(completion) => {
                let mut session_end = completion.session_end_seen;
                if !session_end
                    && let Some(process) = self.process.as_mut()
                    && !process.is_running()?
                {
                    session_end = true;
                }
                if session_end {
                    self.note_session_end(true);
                }
                let end_offset = self.output.end_offset().unwrap_or(context.start_offset);
                let first_page_budget = page_bytes.saturating_sub(context.prefix_bytes);
                let mut contents = context.prefix_contents;
                if let Some(echo) = context.input_echo {
                    contents.push(WorkerContent::stdout(echo));
                }
                let completion_snapshot = snapshot_after_completion(
                    &self.output,
                    context.start_offset,
                    end_offset,
                    first_page_budget,
                    &completion,
                );
                let saw_stderr = completion_snapshot.saw_stderr;
                let is_error = context.prefix_is_error || saw_stderr;
                let page_is_error = is_error;
                let SnapshotWithImages {
                    contents: mut page_contents,
                    pages_left,
                    buffer,
                    last_range,
                } = completion_snapshot.snapshot;
                contents.append(&mut page_contents);
                pager::maybe_activate_and_append_footer(
                    &mut self.pager,
                    &mut contents,
                    pages_left,
                    page_is_error,
                    buffer,
                    last_range,
                );
                let resolved_prompt = if session_end {
                    None
                } else {
                    normalize_prompt(completion.prompt.clone())
                };
                self.remember_prompt(resolved_prompt.clone());
                if self.pager.is_active() && !session_end {
                    self.pager_prompt = resolved_prompt.clone();
                }
                let trim_enabled = should_trim_echo_prefix(&completion.echo_events);
                let echo_transcript = echo_transcript_from_events(&completion.echo_events);
                trim_echo_then_append_protocol_warnings(
                    &mut contents,
                    echo_transcript.as_deref(),
                    trim_enabled,
                    should_drop_echo_only_contents(&completion.echo_events),
                    &completion.protocol_warnings,
                );
                if !session_end {
                    if let Some(prompt_text) = resolved_prompt.as_deref() {
                        strip_prompt_from_contents(&mut contents, prompt_text);
                    }
                    if !self.pager.is_active() {
                        append_prompt_if_missing(&mut contents, resolved_prompt.clone());
                    }
                }
                self.guardrail.busy.store(false, Ordering::Relaxed);
                Ok(ReplyWithOffset {
                    reply: WorkerReply::Output {
                        contents,
                        is_error,
                        error_code: None,
                        prompt: (!self.pager.is_active() && !session_end)
                            .then_some(())
                            .and(resolved_prompt),
                        prompt_variants: completion.prompt_variants.clone(),
                    },
                    end_offset,
                })
            }
            Err(WorkerError::Timeout(_)) => {
                if let Some(process) = self.process.as_mut() {
                    match process.is_running() {
                        Ok(true) => {}
                        Ok(false) => {
                            return Err(WorkerError::Protocol(
                                "worker connection closed unexpectedly".to_string(),
                            ));
                        }
                        Err(err) => {
                            return Err(err);
                        }
                    }
                }

                self.pending_request = true;
                self.pending_request_started_at = Some(request.started_at);
                let end_offset = self.output.end_offset().unwrap_or(0);
                let first_page_budget = page_bytes.saturating_sub(context.prefix_bytes);
                let mut contents = context.prefix_contents;
                if let Some(echo) = context.input_echo {
                    contents.push(WorkerContent::stdout(echo));
                }
                let SnapshotWithImages {
                    contents: mut page_contents,
                    pages_left,
                    buffer,
                    last_range,
                } = snapshot_page_with_images(&self.output, end_offset, first_page_budget);
                contents.append(&mut page_contents);

                contents.push(timeout_status_content(request.started_at.elapsed()));

                let saw_stderr = self
                    .output
                    .saw_stderr_in_range(context.start_offset.min(end_offset), end_offset);
                let is_error = context.prefix_is_error || saw_stderr;

                pager::maybe_activate_and_append_footer(
                    &mut self.pager,
                    &mut contents,
                    pages_left,
                    is_error,
                    buffer,
                    last_range,
                );

                Ok(ReplyWithOffset {
                    reply: WorkerReply::Output {
                        contents,
                        is_error,
                        error_code: Some(WorkerErrorCode::Timeout),
                        prompt: None,
                        prompt_variants: None,
                    },
                    end_offset,
                })
            }
            Err(err) => {
                let reply = self.build_reply_from_worker_error_pager(&err, context, page_bytes);
                let preserve_pager = self.pager.is_active();
                let _ = self.reset_with_pager_preserving_detached_prefix_item_count(preserve_pager);
                Ok(reply)
            }
        }
    }

    fn wait_for_request_completion(
        &mut self,
        timeout: Duration,
    ) -> Result<CompletionInfo, WorkerError> {
        let Some(process) = self.process.as_ref() else {
            return Err(WorkerError::Protocol(
                "worker process unavailable".to_string(),
            ));
        };
        let ipc = process
            .ipc
            .get()
            .ok_or_else(|| WorkerError::Protocol("worker ipc unavailable".to_string()))?;
        let start = std::time::Instant::now();
        let mut result = self.driver.wait_for_completion(timeout, ipc.clone());
        if matches!(
            &result,
            Err(WorkerError::Protocol(message))
                if message.contains("ipc disconnected while waiting for request completion")
        ) {
            let deadline = std::time::Instant::now() + Duration::from_millis(500);
            let mut worker_exited = self.process.is_none();
            while !worker_exited {
                worker_exited = match self.process.as_mut() {
                    Some(process) => !process.is_running()?,
                    None => true,
                };
                if worker_exited || std::time::Instant::now() >= deadline {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            if worker_exited {
                result = Ok(CompletionInfo {
                    prompt: None,
                    prompt_variants: None,
                    echo_events: Vec::new(),
                    protocol_warnings: ipc.take_protocol_warnings(),
                    session_end_seen: true,
                });
            }
        }
        // Best-effort: after IPC completion, give the output reader threads a brief window to
        // drain any bytes already written by the worker before we snapshot the ring.
        let elapsed = start.elapsed();
        let remaining = timeout.saturating_sub(elapsed);
        self.settle_output_after_request_end(remaining);
        if let Ok(completion) = &mut result
            && !completion.session_end_seen
        {
            merge_completion_info_from_ipc(completion, &ipc, false);
        }
        if self.guardrail_event_pending() {
            let event = self
                .guardrail
                .event
                .lock()
                .expect("guardrail event mutex poisoned")
                .take()
                .expect("guardrail event should be present");
            return Err(WorkerError::Guardrail(event.message));
        }
        result
    }

    fn settle_output_after_request_end(&self, budget: Duration) {
        let total = budget.min(Duration::from_millis(120));
        if total.is_zero() {
            return;
        }
        let stable_needed = Duration::from_millis(15).min(total);
        self.settle_output_until_stable(total, stable_needed);
    }

    fn settle_output_after_timeout(&self) {
        let total = Duration::from_millis(2500);
        let stable_needed = Duration::from_millis(40);
        let poll = Duration::from_millis(5);
        let start = std::time::Instant::now();
        let baseline = self.pending_output_tape.current_settle_state();
        let mut last_seq = baseline.progress_seq;
        let mut ready = baseline.has_image;
        let mut stable_for = Duration::from_millis(0);
        while start.elapsed() < total {
            thread::sleep(poll);
            let now = self.pending_output_tape.current_settle_state();
            if !ready
                && (now.has_image || now.readline_results_seen > baseline.readline_results_seen)
            {
                ready = true;
                stable_for = Duration::from_millis(0);
                last_seq = now.progress_seq;
                continue;
            }
            if now.progress_seq == last_seq {
                stable_for = stable_for.saturating_add(poll);
                if ready && stable_for >= stable_needed {
                    return;
                }
            } else {
                last_seq = now.progress_seq;
                stable_for = Duration::from_millis(0);
            }
        }
    }

    fn should_settle_multiline_r_timeout(&self) -> bool {
        if self.backend != Backend::R {
            return false;
        }
        self.pending_request_input
            .as_deref()
            .map(|input| input.trim_end_matches(['\r', '\n']).contains('\n'))
            .unwrap_or(false)
    }

    fn settle_output_until_stable(&self, total: Duration, stable_needed: Duration) {
        if total.is_zero() {
            return;
        }
        let poll = Duration::from_millis(5);
        let start = std::time::Instant::now();

        let mut last = match self.oversized_output {
            OversizedOutputMode::Files => self.pending_output_tape.current_seq(),
            OversizedOutputMode::Pager => self.output.end_offset().unwrap_or(0),
        };
        let mut stable_for = Duration::from_millis(0);
        while start.elapsed() < total {
            thread::sleep(poll);
            let now = match self.oversized_output {
                OversizedOutputMode::Files => self.pending_output_tape.current_seq(),
                OversizedOutputMode::Pager => self.output.end_offset().unwrap_or(0),
            };
            if now == last {
                stable_for = stable_for.saturating_add(poll);
                if stable_for >= stable_needed {
                    return;
                }
            } else {
                last = now;
                stable_for = Duration::from_millis(0);
            }
        }
    }

    fn guardrail_event_pending(&self) -> bool {
        self.guardrail
            .event
            .lock()
            .expect("guardrail event mutex poisoned")
            .is_some()
    }

    fn guardrail_busy_event_pending(&self) -> bool {
        self.guardrail
            .event
            .lock()
            .expect("guardrail event mutex poisoned")
            .as_ref()
            .is_some_and(|event| event.was_busy)
    }

    fn clear_guardrail_busy_event(&mut self) {
        let mut slot = self
            .guardrail
            .event
            .lock()
            .expect("guardrail event mutex poisoned");
        if slot.as_ref().is_some_and(|event| event.was_busy) {
            *slot = None;
            self.guardrail.busy.store(false, Ordering::Relaxed);
        }
    }

    fn maybe_emit_guardrail_notice(&mut self) {
        let mut slot = self
            .guardrail
            .event
            .lock()
            .expect("guardrail event mutex poisoned");
        if slot.as_ref().is_some_and(|event| event.was_busy) {
            return;
        }
        let Some(event) = slot.take() else {
            return;
        };
        match self.oversized_output {
            OversizedOutputMode::Files => self
                .pending_output_tape
                .append_server_stderr_bytes(event.message.as_bytes()),
            OversizedOutputMode::Pager => self.output_timeline.append_text(
                event.message.as_bytes(),
                true,
                ContentOrigin::Server,
            ),
        }
    }

    fn finalize_reply(&self, reply: ReplyWithOffset) -> WorkerReply {
        if matches!(self.oversized_output, OversizedOutputMode::Pager) {
            set_last_reply_marker_offset(reply.end_offset);
        }
        reply.reply
    }

    fn note_session_end(&mut self, include_notice: bool) {
        self.session_end_seen = true;
        if let Some(process) = self.process.as_mut() {
            process.note_expected_exit();
            if include_notice {
                let status_message = process.exit_status_message().ok().flatten();
                if let Some(mut message) = status_message {
                    if !message.ends_with('\n') {
                        message.push('\n');
                    }
                    match self.oversized_output {
                        OversizedOutputMode::Files => self
                            .pending_output_tape
                            .append_server_stderr_status_line(message.as_bytes()),
                        OversizedOutputMode::Pager => {
                            self.output_timeline.append_text(
                                message.as_bytes(),
                                true,
                                ContentOrigin::Server,
                            );
                        }
                    }
                } else {
                    let message = "[repl] session ended\n".to_string();
                    match self.oversized_output {
                        OversizedOutputMode::Files => self
                            .pending_output_tape
                            .append_stdout_status_line(message.as_bytes()),
                        OversizedOutputMode::Pager => {
                            self.output_timeline.append_text(
                                message.as_bytes(),
                                false,
                                ContentOrigin::Server,
                            );
                        }
                    }
                }
            }
        }
    }

    fn maybe_reset_after_session_end(&mut self) {
        if self.session_end_seen {
            let _ = match self.oversized_output {
                OversizedOutputMode::Files => self.reset_preserving_detached_prefix_item_count(),
                OversizedOutputMode::Pager => self
                    .reset_with_pager_preserving_detached_prefix_item_count(self.pager.is_active()),
            };
            self.session_end_seen = false;
        }
    }

    pub fn interrupt(&mut self, timeout: Duration) -> Result<WorkerReply, WorkerError> {
        match self.oversized_output {
            OversizedOutputMode::Files => self.interrupt_files(timeout),
            OversizedOutputMode::Pager => self.interrupt_pager(timeout),
        }
    }

    fn interrupt_files(&mut self, timeout: Duration) -> Result<WorkerReply, WorkerError> {
        crate::event_log::log(
            "worker_interrupt_begin",
            serde_json::json!({
                "timeout_ms": timeout.as_millis(),
            }),
        );
        self.ensure_process()?;
        if self.pending_request
            && let Err(err) = self.driver.interrupt(
                self.process
                    .as_mut()
                    .expect("worker process should be available"),
            )
        {
            self.reset()?;
            crate::event_log::log(
                "worker_interrupt_error",
                serde_json::json!({
                    "error": err.to_string(),
                }),
            );
            return Err(err);
        }

        if self.pending_request {
            let mut reply = self.poll_pending_output_files(timeout)?;
            let prompt = match &reply.reply {
                WorkerReply::Output { prompt, .. } => prompt.clone(),
            };
            let WorkerReply::Output { contents, .. } = &mut reply.reply;
            if let Some(prompt) = prompt.as_deref() {
                strip_trailing_prompt(contents, prompt);
            }
            if let Some(prompt) = prompt {
                append_prompt_if_missing(contents, Some(prompt));
            }
            let reply = self.finalize_reply(reply);
            self.maybe_reset_after_session_end();
            return Ok(reply);
        }

        let mut timed_out = false;
        let mut prompt: Option<String> = None;
        if let Some(process) = self.process.as_ref()
            && let Some(ipc) = process.ipc.get()
        {
            let result = ipc.wait_for_prompt(timeout);
            match result {
                Ok(value) => {
                    prompt = Some(value);
                }
                Err(IpcWaitError::Timeout) => {
                    timed_out = true;
                }
                Err(IpcWaitError::SessionEnd) => {
                    self.note_session_end(true);
                }
                Err(IpcWaitError::Disconnected) => {
                    // IPC is optional for the R backend; fall back to prompt-as-output.
                }
            }
        }

        let FormattedPendingOutput {
            mut contents,
            saw_stderr,
        } = self.drain_formatted_output();
        let is_error = saw_stderr;

        if timed_out {
            contents.push(timeout_status_content(timeout));
        }

        let session_end = self.session_end_seen;
        let resolved_prompt = normalize_prompt(prompt.clone());
        let resolved_prompt = if session_end || timed_out {
            None
        } else {
            resolved_prompt
        };
        self.remember_prompt(resolved_prompt.clone());
        if !session_end {
            if let Some(prompt_text) = resolved_prompt.as_deref() {
                strip_trailing_prompt(&mut contents, prompt_text);
            }
            if !timed_out {
                append_prompt_if_missing(&mut contents, resolved_prompt.clone());
            }
        }

        let reply = WorkerReply::Output {
            contents,
            is_error,
            error_code: timed_out.then_some(WorkerErrorCode::Timeout),
            prompt: (!session_end).then_some(()).and(resolved_prompt),
            prompt_variants: None,
        };
        crate::event_log::log(
            "worker_interrupt_end",
            serde_json::json!({
                "timed_out": timed_out,
                "session_end": session_end,
            }),
        );
        Ok(self.finalize_reply(ReplyWithOffset {
            reply,
            end_offset: 0,
        }))
    }

    pub fn restart(&mut self, timeout: Duration) -> Result<WorkerReply, WorkerError> {
        match self.oversized_output {
            OversizedOutputMode::Files => self.restart_files(timeout),
            OversizedOutputMode::Pager => self.restart_pager(timeout),
        }
    }

    fn restart_files(&mut self, timeout: Duration) -> Result<WorkerReply, WorkerError> {
        crate::event_log::log(
            "worker_restart_begin",
            serde_json::json!({
                "timeout_ms": timeout.as_millis(),
            }),
        );
        if self.awaiting_initial_sandbox_state_update {
            return Err(WorkerError::Sandbox(
                MISSING_INHERITED_SANDBOX_STATE_MESSAGE.to_string(),
            ));
        }
        if let Some(process) = self.process.take() {
            let _ = process.shutdown_graceful(timeout);
        }
        self.guardrail.busy.store(false, Ordering::Relaxed);

        let reply = self.build_session_reset_reply_files("new session started");
        self.reset_output_state_files(true);
        crate::event_log::log("worker_restart_end", serde_json::json!({"status": "ok"}));
        Ok(self.finalize_reply(reply))
    }

    fn interrupt_pager(&mut self, timeout: Duration) -> Result<WorkerReply, WorkerError> {
        crate::event_log::log(
            "worker_interrupt_begin",
            serde_json::json!({
                "timeout_ms": timeout.as_millis(),
            }),
        );
        self.ensure_process()?;
        if self.pending_request
            && let Err(err) = self.driver.interrupt(
                self.process
                    .as_mut()
                    .expect("worker process should be available"),
            )
        {
            self.reset()?;
            crate::event_log::log(
                "worker_interrupt_error",
                serde_json::json!({
                    "error": err.to_string(),
                }),
            );
            return Err(err);
        }

        let page_bytes = pager::resolve_page_bytes(None);
        if self.pending_request {
            let mut reply = self.poll_pending_output_pager(timeout, page_bytes)?;
            let pager_active = self.pager.is_active();
            let prompt = match &reply.reply {
                WorkerReply::Output { prompt, .. } => prompt.clone(),
            };
            let WorkerReply::Output { contents, .. } = &mut reply.reply;
            if !pager_active {
                if let Some(prompt) = prompt.as_deref() {
                    strip_trailing_prompt(contents, prompt);
                }
                if let Some(prompt) = prompt {
                    append_prompt_if_missing(contents, Some(prompt));
                }
            }
            let reply = self.finalize_reply(reply);
            self.maybe_reset_after_session_end();
            return Ok(reply);
        }

        let mut timed_out = false;
        let mut prompt: Option<String> = None;
        if let Some(process) = self.process.as_ref()
            && let Some(ipc) = process.ipc.get()
        {
            let result = ipc.wait_for_prompt(timeout);
            match result {
                Ok(value) => {
                    prompt = Some(value);
                }
                Err(IpcWaitError::Timeout) => {
                    timed_out = true;
                }
                Err(IpcWaitError::SessionEnd) => {
                    self.note_session_end(true);
                }
                Err(IpcWaitError::Disconnected) => {}
            }
        }

        let start_offset = self.output.current_offset().unwrap_or(0);
        let mut end_offset = self.output.end_offset().unwrap_or(start_offset);
        if end_offset < start_offset {
            end_offset = start_offset;
        }

        let is_error = self
            .output
            .saw_stderr_in_range(start_offset.min(end_offset), end_offset);
        let SnapshotWithImages {
            mut contents,
            pages_left,
            buffer,
            last_range,
        } = snapshot_page_with_images(&self.output, end_offset, page_bytes);

        if timed_out {
            contents.push(timeout_status_content(timeout));
        }

        pager::maybe_activate_and_append_footer(
            &mut self.pager,
            &mut contents,
            pages_left,
            is_error,
            buffer,
            last_range,
        );

        let session_end = self.session_end_seen;
        let resolved_prompt = normalize_prompt(prompt.clone());
        let resolved_prompt = if session_end || timed_out {
            None
        } else {
            resolved_prompt
        };
        self.remember_prompt(resolved_prompt.clone());
        if self.pager.is_active() && !session_end {
            self.pager_prompt = resolved_prompt.clone();
        }
        if !session_end {
            if let Some(prompt_text) = resolved_prompt.as_deref() {
                strip_trailing_prompt(&mut contents, prompt_text);
            }
            if !timed_out && !self.pager.is_active() {
                append_prompt_if_missing(&mut contents, resolved_prompt.clone());
            }
        }

        let reply = WorkerReply::Output {
            contents,
            is_error,
            error_code: timed_out.then_some(WorkerErrorCode::Timeout),
            prompt: (!self.pager.is_active() && !session_end)
                .then_some(())
                .and(resolved_prompt),
            prompt_variants: None,
        };
        crate::event_log::log(
            "worker_interrupt_end",
            serde_json::json!({
                "timed_out": timed_out,
                "session_end": session_end,
            }),
        );
        Ok(self.finalize_reply(ReplyWithOffset { reply, end_offset }))
    }

    fn restart_pager(&mut self, timeout: Duration) -> Result<WorkerReply, WorkerError> {
        crate::event_log::log(
            "worker_restart_begin",
            serde_json::json!({
                "timeout_ms": timeout.as_millis(),
            }),
        );
        if self.awaiting_initial_sandbox_state_update {
            return Err(WorkerError::Sandbox(
                MISSING_INHERITED_SANDBOX_STATE_MESSAGE.to_string(),
            ));
        }
        if let Some(process) = self.process.take() {
            let _ = process.shutdown_graceful(timeout);
        }
        self.guardrail.busy.store(false, Ordering::Relaxed);

        let page_bytes = pager::resolve_page_bytes(None);
        let reply = self.build_session_reset_reply_pager(page_bytes, "new session started");
        self.reset_output_state_pager(true, false);
        crate::event_log::log("worker_restart_end", serde_json::json!({"status": "ok"}));
        Ok(self.finalize_reply(reply))
    }

    pub fn shutdown(&mut self) {
        crate::event_log::log("worker_shutdown", serde_json::json!({}));
        if let Some(process) = self.process.take() {
            let _ = process.kill();
        }
        self.guardrail.busy.store(false, Ordering::Relaxed);
    }

    fn ensure_process(&mut self) -> Result<(), WorkerError> {
        if self.awaiting_initial_sandbox_state_update {
            return Err(WorkerError::Sandbox(
                MISSING_INHERITED_SANDBOX_STATE_MESSAGE.to_string(),
            ));
        }
        let needs_spawn = match self.process.as_mut() {
            Some(process) => !process.is_running()?,
            None => true,
        };

        if needs_spawn {
            if let Some(process) = self.process.take() {
                process.finish_exited()?;
            }
            match self.oversized_output {
                OversizedOutputMode::Files => self.reset_output_state_files(false),
                OversizedOutputMode::Pager => self.reset_output_state_pager(true, false),
            }
            self.process = Some(match self.oversized_output {
                OversizedOutputMode::Files => self.spawn_process_files()?,
                OversizedOutputMode::Pager => self.spawn_process_with_pager(false)?,
            });
        }

        Ok(())
    }

    fn reset(&mut self) -> Result<(), WorkerError> {
        crate::event_log::log("worker_reset_begin", serde_json::json!({}));
        if let Some(process) = self.process.take() {
            let _ = process.kill();
        }
        if self.awaiting_initial_sandbox_state_update {
            return Err(WorkerError::Sandbox(
                MISSING_INHERITED_SANDBOX_STATE_MESSAGE.to_string(),
            ));
        }
        match self.oversized_output {
            OversizedOutputMode::Files => self.reset_output_state_files(true),
            OversizedOutputMode::Pager => self.reset_output_state_pager(true, false),
        }
        self.process = Some(match self.oversized_output {
            OversizedOutputMode::Files => self.spawn_process_files()?,
            OversizedOutputMode::Pager => self.spawn_process_with_pager(false)?,
        });
        crate::event_log::log("worker_reset_end", serde_json::json!({"status": "ok"}));
        Ok(())
    }

    fn reset_with_pager(&mut self, preserve_pager: bool) -> Result<(), WorkerError> {
        crate::event_log::log(
            "worker_reset_with_pager_begin",
            serde_json::json!({
                "preserve_pager": preserve_pager,
            }),
        );
        if let Some(process) = self.process.take() {
            let _ = process.kill();
        }
        if self.awaiting_initial_sandbox_state_update {
            return Err(WorkerError::Sandbox(
                MISSING_INHERITED_SANDBOX_STATE_MESSAGE.to_string(),
            ));
        }
        self.reset_output_state_pager(true, preserve_pager);
        self.process = Some(self.spawn_process_with_pager(preserve_pager)?);
        crate::event_log::log(
            "worker_reset_with_pager_end",
            serde_json::json!({
                "status": "ok",
                "preserve_pager": preserve_pager,
            }),
        );
        Ok(())
    }

    // Updates the server-side sandbox configuration. The new policy becomes
    // effective in the worker only after the current process is recycled and a
    // replacement worker is spawned. Session temp handling is separate: the
    // server-owned temp dir is reset before each spawn, and today that reset
    // reuses the same configured path in place.
    pub fn update_sandbox_state(
        &mut self,
        update: SandboxStateUpdate,
        timeout: Duration,
    ) -> Result<bool, WorkerError> {
        let update_for_log = serde_json::to_value(&update)
            .unwrap_or_else(|err| serde_json::json!({"serialize_error": err.to_string()}));
        crate::sandbox::log_sandbox_policy_update(&update.sandbox_policy);
        let mut inherited_state = self
            .inherited_sandbox_state
            .clone()
            .unwrap_or_else(|| self.sandbox_defaults.clone());
        inherited_state.apply_update(update);
        let resolved_state = resolve_effective_sandbox_state_with_defaults(
            &self.sandbox_plan,
            Some(&inherited_state),
            &self.sandbox_defaults,
        )
        .map_err(WorkerError::Sandbox)?;
        let awaiting_before = self.awaiting_initial_sandbox_state_update;
        self.awaiting_initial_sandbox_state_update = false;
        self.inherited_sandbox_state = Some(inherited_state);
        let changed = self.sandbox_state != resolved_state;
        self.sandbox_state = resolved_state;
        #[cfg(target_os = "windows")]
        if changed {
            // Prepared Windows launch state is keyed to the effective worker
            // sandbox configuration. Drop it before respawn so the next worker
            // picks up the updated sandbox state.
            self.windows_sandbox_launch = None;
        }
        crate::event_log::log(
            "worker_sandbox_state_update",
            serde_json::json!({
                "changed": changed,
                "timeout_ms": timeout.as_millis(),
                "update": update_for_log,
            }),
        );
        if !changed {
            if awaiting_before && self.process.is_none() {
                match self.oversized_output {
                    OversizedOutputMode::Files => self.reset_output_state_files(true),
                    OversizedOutputMode::Pager => self.reset_output_state_pager(true, false),
                }
                self.process = Some(match self.oversized_output {
                    OversizedOutputMode::Files => self.spawn_process_files()?,
                    OversizedOutputMode::Pager => self.spawn_process_with_pager(false)?,
                });
                return Ok(true);
            }
            return Ok(false);
        }

        if let Some(process) = self.process.take() {
            let _ = process.shutdown_graceful(timeout);
        }
        match self.oversized_output {
            OversizedOutputMode::Files => self.reset_output_state_files(true),
            OversizedOutputMode::Pager => self.reset_output_state_pager(true, false),
        }
        self.process = Some(match self.oversized_output {
            OversizedOutputMode::Files => self.spawn_process_files()?,
            OversizedOutputMode::Pager => self.spawn_process_with_pager(false)?,
        });
        Ok(true)
    }

    fn reset_output_state_files(&mut self, clear_pending_output: bool) {
        if clear_pending_output {
            self.pending_output_tape.clear();
        }
        self.pending_request = false;
        self.pending_request_started_at = None;
        self.pending_request_input = None;
        self.session_end_seen = false;
        self.settled_pending_completion = None;
        self.last_detached_prefix_item_count = 0;
        self.last_prompt = None;
        self.guardrail.busy.store(false, Ordering::Relaxed);
    }

    fn reset_output_state_pager(&mut self, clear_pending_output: bool, preserve_pager: bool) {
        if clear_pending_output {
            self.pending_output_tape.clear();
        }
        reset_output_ring();
        reset_last_reply_marker_offset();
        self.output = OutputBuffer::default();
        if !preserve_pager {
            self.pager = Pager::default();
        }
        self.pending_request = false;
        self.pending_request_started_at = None;
        self.pending_request_input = None;
        self.session_end_seen = false;
        self.settled_pending_completion = None;
        self.last_detached_prefix_item_count = 0;
        self.pager_prompt = None;
        self.last_prompt = None;
        self.guardrail.busy.store(false, Ordering::Relaxed);
    }

    fn remember_prompt(&mut self, prompt: Option<String>) {
        if let Some(prompt) = normalize_prompt(prompt) {
            self.last_prompt = Some(prompt);
        }
    }

    fn current_prompt_hint(&self) -> Option<String> {
        let prompt = self
            .process
            .as_ref()
            .and_then(|process| process.ipc.get())
            .and_then(|ipc| ipc.try_take_prompt())
            .and_then(|prompt| normalize_prompt(Some(prompt)));
        prompt.or_else(|| self.last_prompt.clone())
    }

    fn drain_formatted_output(&self) -> FormattedPendingOutput {
        self.pending_output_tape.drain_snapshot().format_contents()
    }

    fn drain_final_formatted_output(&self) -> FormattedPendingOutput {
        self.pending_output_tape
            .drain_final_snapshot()
            .format_contents_for_reply()
    }

    fn drain_sealed_formatted_output(&self) -> FormattedPendingOutput {
        self.pending_output_tape
            .drain_sealed_snapshot()
            .format_contents()
    }

    fn build_idle_poll_reply_files(&mut self) -> ReplyWithOffset {
        let prompt = self.current_prompt_hint();
        self.remember_prompt(prompt.clone());
        let mut contents = vec![idle_status_content()];
        append_prompt_if_missing(&mut contents, prompt.clone());
        ReplyWithOffset {
            reply: WorkerReply::Output {
                contents,
                is_error: false,
                error_code: None,
                prompt,
                prompt_variants: None,
            },
            end_offset: 0,
        }
    }

    fn build_idle_poll_reply_pager(&mut self) -> ReplyWithOffset {
        let prompt = self.current_prompt_hint();
        self.remember_prompt(prompt.clone());
        let mut contents = vec![idle_status_content()];
        append_prompt_if_missing(&mut contents, prompt.clone());
        ReplyWithOffset {
            reply: WorkerReply::Output {
                contents,
                is_error: false,
                error_code: None,
                prompt,
                prompt_variants: None,
            },
            end_offset: self.output.end_offset().unwrap_or(0),
        }
    }

    fn spawn_process_files(&mut self) -> Result<WorkerProcess, WorkerError> {
        #[cfg(target_os = "linux")]
        {
            loop {
                match self.spawn_process_files_once() {
                    Ok(process) => return Ok(process),
                    Err(err) => {
                        if self.maybe_retry_spawn_without_linux_bwrap(&err, false) {
                            continue;
                        }
                        return Err(err);
                    }
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        self.spawn_process_files_once()
    }

    fn spawn_process_files_once(&mut self) -> Result<WorkerProcess, WorkerError> {
        // Start each worker with a clean server-owned session temp dir. The
        // current implementation reuses the same configured path across
        // respawns and wipes/recreates it in place before launch.
        crate::sandbox::prepare_session_temp_dir(&self.sandbox_state.session_temp_dir)
            .map_err(|err| WorkerError::Sandbox(err.to_string()))?;
        crate::event_log::log_lazy("worker_spawn_begin", || {
            worker_context_event_payload(self.backend, &self.sandbox_state)
        });
        #[cfg(target_os = "windows")]
        let prepared_windows_launch = self.ensure_windows_sandbox_launch()?;
        let process = WorkerProcess::spawn(
            self.backend,
            &self.exe_path,
            &self.sandbox_state,
            WorkerSpawnContext {
                oversized_output: self.oversized_output,
                pending_output_tape: self.pending_output_tape.clone(),
                output_timeline: self.output_timeline.clone(),
                guardrail: self.guardrail.clone(),
                #[cfg(target_os = "windows")]
                prepared_windows_launch,
            },
        )?;
        let ipc = process
            .ipc
            .get()
            .ok_or_else(|| WorkerError::Protocol("worker ipc unavailable".to_string()))?;
        if let Err(err) = self.driver.refresh_backend_info(ipc, BACKEND_INFO_TIMEOUT) {
            let _ = process.kill();
            crate::event_log::log(
                "worker_spawn_error",
                serde_json::json!({
                    "error": err.to_string(),
                    "backend": format!("{:?}", self.backend),
                }),
            );
            return Err(err);
        }
        self.seed_last_prompt_from_process(&process);
        self.record_spawn();
        crate::event_log::log(
            "worker_spawn_end",
            serde_json::json!({
                "backend": format!("{:?}", self.backend),
                "spawn_count": self.spawn_count,
            }),
        );
        Ok(process)
    }

    fn spawn_process_with_pager(
        &mut self,
        preserve_pager: bool,
    ) -> Result<WorkerProcess, WorkerError> {
        #[cfg(target_os = "linux")]
        {
            loop {
                match self.spawn_process_with_pager_once(preserve_pager) {
                    Ok(process) => return Ok(process),
                    Err(err) => {
                        if self.maybe_retry_spawn_without_linux_bwrap(&err, preserve_pager) {
                            continue;
                        }
                        return Err(err);
                    }
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        self.spawn_process_with_pager_once(preserve_pager)
    }

    fn spawn_process_with_pager_once(
        &mut self,
        preserve_pager: bool,
    ) -> Result<WorkerProcess, WorkerError> {
        // Start each worker with a clean server-owned session temp dir. The
        // current implementation reuses the same configured path across
        // respawns and wipes/recreates it in place before launch.
        crate::sandbox::prepare_session_temp_dir(&self.sandbox_state.session_temp_dir)
            .map_err(|err| WorkerError::Sandbox(err.to_string()))?;
        crate::event_log::log_lazy("worker_spawn_begin", || {
            worker_context_event_payload(self.backend, &self.sandbox_state)
        });
        #[cfg(target_os = "windows")]
        let prepared_windows_launch = self.ensure_windows_sandbox_launch()?;
        let process = WorkerProcess::spawn(
            self.backend,
            &self.exe_path,
            &self.sandbox_state,
            WorkerSpawnContext {
                oversized_output: self.oversized_output,
                pending_output_tape: self.pending_output_tape.clone(),
                output_timeline: self.output_timeline.clone(),
                guardrail: self.guardrail.clone(),
                #[cfg(target_os = "windows")]
                prepared_windows_launch,
            },
        )?;
        let ipc = process
            .ipc
            .get()
            .ok_or_else(|| WorkerError::Protocol("worker ipc unavailable".to_string()))?;
        if let Err(err) = self.driver.refresh_backend_info(ipc, BACKEND_INFO_TIMEOUT) {
            let _ = process.kill();
            crate::event_log::log(
                "worker_spawn_error",
                serde_json::json!({
                    "error": err.to_string(),
                    "backend": format!("{:?}", self.backend),
                }),
            );
            return Err(err);
        }
        self.seed_last_prompt_from_process(&process);
        self.record_spawn();
        crate::event_log::log(
            "worker_spawn_end",
            serde_json::json!({
                "backend": format!("{:?}", self.backend),
                "spawn_count": self.spawn_count,
                "preserve_pager": preserve_pager,
            }),
        );
        Ok(process)
    }

    #[cfg(target_os = "linux")]
    fn maybe_retry_spawn_without_linux_bwrap(
        &mut self,
        err: &WorkerError,
        preserve_pager: bool,
    ) -> bool {
        if !self.sandbox_state.use_linux_sandbox_bwrap || !linux_sandbox_startup_retryable(err) {
            return false;
        }

        crate::event_log::log(
            "worker_spawn_retry_without_bwrap",
            serde_json::json!({
                "backend": format!("{:?}", self.backend),
                "error": err.to_string(),
            }),
        );

        self.sandbox_state.use_linux_sandbox_bwrap = false;
        self.sandbox_defaults.use_linux_sandbox_bwrap = false;
        if let Some(inherited_state) = self.inherited_sandbox_state.as_mut() {
            inherited_state.use_linux_sandbox_bwrap = false;
        }

        match self.oversized_output {
            OversizedOutputMode::Files => {
                self.reset_output_state_files(true);
                self.pending_output_tape
                    .append_stdout_status_line(LINUX_BWRAP_FALLBACK_NOTICE.as_bytes());
            }
            OversizedOutputMode::Pager => {
                self.reset_output_state_pager(true, preserve_pager);
                self.output_timeline.append_text(
                    LINUX_BWRAP_FALLBACK_NOTICE.as_bytes(),
                    false,
                    ContentOrigin::Server,
                );
            }
        }

        true
    }

    fn seed_last_prompt_from_process(&mut self, process: &WorkerProcess) {
        let Some(ipc) = process.ipc.get() else {
            return;
        };
        if let Some(prompt) = ipc
            .try_take_prompt()
            .and_then(|p| normalize_prompt(Some(p)))
        {
            self.last_prompt = Some(prompt);
            return;
        }
        match ipc.wait_for_prompt(Duration::from_millis(200)) {
            Ok(prompt) => {
                if let Some(prompt) = normalize_prompt(Some(prompt)) {
                    self.last_prompt = Some(prompt);
                }
            }
            Err(IpcWaitError::Timeout | IpcWaitError::SessionEnd | IpcWaitError::Disconnected) => {}
        }
    }

    fn record_spawn(&mut self) {
        let now = std::time::Instant::now();
        self.last_spawn = Some(now);
        self.spawn_count = self.spawn_count.saturating_add(1);
    }

    #[cfg(target_os = "windows")]
    fn ensure_windows_sandbox_launch(
        &mut self,
    ) -> Result<Option<crate::windows_sandbox::PreparedSandboxLaunch>, WorkerError> {
        if self.backend != Backend::R || !self.sandbox_state.sandbox_policy.requires_sandbox() {
            self.windows_sandbox_launch = None;
            return Ok(None);
        }

        let launch_matches = self.windows_sandbox_launch.as_ref().is_some_and(|launch| {
            launch.matches(
                &self.sandbox_state.sandbox_policy,
                &self.sandbox_state.sandbox_cwd,
                &self.sandbox_state.session_temp_dir,
            )
        });
        if launch_matches {
            // Reuse the prepared Windows launch only while the effective worker
            // sandbox configuration still matches. Session temp ACLs are
            // refreshed separately on each spawn after the temp dir reset.
            crate::windows_sandbox::refresh_prepared_sandbox_launch_acl_state(
                self.windows_sandbox_launch
                    .as_ref()
                    .expect("matching launch must exist"),
            )
            .map_err(WorkerError::Sandbox)?;
            return Ok(self.windows_sandbox_launch.clone());
        }

        crate::event_log::log_lazy("worker_windows_sandbox_prepare_begin", || {
            worker_context_event_payload(self.backend, &self.sandbox_state)
        });
        let prepared = crate::windows_sandbox::prepare_sandbox_launch(
            &self.sandbox_state.sandbox_policy,
            &self.sandbox_state.sandbox_cwd,
            &self.sandbox_state.session_temp_dir,
        );
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(err) => return Err(WorkerError::Sandbox(err)),
        };
        crate::event_log::log(
            "worker_windows_sandbox_prepare_end",
            serde_json::json!({
                "status": "ok",
                "capability_sid": prepared.capability_sid(),
            }),
        );
        self.windows_sandbox_launch = Some(prepared);

        Ok(self.windows_sandbox_launch.clone())
    }

    fn resolve_timeout_marker(&mut self) {
        self.resolve_timeout_marker_with_wait(Duration::from_millis(0));
    }

    fn resolve_timeout_marker_with_wait(&mut self, wait: Duration) {
        if !self.pending_request {
            return;
        }
        let Some(ipc) = self.process.as_ref().and_then(|process| process.ipc.get()) else {
            return;
        };
        let status = if wait.is_zero() {
            if ipc.try_take_request_end() {
                Ok(())
            } else {
                Err(IpcWaitError::Timeout)
            }
        } else {
            ipc.wait_for_request_end(wait)
        };
        match status {
            Ok(()) => {
                let mut settled_completion = completion_info_from_ipc(&ipc, false);
                self.settle_output_after_request_end(Duration::from_millis(120));
                if matches!(self.oversized_output, OversizedOutputMode::Pager) {
                    update_last_reply_marker_offset_max(self.output.end_offset().unwrap_or(0));
                }
                let worker_exited = match self.process.as_mut() {
                    Some(process) => match process.is_running() {
                        Ok(running) => !running,
                        Err(_) => false,
                    },
                    None => true,
                };
                self.clear_pending_request_state();
                if worker_exited {
                    settled_completion.session_end_seen = true;
                    self.note_session_end(true);
                } else {
                    self.remember_prompt(settled_completion.prompt.clone());
                }
                self.settled_pending_completion = Some(settled_completion);
            }
            Err(IpcWaitError::SessionEnd) => {
                self.note_session_end(true);
                self.clear_pending_request_state();
            }
            Err(IpcWaitError::Timeout | IpcWaitError::Disconnected) => {
                let worker_exited = self
                    .process
                    .as_mut()
                    .and_then(|process| process.is_running().ok())
                    .is_some_and(|running| !running);
                if worker_exited {
                    self.note_session_end(true);
                    self.clear_pending_request_state();
                }
            }
        }
    }

    fn clear_pending_request_state(&mut self) {
        self.pending_request = false;
        self.pending_request_started_at = None;
        self.pending_request_input = None;
        self.settled_pending_completion = None;
        self.guardrail.busy.store(false, Ordering::Relaxed);
    }

    fn build_session_reset_reply_files(&mut self, meta: &str) -> ReplyWithOffset {
        let FormattedPendingOutput {
            mut contents,
            saw_stderr,
        } = self.drain_sealed_formatted_output();
        contents.retain(|content| match content {
            WorkerContent::ContentText { text, .. } => !text.trim().is_empty(),
            _ => true,
        });
        let is_error = saw_stderr;
        if !meta.is_empty() {
            contents.push(WorkerContent::server_stderr(format!("[repl] {meta}")));
        }

        ReplyWithOffset {
            reply: WorkerReply::Output {
                contents,
                is_error,
                error_code: None,
                prompt: None,
                prompt_variants: None,
            },
            end_offset: 0,
        }
    }

    fn build_session_reset_reply_pager(&mut self, page_bytes: u64, meta: &str) -> ReplyWithOffset {
        let end_offset = self.output.end_offset().unwrap_or(0);
        let mut is_error = false;

        let SnapshotWithImages {
            mut contents,
            pages_left,
            buffer,
            last_range,
        } = snapshot_page_with_images(&self.output, end_offset, page_bytes);

        contents.retain(|content| match content {
            WorkerContent::ContentText { text, .. } => !text.trim().is_empty(),
            _ => true,
        });

        if !contents.is_empty() {
            let start_offset = self.output.current_offset().unwrap_or(end_offset);
            is_error = self
                .output
                .saw_stderr_in_range(start_offset.min(end_offset), end_offset);
        }

        if !meta.is_empty() {
            contents.push(WorkerContent::server_stderr(format!("[repl] {meta}")));
        }

        pager::maybe_activate_and_append_footer(
            &mut self.pager,
            &mut contents,
            pages_left,
            is_error,
            buffer,
            last_range,
        );

        ReplyWithOffset {
            reply: WorkerReply::Output {
                contents,
                is_error,
                error_code: None,
                prompt: None,
                prompt_variants: None,
            },
            end_offset,
        }
    }
}

fn snapshot_page_with_images(
    output: &OutputBuffer,
    end_offset: u64,
    target_bytes: u64,
) -> SnapshotWithImages {
    let start_offset = output.current_offset().unwrap_or(end_offset);
    let image_groups = collect_image_groups(output, start_offset, end_offset);
    let pager::SnapshotPage {
        mut contents,
        pages_left,
        buffer,
        last_range,
        last_range_end_byte,
    } = pager::take_snapshot_page_from_ring(output, end_offset, target_bytes);
    if pages_left == 0
        && pager::MAX_IMAGES_PER_PAGE > 0
        && contents
            .iter()
            .all(|content| !matches!(content, WorkerContent::ContentImage { .. }))
        && !image_groups.is_empty()
    {
        let max = pager::MAX_IMAGES_PER_PAGE.min(image_groups.len());
        for (_, image) in image_groups.into_iter().take(max) {
            contents.push(image);
        }
        return SnapshotWithImages {
            contents,
            pages_left,
            buffer,
            last_range,
        };
    }
    let page_end = page_end_offset(start_offset, end_offset, pages_left, last_range_end_byte);
    let mut remaining_images = pager::MAX_IMAGES_PER_PAGE;
    if remaining_images > 0 {
        let already = contents
            .iter()
            .filter(|content| matches!(content, WorkerContent::ContentImage { .. }))
            .count();
        remaining_images = remaining_images.saturating_sub(already);
    }
    if remaining_images > 0 && page_end < end_offset {
        append_image_groups_after_page(&mut contents, page_end, image_groups, remaining_images);
    }
    SnapshotWithImages {
        contents,
        pages_left,
        buffer,
        last_range,
    }
}

fn snapshot_page_with_images_from_collapsed(
    bytes: Vec<u8>,
    events: Vec<(u64, OutputEventKind)>,
    text_spans: Vec<OutputTextSpan>,
    source_end: u64,
    target_bytes: u64,
) -> SnapshotWithImages {
    let buffer = pager::PagerBuffer::from_bytes_and_events(bytes, events, text_spans, source_end);
    let pager::SnapshotPage {
        contents,
        pages_left,
        buffer,
        last_range,
        last_range_end_byte: _,
    } = pager::take_snapshot_page_from_buffer(buffer, target_bytes);
    SnapshotWithImages {
        contents,
        pages_left,
        buffer,
        last_range,
    }
}

fn snapshot_after_completion(
    output: &OutputBuffer,
    start_offset: u64,
    end_offset: u64,
    target_bytes: u64,
    completion: &CompletionInfo,
) -> CompletionSnapshot {
    if !completion.echo_events.is_empty() {
        let saw_stderr = output.saw_stderr_in_range(start_offset.min(end_offset), end_offset);
        let range = output.read_range(start_offset, end_offset);
        output.advance_offset_to(end_offset);
        let prompt_variants = completion.prompt_variants.clone().unwrap_or_default();
        let collapsed = collapse_echo_with_attribution(
            range,
            &completion.echo_events,
            0,
            &prompt_variants,
            EchoCollapseMode::CollapseForFinalReply,
        );
        let snapshot = snapshot_page_with_images_from_collapsed(
            collapsed.bytes,
            collapsed.events,
            collapsed.text_spans,
            end_offset,
            target_bytes,
        );
        return CompletionSnapshot {
            snapshot,
            saw_stderr,
        };
    }

    let saw_stderr = output.saw_stderr_in_range(start_offset.min(end_offset), end_offset);
    let snapshot = snapshot_page_with_images(output, end_offset, target_bytes);
    CompletionSnapshot {
        snapshot,
        saw_stderr,
    }
}

fn take_range_from_ring_after_completion(
    output: &OutputBuffer,
    start_offset: u64,
    end_offset: u64,
    completion: &CompletionInfo,
) -> FormattedPendingOutput {
    if !completion.echo_events.is_empty() {
        let saw_stderr = output.saw_stderr_in_range(start_offset.min(end_offset), end_offset);
        let range = output.read_range(start_offset, end_offset);
        output.advance_offset_to(end_offset);
        let prompt_variants = completion.prompt_variants.clone().unwrap_or_default();
        let collapsed = collapse_echo_with_attribution(
            range,
            &completion.echo_events,
            0,
            &prompt_variants,
            EchoCollapseMode::CollapseForFinalReply,
        );
        let mut contents = pager::contents_from_collapsed_output(
            collapsed.bytes,
            collapsed.events,
            collapsed.text_spans,
            end_offset,
        );
        append_protocol_warnings(&mut contents, &completion.protocol_warnings);
        return FormattedPendingOutput {
            contents,
            saw_stderr,
        };
    }

    let saw_stderr = output.saw_stderr_in_range(start_offset.min(end_offset), end_offset);
    let mut contents = pager::take_range_from_ring(output, end_offset);
    append_protocol_warnings(&mut contents, &completion.protocol_warnings);
    FormattedPendingOutput {
        contents,
        saw_stderr,
    }
}

fn page_end_offset(
    start_offset: u64,
    end_offset: u64,
    pages_left: u64,
    last_range_end_byte: Option<u64>,
) -> u64 {
    if pages_left == 0 {
        return end_offset;
    }
    if let Some(end_byte) = last_range_end_byte {
        return start_offset.saturating_add(end_byte);
    }
    start_offset
}

fn collect_image_groups(
    output: &OutputBuffer,
    start_offset: u64,
    end_offset: u64,
) -> Vec<(u64, WorkerContent)> {
    let range = output.read_range(start_offset, end_offset);
    let mut groups: Vec<(u64, WorkerContent)> = Vec::new();
    let mut current: Option<(u64, WorkerContent)> = None;

    for event in range.events.iter() {
        let (is_new, content) = match &event.kind {
            OutputEventKind::Image {
                data,
                mime_type,
                id,
                is_new,
                ..
            } => (
                *is_new,
                WorkerContent::ContentImage {
                    data: data.clone(),
                    mime_type: mime_type.clone(),
                    id: id.clone(),
                    is_new: *is_new,
                },
            ),
            _ => continue,
        };

        if is_new || current.is_none() {
            if let Some(prev) = current.take() {
                groups.push(prev);
            }
            current = Some((event.offset, content));
        } else {
            current = Some((event.offset, content));
        }
    }
    if let Some(prev) = current.take() {
        groups.push(prev);
    }

    groups
}

fn append_image_groups_after_page(
    contents: &mut Vec<WorkerContent>,
    page_end_offset: u64,
    groups: Vec<(u64, WorkerContent)>,
    max_images: usize,
) {
    let mut appended = 0usize;
    let mut last_offset = page_end_offset;
    for (offset, content) in groups {
        if offset <= page_end_offset {
            continue;
        }
        if appended >= max_images {
            break;
        }
        if offset > last_offset {
            contents.push(WorkerContent::server_stderr(format!(
                "[pager] elided output: @{last_offset}..{offset}\n"
            )));
        }
        contents.push(content);
        appended = appended.saturating_add(1);
        last_offset = offset;
    }
}

fn echo_transcript_from_events(events: &[IpcEchoEvent]) -> Option<String> {
    if events.is_empty() {
        return None;
    }
    let mut transcript = String::new();
    for event in events {
        transcript.push_str(&event.prompt);
        transcript.push_str(&event.line);
    }
    Some(transcript)
}

fn echo_event_prefix_len(line: &[u8], event: &IpcEchoEvent) -> Option<usize> {
    let consumed = event.line.as_bytes();
    match_echo_line_prefix(line, event.prompt.as_bytes(), consumed)
        .or_else(|| match_echo_line_prefix(line, b"", consumed))
}

fn match_echo_line_prefix(line: &[u8], prompt: &[u8], consumed: &[u8]) -> Option<usize> {
    if line.len() == prompt.len().saturating_add(consumed.len()) {
        let (prefix, suffix) = line.split_at(prompt.len());
        if prefix == prompt && suffix == consumed {
            return Some(line.len());
        }
    }

    let consumed = if let Some(consumed) = consumed.strip_suffix(b"\r\n") {
        consumed
    } else if let Some(consumed) = consumed.strip_suffix(b"\n") {
        consumed
    } else {
        return None;
    };
    let prefix_len = prompt.len().saturating_add(consumed.len());
    if line.len() <= prefix_len {
        return None;
    }
    let (prefix, suffix) = line.split_at(prompt.len());
    if prefix != prompt || !suffix.starts_with(consumed) {
        return None;
    }
    Some(prefix_len)
}
fn should_trim_echo_prefix(events: &[IpcEchoEvent]) -> bool {
    let Some((first, rest)) = events.split_first() else {
        return false;
    };
    if !is_primary_repl_prompt(&first.prompt) {
        return false;
    }
    if rest.is_empty() {
        return true;
    }
    rest.iter()
        .all(|event| is_continuation_prompt(&event.prompt))
}

fn should_drop_echo_only_contents(events: &[IpcEchoEvent]) -> bool {
    let Some((first, rest)) = events.split_first() else {
        return false;
    };
    if !is_primary_repl_prompt(&first.prompt) {
        return false;
    }
    rest.iter()
        .all(|event| is_primary_repl_prompt(&event.prompt) || is_continuation_prompt(&event.prompt))
}

fn is_primary_repl_prompt(prompt: &str) -> bool {
    matches!(
        prompt.trim_end_matches(|ch: char| ch.is_whitespace()),
        ">" | ">>>"
    )
}

fn is_continuation_prompt(prompt: &str) -> bool {
    let trimmed = prompt.trim_end_matches(|ch: char| ch.is_whitespace());
    if trimmed.is_empty() {
        return false;
    }
    if trimmed == "..." {
        return true;
    }
    trimmed.ends_with('+')
}

fn maybe_trim_echo_prefix(
    contents: &mut Vec<WorkerContent>,
    echo_prefix: Option<&str>,
    trim_enabled: bool,
) {
    if !trim_enabled {
        return;
    }
    let Some(echo_prefix) = echo_prefix else {
        return;
    };
    let _ = trim_matching_echo_prefix_from_contents(contents, echo_prefix);
}

fn trim_matching_echo_prefix_from_contents(
    contents: &mut Vec<WorkerContent>,
    echo_prefix: &str,
) -> bool {
    if echo_prefix.is_empty() {
        return false;
    }

    let mut remaining = echo_prefix;
    for content in contents.iter() {
        if remaining.is_empty() {
            break;
        }
        let WorkerContent::ContentText { text, stream, .. } = content else {
            return false;
        };
        if !matches!(stream, TextStream::Stdout) {
            return false;
        }
        if remaining.len() >= text.len() {
            if !remaining.starts_with(text.as_str()) {
                return false;
            }
            remaining = &remaining[text.len()..];
        } else {
            if !text.starts_with(remaining) {
                return false;
            }
            remaining = "";
        }
    }

    if !remaining.is_empty() {
        return false;
    }

    let mut remaining = echo_prefix;
    let mut idx = 0usize;
    while idx < contents.len() && !remaining.is_empty() {
        let remove_current = match &mut contents[idx] {
            WorkerContent::ContentText { text, .. } => {
                if remaining.len() >= text.len() {
                    remaining = &remaining[text.len()..];
                    text.clear();
                    true
                } else {
                    let updated = text[remaining.len()..].to_string();
                    *text = updated;
                    remaining = "";
                    false
                }
            }
            _ => return false,
        };

        if remove_current {
            contents.remove(idx);
            continue;
        }
        idx = idx.saturating_add(1);
    }

    true
}

fn trim_matching_echo_event_suffix_from_contents(
    contents: &mut Vec<WorkerContent>,
    echo_events: &[IpcEchoEvent],
) -> bool {
    for start in 0..echo_events.len() {
        if !should_drop_echo_only_contents(&echo_events[start..]) {
            continue;
        }
        if trim_matching_echo_event_prefix_from_contents(contents, &echo_events[start..]) {
            return true;
        }
    }
    false
}

fn trim_matching_echo_event_prefix_from_contents(
    contents: &mut Vec<WorkerContent>,
    echo_events: &[IpcEchoEvent],
) -> bool {
    if echo_events.is_empty() {
        return false;
    }

    let mut leading_text = String::new();
    for content in contents.iter() {
        let WorkerContent::ContentText {
            text,
            stream,
            origin,
        } = content
        else {
            break;
        };
        if !matches!(stream, TextStream::Stdout) || !matches!(origin, ContentOrigin::Worker) {
            break;
        }
        leading_text.push_str(text);
    }
    if leading_text.is_empty() {
        return false;
    }

    let bytes = leading_text.as_bytes();
    let mut start = 0usize;
    let mut trim_bytes = 0usize;
    let mut echo_idx = 0usize;

    while start < bytes.len() && echo_idx < echo_events.len() {
        let mut end = start;
        while end < bytes.len() && bytes[end] != b'\n' {
            end += 1;
        }
        if end < bytes.len() && bytes[end] == b'\n' {
            end += 1;
        }

        let line = &bytes[start..end];
        let Some(mut prefix_len) = echo_event_prefix_len(line, &echo_events[echo_idx]) else {
            return false;
        };
        let remainder = &line[prefix_len..];
        if matches!(remainder, b"\r\n" | b"\n" | b"\r") {
            prefix_len = line.len();
        } else if !remainder.is_empty() {
            return false;
        }

        trim_bytes = trim_bytes.saturating_add(prefix_len);
        start = end;
        echo_idx = echo_idx.saturating_add(1);
    }

    if echo_idx != echo_events.len() {
        return false;
    }

    trim_leading_text_prefix(contents, trim_bytes)
}

fn trim_leading_text_prefix(contents: &mut Vec<WorkerContent>, mut prefix_bytes: usize) -> bool {
    if prefix_bytes == 0 {
        return false;
    }

    let mut idx = 0usize;
    while idx < contents.len() && prefix_bytes > 0 {
        let remove_current = match &mut contents[idx] {
            WorkerContent::ContentText {
                text,
                stream,
                origin,
            } if matches!(stream, TextStream::Stdout)
                && matches!(origin, ContentOrigin::Worker) =>
            {
                if prefix_bytes >= text.len() {
                    prefix_bytes -= text.len();
                    text.clear();
                    true
                } else {
                    if !text.is_char_boundary(prefix_bytes) {
                        return false;
                    }
                    *text = text[prefix_bytes..].to_string();
                    prefix_bytes = 0;
                    false
                }
            }
            _ => break,
        };
        if remove_current {
            contents.remove(idx);
        } else {
            idx = idx.saturating_add(1);
        }
    }

    prefix_bytes == 0
}

fn drop_echo_only_contents(contents: &mut Vec<WorkerContent>, echo: &str) -> bool {
    if echo.is_empty() {
        return false;
    }

    let mut remaining = echo;
    for content in contents.iter() {
        let WorkerContent::ContentText {
            text,
            stream,
            origin,
        } = content
        else {
            return false;
        };
        if !matches!(stream, TextStream::Stdout) || !matches!(origin, ContentOrigin::Worker) {
            return false;
        }
        if remaining.len() >= text.len() {
            if !remaining.starts_with(text.as_str()) {
                return false;
            }
            remaining = &remaining[text.len()..];
        } else {
            return false;
        }
    }

    if !remaining.is_empty() {
        return false;
    }

    contents.clear();
    true
}

fn trim_echo_then_append_protocol_warnings(
    contents: &mut Vec<WorkerContent>,
    echo: Option<&str>,
    trim_enabled: bool,
    drop_echo_only_enabled: bool,
    warnings: &[String],
) {
    maybe_trim_echo_prefix(contents, echo, trim_enabled);
    if drop_echo_only_enabled && let Some(echo) = echo {
        let _ = drop_echo_only_contents(contents, echo);
    }
    append_protocol_warnings(contents, warnings);
}

fn normalize_prompt(prompt: Option<String>) -> Option<String> {
    prompt.filter(|value| !value.is_empty())
}

fn normalize_input_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn timeout_status_content(timeout: Duration) -> WorkerContent {
    let elapsed_ms = duration_to_millis(timeout);
    let elapsed_ms = (elapsed_ms / TIMEOUT_STATUS_GRANULARITY_MS) * TIMEOUT_STATUS_GRANULARITY_MS;
    WorkerContent::server_stdout(format!(
        "<<repl status: busy, write_stdin timeout reached; elapsed_ms={elapsed_ms}>>"
    ))
}

fn idle_status_content() -> WorkerContent {
    WorkerContent::server_stdout("<<repl status: idle>>")
}

fn append_protocol_warnings(contents: &mut Vec<WorkerContent>, warnings: &[String]) {
    for warning in warnings {
        contents.push(WorkerContent::server_stderr(format!("[repl] {warning}")));
    }
}

const TIMEOUT_STATUS_GRANULARITY_MS: u64 = 100;

fn append_prompt_if_missing(contents: &mut Vec<WorkerContent>, prompt: Option<String>) {
    let Some(prompt) = prompt else {
        return;
    };
    if prompt.is_empty() {
        return;
    }
    if let Some(WorkerContent::ContentText { text, .. }) = contents
        .iter()
        .rev()
        .find(|content| matches!(content, WorkerContent::ContentText { .. }))
        && text.ends_with(&prompt)
    {
        return;
    }
    contents.push(WorkerContent::worker_stdout(prompt));
}

fn strip_trailing_prompt(contents: &mut Vec<WorkerContent>, prompt: &str) {
    if prompt.is_empty() {
        return;
    }
    let idx = contents
        .iter()
        .rposition(|content| matches!(content, WorkerContent::ContentText { .. }));
    let Some(idx) = idx else {
        return;
    };
    let WorkerContent::ContentText { text, stream, .. } = &contents[idx] else {
        return;
    };
    let Some(prefix) = text.strip_suffix(prompt) else {
        return;
    };
    if prefix.is_empty() {
        contents.remove(idx);
    } else {
        contents[idx] = WorkerContent::ContentText {
            text: prefix.to_string(),
            stream: *stream,
            origin: crate::worker_protocol::ContentOrigin::Worker,
        };
    }
}

fn strip_prompt_from_contents(contents: &mut Vec<WorkerContent>, prompt: &str) {
    if prompt.is_empty() {
        return;
    }
    let mut idx = 0usize;
    while idx < contents.len() {
        let remove = match &contents[idx] {
            WorkerContent::ContentText { text, stream, .. } => {
                if !matches!(stream, crate::worker_protocol::TextStream::Stdout) {
                    false
                } else if text == prompt {
                    true
                } else if let Some(prefix) = text.strip_suffix(prompt) {
                    if prefix.is_empty() {
                        true
                    } else {
                        contents[idx] = WorkerContent::ContentText {
                            text: prefix.to_string(),
                            stream: *stream,
                            origin: crate::worker_protocol::ContentOrigin::Worker,
                        };
                        false
                    }
                } else {
                    false
                }
            }
            _ => false,
        };
        if remove {
            contents.remove(idx);
        } else {
            idx = idx.saturating_add(1);
        }
    }
}

fn prefix_worker_reply(prefix: WorkerReply, suffix: WorkerReply) -> WorkerReply {
    let WorkerReply::Output {
        mut contents,
        is_error,
        error_code,
        prompt,
        prompt_variants,
    } = prefix;
    let WorkerReply::Output {
        contents: suffix_contents,
        is_error: suffix_is_error,
        error_code: suffix_error_code,
        prompt: suffix_prompt,
        prompt_variants: suffix_prompt_variants,
    } = suffix;
    if let Some(prompt_text) = prompt.as_deref() {
        strip_trailing_prompt(&mut contents, prompt_text);
    }
    if let Some(WorkerContent::ContentText {
        text: prefix_text, ..
    }) = contents.last_mut()
        && let Some(WorkerContent::ContentText {
            text: suffix_text, ..
        }) = suffix_contents.first()
        && !prefix_text.is_empty()
        && !suffix_text.is_empty()
        && !prefix_text.ends_with('\n')
        && !suffix_text.starts_with('\n')
    {
        prefix_text.push('\n');
    }
    contents.extend(suffix_contents);
    WorkerReply::Output {
        contents,
        is_error: is_error || suffix_is_error,
        error_code: suffix_error_code.or(error_code),
        prompt: suffix_prompt.or(prompt),
        prompt_variants: suffix_prompt_variants.or(prompt_variants),
    }
}

fn prefixed_worker_reply_item_count(prefix: &WorkerReply) -> usize {
    let WorkerReply::Output {
        contents, prompt, ..
    } = prefix;
    let Some(prompt_text) = prompt.as_deref() else {
        return contents.len();
    };
    if prompt_text.is_empty() {
        return contents.len();
    }
    let Some(idx) = contents
        .iter()
        .rposition(|content| matches!(content, WorkerContent::ContentText { .. }))
    else {
        return contents.len();
    };
    let WorkerContent::ContentText { text, .. } = &contents[idx] else {
        return contents.len();
    };
    if matches!(text.strip_suffix(prompt_text), Some("")) {
        contents.len().saturating_sub(1)
    } else {
        contents.len()
    }
}

fn mark_busy_follow_up_reply(reply: &mut WorkerReply) {
    let WorkerReply::Output {
        contents,
        is_error,
        error_code,
        ..
    } = reply;
    contents.push(WorkerContent::server_stderr(
        "[repl] input discarded while worker busy",
    ));
    *is_error = true;
    if error_code.is_none() {
        *error_code = Some(WorkerErrorCode::Busy);
    }
}

struct WorkerProcess {
    child: Child,
    stdin_tx: mpsc::Sender<StdinCommand>,
    session_tmpdir: Option<PathBuf>,
    ipc: IpcHandle,
    stdout_reader: Option<OutputReader>,
    stderr_reader: Option<OutputReader>,
    expected_exit: bool,
    exit_status: Option<std::process::ExitStatus>,
    #[cfg(target_family = "unix")]
    guardrail_stop: Arc<AtomicBool>,
    #[cfg(target_family = "unix")]
    guardrail_thread: Option<std::thread::JoinHandle<()>>,
    #[cfg(target_family = "unix")]
    guardrail_thread_handle: Option<std::thread::Thread>,
    #[cfg(target_os = "macos")]
    denial_logger: Option<crate::sandbox::DenialLogger>,
}

enum StdinCommand {
    Write {
        payload: Vec<u8>,
        reply: mpsc::Sender<Result<(), WorkerError>>,
    },
    Close {
        reply: mpsc::Sender<Result<(), WorkerError>>,
    },
}

struct SpawnedWorker {
    child: Child,
    stdin_tx: mpsc::Sender<StdinCommand>,
    session_tmpdir: Option<PathBuf>,
    stdout_reader: Option<OutputReader>,
    stderr_reader: Option<OutputReader>,
    #[cfg(target_os = "macos")]
    denial_logger: Option<crate::sandbox::DenialLogger>,
}

struct WorkerSpawnContext {
    oversized_output: OversizedOutputMode,
    pending_output_tape: PendingOutputTape,
    output_timeline: OutputTimeline,
    guardrail: GuardrailShared,
    #[cfg(target_os = "windows")]
    prepared_windows_launch: Option<crate::windows_sandbox::PreparedSandboxLaunch>,
}

struct OutputReader {
    handle: std::thread::JoinHandle<()>,
    done_rx: mpsc::Receiver<()>,
    stop_requested: Arc<AtomicBool>,
    #[cfg(target_family = "unix")]
    wake_writer: std::io::PipeWriter,
}

impl OutputReader {
    fn stop_and_join(mut self, panic_message: &'static str) -> Result<(), WorkerError> {
        if matches!(
            self.done_rx.recv_timeout(OUTPUT_READER_QUIESCE_GRACE),
            Err(mpsc::RecvTimeoutError::Timeout)
        ) {
            self.request_stop();
            let _ = self.done_rx.recv();
        }
        self.handle
            .join()
            .map_err(|_| WorkerError::Protocol(panic_message.to_string()))
    }

    fn request_stop(&mut self) {
        self.stop_requested.store(true, Ordering::Relaxed);
        #[cfg(target_family = "unix")]
        {
            let _ = self.wake_writer.write_all(&[0]);
            let _ = self.wake_writer.flush();
        }
    }
}

impl WorkerProcess {
    #[cfg(target_family = "unix")]
    const PYTHON_PROGRAM: &'static str = "python3";
    #[cfg(target_family = "unix")]
    const PYTHON_PROGRAM_FALLBACK: &'static str = "python";
    #[cfg(target_family = "unix")]
    const PYTHON_STARTUP_SNIPPET: &'static str = include_str!("../python/driver.py");

    #[cfg(target_family = "unix")]
    fn resolve_python_program() -> PathBuf {
        // Prefer a local `.venv` (common with uv and other tooling) so the Python backend runs in
        // the project environment without requiring any explicit configuration.
        //
        // Search the current working directory and its parents, stopping at `$HOME` (inclusive)
        // when available, otherwise at the filesystem root.
        fn find_dot_venv_python(start: &Path) -> Option<PathBuf> {
            let home = std::env::var_os("HOME").map(PathBuf::from);
            let stop_at_home = home
                .as_ref()
                .filter(|home| start.starts_with(home))
                .cloned();

            let mut dir = start.to_path_buf();
            loop {
                for candidate in [
                    dir.join(".venv").join("bin").join("python"),
                    dir.join(".venv").join("bin").join("python3"),
                ] {
                    if candidate.is_file() {
                        return Some(candidate);
                    }
                }

                if let Some(stop) = stop_at_home.as_ref()
                    && &dir == stop
                {
                    break;
                }

                let Some(parent) = dir.parent() else {
                    break;
                };
                if parent == dir {
                    break;
                }
                dir = parent.to_path_buf();
            }
            None
        }

        fn find_program_on_path(name: &str) -> Option<PathBuf> {
            let path = std::env::var_os("PATH")?;
            for dir in std::env::split_paths(&path) {
                let candidate = dir.join(name);
                if !candidate.is_file() {
                    continue;
                }

                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Ok(meta) = std::fs::metadata(&candidate)
                        && meta.permissions().mode() & 0o111 != 0
                    {
                        return Some(candidate);
                    }
                }

                #[cfg(not(unix))]
                {
                    return Some(candidate);
                }
            }
            None
        }

        std::env::current_dir()
            .ok()
            .and_then(|cwd| find_dot_venv_python(&cwd))
            .or_else(|| find_program_on_path(Self::PYTHON_PROGRAM))
            .or_else(|| find_program_on_path(Self::PYTHON_PROGRAM_FALLBACK))
            .unwrap_or_else(|| PathBuf::from(Self::PYTHON_PROGRAM))
    }

    fn spawn(
        backend: Backend,
        exe_path: &Path,
        sandbox_state: &SandboxState,
        context: WorkerSpawnContext,
    ) -> Result<Self, WorkerError> {
        let WorkerSpawnContext {
            oversized_output,
            pending_output_tape,
            output_timeline,
            guardrail,
            #[cfg(target_os = "windows")]
            prepared_windows_launch,
        } = context;

        #[cfg(not(target_family = "unix"))]
        let _ = &guardrail;

        let mut ipc_server = IpcServer::bind().map_err(WorkerError::Io)?;
        let live_output = LiveOutputCapture::new(
            oversized_output,
            pending_output_tape.clone(),
            output_timeline.clone(),
        );
        let SpawnedWorker {
            child,
            stdin_tx,
            session_tmpdir,
            stdout_reader,
            stderr_reader,
            #[cfg(target_os = "macos")]
            denial_logger,
        } = match backend {
            Backend::R => Self::spawn_r_worker(
                exe_path,
                sandbox_state,
                live_output.clone(),
                &mut ipc_server,
                #[cfg(target_os = "windows")]
                prepared_windows_launch.as_ref(),
            )?,
            Backend::Python => {
                Self::spawn_python_worker(sandbox_state, live_output.clone(), &mut ipc_server)?
            }
        };
        #[allow(unused_mut)]
        let mut child = child;

        let ipc = IpcHandle::new();
        #[cfg(any(target_family = "unix", target_family = "windows"))]
        {
            let image_capture = live_output.clone();
            let sideband_capture = live_output.clone();
            let handlers = IpcHandlers {
                on_plot_image: Some(Arc::new(move |image: IpcPlotImage| {
                    image_capture.append_image(image);
                })),
                on_readline_start: Some(Arc::new(move |prompt: String| {
                    sideband_capture.append_sideband(PendingSidebandKind::ReadlineStart { prompt });
                })),
                on_readline_result: {
                    let sideband_capture = live_output.clone();
                    Some(Arc::new(move |event: IpcEchoEvent| {
                        sideband_capture.append_sideband(PendingSidebandKind::ReadlineResult {
                            prompt: event.prompt,
                            line: event.line,
                        });
                    }))
                },
                on_request_end: {
                    let sideband_capture = live_output.clone();
                    Some(Arc::new(move || {
                        sideband_capture.append_sideband(PendingSidebandKind::RequestEnd);
                    }))
                },
                on_session_end: {
                    let sideband_capture = live_output.clone();
                    Some(Arc::new(move || {
                        sideband_capture.append_sideband(PendingSidebandKind::SessionEnd);
                    }))
                },
            };
            #[cfg(target_family = "unix")]
            ipc_server
                .connect(ipc.clone(), handlers)
                .map_err(WorkerError::Io)?;
            #[cfg(target_family = "windows")]
            handle_windows_ipc_connect_result(
                ipc_server.connect(
                    ipc.clone(),
                    handlers,
                    &mut child,
                    WINDOWS_IPC_CONNECT_MAX_WAIT,
                ),
                &mut child,
            )?;
        }

        #[cfg(target_family = "unix")]
        let (guardrail_stop, guardrail_thread, guardrail_thread_handle) =
            start_memory_guardrail(child.id(), guardrail.clone());

        Ok(Self {
            child,
            stdin_tx,
            session_tmpdir,
            ipc,
            stdout_reader,
            stderr_reader,
            expected_exit: false,
            exit_status: None,
            #[cfg(target_family = "unix")]
            guardrail_stop,
            #[cfg(target_family = "unix")]
            guardrail_thread: Some(guardrail_thread),
            #[cfg(target_family = "unix")]
            guardrail_thread_handle: Some(guardrail_thread_handle),
            #[cfg(target_os = "macos")]
            denial_logger,
        })
    }

    fn spawn_r_worker(
        exe_path: &Path,
        sandbox_state: &SandboxState,
        live_output: LiveOutputCapture,
        ipc_server: &mut IpcServer,
        #[cfg(target_os = "windows")] prepared_windows_launch: Option<
            &crate::windows_sandbox::PreparedSandboxLaunch,
        >,
    ) -> Result<SpawnedWorker, WorkerError> {
        let prepared =
            prepare_worker_command(exe_path, vec![WORKER_MODE_ARG.to_string()], sandbox_state)
                .map_err(|err| WorkerError::Sandbox(err.to_string()))?;
        #[cfg(target_os = "windows")]
        let mut prepared = prepared;
        #[cfg(target_os = "windows")]
        if let Some(prepared_windows_launch) = prepared_windows_launch {
            crate::sandbox::append_windows_prepared_capability_sid(
                &mut prepared.args,
                prepared_windows_launch.capability_sid(),
            )
            .map_err(WorkerError::Sandbox)?;
        }
        let session_tmpdir = prepared
            .env
            .get(R_SESSION_TMPDIR_ENV)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);

        let mut command = Command::new(&prepared.program);
        if let Some(arg0) = &prepared.arg0 {
            set_command_arg0(&mut command, arg0);
        }
        command.args(&prepared.args);
        command.envs(prepared.env.iter());
        #[cfg(target_family = "unix")]
        let client_fds = ipc_server.take_child_fds().ok_or_else(|| {
            WorkerError::Protocol("IPC pipe setup failed; no client fds available".to_string())
        })?;
        #[cfg(target_family = "unix")]
        {
            command.env(IPC_READ_FD_ENV, client_fds.read_fd.to_string());
            command.env(IPC_WRITE_FD_ENV, client_fds.write_fd.to_string());
        }
        #[cfg(target_family = "windows")]
        let (pipe_to_worker, pipe_from_worker) = ipc_server.take_pipe_names().ok_or_else(|| {
            WorkerError::Protocol("IPC pipe setup failed; missing pipe names".to_string())
        })?;
        #[cfg(target_family = "windows")]
        {
            command.env(IPC_PIPE_TO_WORKER_ENV, pipe_to_worker);
            command.env(IPC_PIPE_FROM_WORKER_ENV, pipe_from_worker);
            command.creation_flags(CREATE_NEW_PROCESS_GROUP);
        }
        apply_debug_startup_env(&mut command, session_tmpdir.as_ref());
        #[cfg(target_family = "unix")]
        unsafe {
            command.pre_exec(|| {
                libc::setpgid(0, 0);
                Ok(())
            });
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        #[cfg(target_family = "unix")]
        {
            unsafe {
                libc::close(client_fds.read_fd);
                libc::close(client_fds.write_fd);
            }
        }
        if let Some(status) = child.try_wait()? {
            maybe_report_sandbox_exec_failure(&prepared.program, status)?;
            return Err(WorkerError::Protocol(format!(
                "worker process exited immediately with status {status}"
            )));
        }

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| WorkerError::Protocol("worker stdin unavailable".to_string()))?;
        let stdin_tx = spawn_stdin_writer(stdin);
        let stdout_reader =
            spawn_output_reader(child.stdout.take(), TextStream::Stdout, live_output.clone())?;
        let stderr_reader =
            spawn_output_reader(child.stderr.take(), TextStream::Stderr, live_output.clone())?;

        #[cfg(target_os = "macos")]
        let mut denial_logger = prepared.denial_logger;
        #[cfg(target_os = "macos")]
        if let Some(logger) = denial_logger.as_mut() {
            logger.on_child_spawn(&child);
        }

        Ok(SpawnedWorker {
            child,
            stdin_tx,
            session_tmpdir,
            stdout_reader,
            stderr_reader,
            #[cfg(target_os = "macos")]
            denial_logger,
        })
    }

    #[cfg(target_family = "unix")]
    fn python_command_args() -> Vec<String> {
        vec![
            "-i".to_string(),
            "-u".to_string(),
            "-q".to_string(),
            "-c".to_string(),
            Self::PYTHON_STARTUP_SNIPPET.to_string(),
        ]
    }

    fn spawn_python_worker(
        sandbox_state: &SandboxState,
        live_output: LiveOutputCapture,
        ipc_server: &mut IpcServer,
    ) -> Result<SpawnedWorker, WorkerError> {
        #[cfg(not(target_family = "unix"))]
        {
            let _ = sandbox_state;
            let _ = live_output;
            let _ = ipc_server;
            Err(WorkerError::Protocol(
                "python backend requires a unix-style pty".to_string(),
            ))
        }
        #[cfg(target_family = "unix")]
        {
            let python_program = Self::resolve_python_program();
            let prepared =
                prepare_worker_command(&python_program, Self::python_command_args(), sandbox_state)
                    .map_err(|err| WorkerError::Sandbox(err.to_string()))?;
            let session_tmpdir = prepared
                .env
                .get(R_SESSION_TMPDIR_ENV)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from);

            let mut command = Command::new(&prepared.program);
            if let Some(arg0) = &prepared.arg0 {
                set_command_arg0(&mut command, arg0);
            }
            command.args(&prepared.args);
            command.envs(prepared.env.iter());
            // Python 3.13 defaults to the new _pyrepl UI, which emits terminal control sequences
            // and bypasses readline hooks we use for prompt/request accounting.
            command.env("PYTHON_BASIC_REPL", "1");

            let client_fds = ipc_server.take_child_fds().ok_or_else(|| {
                WorkerError::Protocol("IPC pipe setup failed; no client fds available".to_string())
            })?;
            command.env(IPC_READ_FD_ENV, client_fds.read_fd.to_string());
            command.env(IPC_WRITE_FD_ENV, client_fds.write_fd.to_string());

            let (master, slave) = open_pty_pair()?;
            let slave_fd = slave.as_raw_fd();
            let stdin = slave.try_clone()?;
            let stdout = slave.try_clone()?;
            let stderr = slave;
            command
                .stdin(Stdio::from(stdin))
                .stdout(Stdio::from(stdout))
                .stderr(Stdio::from(stderr));

            unsafe {
                command.pre_exec(move || {
                    if libc::setsid() < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::ioctl(slave_fd, libc::TIOCSCTTY as libc::c_ulong, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }

            let mut child = command.spawn()?;
            unsafe {
                libc::close(client_fds.read_fd);
                libc::close(client_fds.write_fd);
            }
            if let Some(status) = child.try_wait()? {
                maybe_report_sandbox_exec_failure(&prepared.program, status)?;
                return Err(WorkerError::Protocol(format!(
                    "worker process exited immediately with status {status}"
                )));
            }

            let master_reader = master.try_clone()?;
            let stdin_tx = spawn_stdin_writer(master);
            // Python runs under a PTY so stdout/stderr are merged.
            let stdout_reader =
                spawn_output_reader(Some(master_reader), TextStream::Stdout, live_output.clone())?;

            #[cfg(target_os = "macos")]
            let mut denial_logger = prepared.denial_logger;
            #[cfg(target_os = "macos")]
            if let Some(logger) = denial_logger.as_mut() {
                logger.on_child_spawn(&child);
            }

            Ok(SpawnedWorker {
                child,
                stdin_tx,
                session_tmpdir,
                stdout_reader,
                stderr_reader: None,
                #[cfg(target_os = "macos")]
                denial_logger,
            })
        }
    }
    fn write_stdin_payload(
        &mut self,
        payload: Vec<u8>,
        timeout: Duration,
    ) -> Result<(), WorkerError> {
        self.send_stdin_payload(Some(payload), timeout)
    }

    fn close_stdin(&mut self, timeout: Duration) -> Result<(), WorkerError> {
        self.send_stdin_payload(None, timeout)
    }

    fn send_stdin_payload(
        &mut self,
        payload: Option<Vec<u8>>,
        timeout: Duration,
    ) -> Result<(), WorkerError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        let command = match payload {
            Some(payload) => StdinCommand::Write {
                payload,
                reply: reply_tx,
            },
            None => StdinCommand::Close { reply: reply_tx },
        };
        self.stdin_tx
            .send(command)
            .map_err(|_| WorkerError::Protocol("worker stdin unavailable".to_string()))?;
        if timeout.is_zero() {
            return Err(WorkerError::Timeout(timeout));
        }
        match reply_rx.recv_timeout(timeout) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(err)) => Err(err),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(WorkerError::Timeout(timeout)),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(WorkerError::Protocol(
                "worker stdin thread exited unexpectedly".to_string(),
            )),
        }
    }

    fn send_interrupt(&mut self) -> Result<(), WorkerError> {
        #[cfg(target_family = "unix")]
        {
            self.send_signal(libc::SIGINT)
        }
        #[cfg(not(target_family = "unix"))]
        {
            Ok(())
        }
    }

    #[cfg(target_family = "windows")]
    fn send_r_interrupt(&mut self) -> Result<(), WorkerError> {
        if self.child.try_wait()?.is_some() {
            return Ok(());
        }
        let ok = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, self.child.id()) };
        if ok != 0 {
            return Ok(());
        }

        match self.child.try_wait()? {
            Some(_) => Ok(()),
            None => Err(WorkerError::Io(std::io::Error::last_os_error())),
        }
    }

    #[cfg(not(target_family = "windows"))]
    fn send_r_interrupt(&mut self) -> Result<(), WorkerError> {
        self.send_interrupt()
    }

    fn send_sigterm(&mut self) -> Result<(), WorkerError> {
        #[cfg(target_family = "unix")]
        {
            self.send_signal_and_descendants(libc::SIGTERM)
        }
        #[cfg(not(target_family = "unix"))]
        {
            request_soft_termination(&mut self.child)
        }
    }

    fn send_sigkill(&mut self) -> Result<(), WorkerError> {
        #[cfg(target_family = "unix")]
        {
            self.send_signal_and_descendants(libc::SIGKILL)
        }
        #[cfg(not(target_family = "unix"))]
        {
            self.child.kill()?;
            Ok(())
        }
    }

    #[cfg(target_family = "unix")]
    fn send_signal(&self, signal: i32) -> Result<(), WorkerError> {
        let pid = self.child.id() as i32;
        let result = raw_unix_kill(-pid, signal);
        if result == 0 {
            Ok(())
        } else {
            let err = std::io::Error::last_os_error();
            // If the process (group) is already gone, we're done.
            if err.kind() == std::io::ErrorKind::NotFound {
                return Ok(());
            }
            Err(WorkerError::Io(err))
        }
    }

    #[cfg(target_family = "unix")]
    fn send_signal_and_descendants(&self, signal: i32) -> Result<(), WorkerError> {
        let root = Pid::from_u32(self.child.id());
        let mut system = System::new();
        system.refresh_processes(ProcessesToUpdate::All, true);
        let descendants = collect_process_tree_pids(&system, root);
        let result = self.send_signal(signal);
        for pid in descendants {
            let _ = raw_unix_kill(pid.as_u32() as i32, signal);
        }
        result
    }

    #[cfg(target_family = "unix")]
    fn send_signal_descendants_only(&self, signal: i32) {
        let root = Pid::from_u32(self.child.id());
        let mut system = System::new();
        system.refresh_processes(ProcessesToUpdate::All, true);
        for pid in collect_process_tree_pids(&system, root) {
            if pid == root {
                continue;
            }
            let _ = raw_unix_kill(pid.as_u32() as i32, signal);
        }
    }

    fn note_expected_exit(&mut self) {
        self.expected_exit = true;
    }

    fn exit_status_message(&mut self) -> Result<Option<String>, WorkerError> {
        if self.exit_status.is_none()
            && let Some(status) = self.child.try_wait()?
        {
            self.exit_status = Some(status);
        }
        let Some(status) = self.exit_status.as_ref() else {
            return Ok(None);
        };
        if status.success() {
            return Ok(None);
        }
        Ok(Some(format_exit_status_message(status)))
    }

    fn is_running(&mut self) -> Result<bool, WorkerError> {
        if let Some(status) = self.child.try_wait()? {
            self.exit_status = Some(status);
            let should_log = !status.success() && !self.expected_exit;
            if should_log {
                #[cfg(target_family = "unix")]
                if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&status) {
                    eprintln!("worker exited with signal {signal}");
                } else {
                    eprintln!("worker exited with status {status}");
                }
                #[cfg(not(target_family = "unix"))]
                eprintln!("worker exited with status {status}");
            }
            return Ok(false);
        }
        Ok(true)
    }

    fn shutdown_graceful(mut self, timeout: Duration) -> Result<(), WorkerError> {
        if let Some(ipc) = self.ipc.get() {
            let _ = ipc.send(ServerToWorkerIpcMessage::SessionEnd);
        }
        let _ = self.close_stdin(Duration::from_millis(200));

        let start = std::time::Instant::now();
        let timeout_deadline = start + timeout;
        let term_deadline = start + shutdown_term_delay(timeout);

        if !timeout.is_zero() {
            // TODO: Replace these try_wait() polling loops with a dedicated waiter thread so
            // teardown can block on a completion signal, then escalate on timeout without spin
            // sleeps.
            loop {
                if let Some(status) = self.child.try_wait()? {
                    self.exit_status = Some(status);
                    break;
                }
                let now = std::time::Instant::now();
                if now >= term_deadline || now >= timeout_deadline {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        }

        if self.child.try_wait()?.is_none() {
            let _ = self.send_sigterm();
            let term_deadline = std::cmp::min(
                timeout_deadline,
                std::time::Instant::now() + Duration::from_secs(2),
            );
            loop {
                if let Some(status) = self.child.try_wait()? {
                    self.exit_status = Some(status);
                    break;
                }
                if std::time::Instant::now() >= term_deadline {
                    let _ = self.send_sigkill();
                    self.exit_status = Some(self.child.wait()?);
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        }

        self.finalize_terminated_process()
    }

    fn kill(mut self) -> Result<(), WorkerError> {
        let _ = self.send_sigkill();
        self.exit_status = Some(self.child.wait()?);
        self.finalize_terminated_process()
    }

    fn finish_exited(mut self) -> Result<(), WorkerError> {
        if self.exit_status.is_none() {
            self.exit_status = Some(self.child.wait()?);
        }
        self.finalize_terminated_process()
    }

    fn finalize_terminated_process(&mut self) -> Result<(), WorkerError> {
        #[cfg(target_family = "unix")]
        {
            // Once the root worker is gone, kill any remaining session peers before waiting on
            // stdio or IPC readers they may still be holding open.
            if self.exit_status.is_some() {
                self.send_signal_descendants_only(libc::SIGKILL);
            } else {
                let _ = self.send_sigkill();
            }
            // TODO: Track descendants or use stronger OS-level containment so children that have
            // escaped the worker process group are still killable after the root exits.
        }
        self.quiesce_output_producers()?;
        self.cleanup_session_tmpdir();
        self.report_denials();
        Ok(())
    }

    fn quiesce_output_producers(&mut self) -> Result<(), WorkerError> {
        // Keep teardown bounded even if a detached descendant still holds stdio open. A more
        // robust long-term design would pair this with session-scoped output rings or stronger
        // OS-level containment so stale descendants cannot target a future session at all.
        // IPC is stricter than stdout/stderr by contract: only the main worker may own the
        // sideband fds. Backend startup strips the bootstrap env vars, marks the fds
        // close-on-exec, and closes them again in forked children, so EOF should track the root
        // worker lifetime.
        if let Some(reader) = self.stdout_reader.take() {
            reader.stop_and_join("worker stdout reader thread panicked")?;
        }
        if let Some(reader) = self.stderr_reader.take() {
            reader.stop_and_join("worker stderr reader thread panicked")?;
        }
        if let Some(ipc) = self.ipc.get() {
            ipc.join_reader_thread().map_err(WorkerError::Io)?;
        }
        Ok(())
    }

    fn cleanup_session_tmpdir(&self) {
        let Some(path) = self.session_tmpdir.as_ref() else {
            return;
        };
        if !path.is_absolute() || path.as_path() == std::path::Path::new("/") {
            return;
        }
        cleanup_worker_session_tmpdir(
            path,
            crate::debug_logs::log_path(crate::diagnostics::WORKER_STARTUP_LOG_FILE_NAME),
        );
    }

    #[cfg(target_os = "macos")]
    fn report_denials(&mut self) {
        let Some(logger) = self.denial_logger.take() else {
            return;
        };
        let denials = logger.finish();
        if denials.is_empty() {
            return;
        }
        eprintln!("\n=== Sandbox denials ===");
        for crate::sandbox::SandboxDenial { name, capability } in denials {
            eprintln!("({name}) {capability}");
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn report_denials(&mut self) {}
}

fn persist_worker_startup_log(session_tmpdir: &Path, destination: Option<PathBuf>) {
    let Some(destination) = destination else {
        return;
    };
    let source = session_tmpdir.join(crate::diagnostics::WORKER_STARTUP_LOG_FILE_NAME);
    if !source.is_file() || source == destination {
        return;
    }
    if let Err(err) = std::fs::copy(&source, &destination) {
        eprintln!(
            "Failed to persist worker startup log to {}: {err}",
            destination.display()
        );
    }
}

fn cleanup_worker_session_tmpdir(session_tmpdir: &Path, worker_log_destination: Option<PathBuf>) {
    persist_worker_startup_log(session_tmpdir, worker_log_destination);
    if std::env::var_os("MCP_REPL_KEEP_SESSION_TMPDIR").is_some() {
        return;
    }
    if let Err(err) = std::fs::remove_dir_all(session_tmpdir)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!("Failed to remove worker session temp dir: {err}");
    }
}

impl Drop for WorkerProcess {
    fn drop(&mut self) {
        #[cfg(target_family = "unix")]
        {
            self.guardrail_stop.store(true, Ordering::Relaxed);
            if let Some(thread) = self.guardrail_thread_handle.as_ref() {
                thread.unpark();
            }
            if let Some(handle) = self.guardrail_thread.take() {
                let _ = handle.join();
            }
        }
    }
}

#[cfg(target_family = "unix")]
fn start_memory_guardrail(
    root_pid: u32,
    guardrail: GuardrailShared,
) -> (
    Arc<AtomicBool>,
    std::thread::JoinHandle<()>,
    std::thread::Thread,
) {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let handle = std::thread::spawn(move || {
        let root = Pid::from_u32(root_pid);
        let mut system = System::new();
        let mut last_check = std::time::Instant::now();
        loop {
            if stop_thread.load(Ordering::Relaxed) {
                return;
            }
            let now = std::time::Instant::now();
            let busy = guardrail.busy.load(Ordering::Relaxed);
            let interval = if busy {
                WORKER_MEM_GUARDRAIL_ACTIVE_INTERVAL
            } else {
                WORKER_MEM_GUARDRAIL_IDLE_INTERVAL
            };
            if now.duration_since(last_check) < interval {
                // Use park_timeout + unpark so shutdown doesn't block for up to 1s waiting
                // for this thread to wake from sleep.
                let remaining = interval.saturating_sub(now.duration_since(last_check));
                std::thread::park_timeout(remaining.min(Duration::from_secs(60)));
                continue;
            }
            last_check = now;

            system.refresh_memory();
            system.refresh_processes(ProcessesToUpdate::All, true);

            let total_kb = system.total_memory();
            let limit_kb = (total_kb as f64 * WORKER_MEM_GUARDRAIL_RATIO) as u64;
            let (used_kb, pids) = process_tree_memory_kb(&system, root);
            if used_kb == 0 || total_kb == 0 {
                continue;
            }
            if used_kb < limit_kb {
                continue;
            }

            let used_mb = used_kb / 1024;
            let limit_mb = limit_kb / 1024;
            let total_mb = total_kb / 1024;
            let mut message = format!(
                "[repl] worker killed by memory guardrail: rss={}MB limit={}MB ({}% of host {}MB)\n",
                used_mb,
                limit_mb,
                (WORKER_MEM_GUARDRAIL_RATIO * 100.0).round() as u64,
                total_mb
            );
            if busy {
                message.push_str("[repl] previous request aborted; retry your last input\n");
            } else {
                message.push_str("[repl] worker was idle; new session started\n");
            }

            {
                let mut slot = guardrail
                    .event
                    .lock()
                    .expect("guardrail event mutex poisoned");
                if slot.is_none() {
                    *slot = Some(GuardrailEvent {
                        message: message.clone(),
                        was_busy: busy,
                    });
                }
            }

            // Best-effort: kill process group and then any discovered descendants.
            let _ = unsafe { libc::kill(-(root_pid as i32), libc::SIGKILL) };
            for pid in pids {
                let _ = unsafe { libc::kill(pid.as_u32() as i32, libc::SIGKILL) };
            }

            return;
        }
    });
    let thread = handle.thread().clone();
    (stop, handle, thread)
}

#[cfg(target_family = "unix")]
fn process_tree_memory_kb(system: &System, root: Pid) -> (u64, Vec<Pid>) {
    let pids = collect_process_tree_pids(system, root);
    let mut total_kb: u64 = 0;
    for pid in &pids {
        if let Some(process) = system.process(*pid) {
            total_kb = total_kb.saturating_add(process.memory());
        }
    }
    (total_kb, pids)
}

#[cfg(target_family = "unix")]
fn collect_process_tree_pids(system: &System, root: Pid) -> Vec<Pid> {
    let mut children: HashMap<Pid, Vec<Pid>> = HashMap::new();
    for (proc_pid, process) in system.processes() {
        if let Some(parent) = process.parent() {
            children.entry(parent).or_default().push(*proc_pid);
        }
    }

    let mut stack = vec![root];
    let mut seen: HashSet<Pid> = HashSet::new();
    while let Some(current) = stack.pop() {
        if !seen.insert(current) {
            continue;
        }
        if let Some(kids) = children.get(&current) {
            for child in kids {
                if !seen.contains(child) {
                    stack.push(*child);
                }
            }
        }
    }

    let mut pids = Vec::new();
    for pid in seen {
        if system.process(pid).is_some() {
            pids.push(pid);
        }
    }
    pids
}

fn apply_debug_startup_env(command: &mut Command, session_tmpdir: Option<&PathBuf>) {
    crate::debug_logs::apply_child_env(command);
    if let Some(tmpdir) = session_tmpdir {
        command.env(
            crate::diagnostics::STARTUP_LOG_PATH_ENV,
            tmpdir.join(crate::diagnostics::WORKER_STARTUP_LOG_FILE_NAME),
        );
    }
}

fn maybe_report_sandbox_exec_failure(
    _program: &Path,
    _status: std::process::ExitStatus,
) -> Result<(), WorkerError> {
    #[cfg(target_os = "macos")]
    {
        let is_sandbox_exec = _program
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == "sandbox-exec");
        if is_sandbox_exec && _status.code() == Some(71) {
            return Err(WorkerError::Sandbox(
                "sandbox-exec failed (Operation not permitted). Start mcp-repl with --sandbox danger-full-access to disable sandboxing."
                    .to_string(),
            ));
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_sandbox_startup_retryable(err: &WorkerError) -> bool {
    match err {
        WorkerError::Protocol(message) => {
            message.contains("ipc disconnected while waiting for backend info")
                || message.contains("worker session ended before backend info")
                || message.contains("worker process exited immediately")
        }
        _ => false,
    }
}

#[cfg(target_family = "unix")]
fn set_cloexec(fd: RawFd, enabled: bool) -> Result<(), WorkerError> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(WorkerError::Io(std::io::Error::last_os_error()));
    }
    let new_flags = if enabled {
        flags | libc::FD_CLOEXEC
    } else {
        flags & !libc::FD_CLOEXEC
    };
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFD, new_flags) };
    if rc < 0 {
        return Err(WorkerError::Io(std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(target_family = "unix")]
fn open_pty_pair() -> Result<(File, File), WorkerError> {
    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if result != 0 {
        return Err(WorkerError::Io(std::io::Error::last_os_error()));
    }
    set_cloexec(master, true)?;
    set_cloexec(slave, false)?;
    let master = unsafe { File::from_raw_fd(master) };
    let slave = unsafe { File::from_raw_fd(slave) };
    Ok((master, slave))
}

#[cfg(target_family = "unix")]
fn spawn_output_reader<R>(
    stream: Option<R>,
    output_stream: TextStream,
    live_output: LiveOutputCapture,
) -> Result<Option<OutputReader>, WorkerError>
where
    R: Read + AsRawFd + Send + 'static,
{
    let Some(mut stream) = stream else {
        return Ok(None);
    };
    let (wake_reader, wake_writer) = std::io::pipe()?;
    let (done_tx, done_rx) = mpsc::channel();
    let stop_requested = Arc::new(AtomicBool::new(false));
    let thread_stop = stop_requested.clone();
    let handle = thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        let stream_fd = stream.as_raw_fd();
        let wake_fd = wake_reader.as_raw_fd();
        loop {
            if thread_stop.load(Ordering::Relaxed) {
                break;
            }
            let mut fds = [
                libc::pollfd {
                    fd: stream_fd,
                    events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                    revents: 0,
                },
                libc::pollfd {
                    fd: wake_fd,
                    events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                    revents: 0,
                },
            ];
            let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
            if ready < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
            if fds[1].revents != 0 {
                break;
            }
            if fds[0].revents == 0 {
                continue;
            }
            match stream.read(&mut buffer) {
                Ok(0) => {
                    break;
                }
                Ok(n) => live_output.append_text(&buffer[..n], output_stream),
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        let _ = done_tx.send(());
    });
    Ok(Some(OutputReader {
        handle,
        done_rx,
        stop_requested,
        wake_writer,
    }))
}

#[cfg(target_family = "windows")]
fn spawn_output_reader<R>(
    stream: Option<R>,
    output_stream: TextStream,
    live_output: LiveOutputCapture,
) -> Result<Option<OutputReader>, WorkerError>
where
    R: Read + AsRawHandle + Send + 'static,
{
    let Some(mut stream) = stream else {
        return Ok(None);
    };
    let (done_tx, done_rx) = mpsc::channel();
    let stop_requested = Arc::new(AtomicBool::new(false));
    let thread_stop = stop_requested.clone();
    let handle = thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        let stream_handle = stream.as_raw_handle();
        loop {
            if thread_stop.load(Ordering::Relaxed) {
                break;
            }
            let mut available = 0u32;
            let peek_ok = unsafe {
                PeekNamedPipe(
                    stream_handle as _,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &mut available,
                    std::ptr::null_mut(),
                )
            };
            if peek_ok == 0 {
                let err = std::io::Error::last_os_error();
                match err.raw_os_error() {
                    Some(code)
                        if code == ERROR_BROKEN_PIPE as i32 || code == ERROR_HANDLE_EOF as i32 =>
                    {
                        break;
                    }
                    _ => break,
                }
            }
            if available == 0 {
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => live_output.append_text(&buffer[..n], output_stream),
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        let _ = done_tx.send(());
    });
    Ok(Some(OutputReader {
        handle,
        done_rx,
        stop_requested,
    }))
}

#[cfg(not(any(target_family = "unix", target_family = "windows")))]
fn spawn_output_reader<R>(
    stream: Option<R>,
    output_stream: TextStream,
    live_output: LiveOutputCapture,
) -> Result<Option<OutputReader>, WorkerError>
where
    R: Read + Send + 'static,
{
    let Some(mut stream) = stream else {
        return Ok(None);
    };
    let stop_requested = Arc::new(AtomicBool::new(false));
    let handle = thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => live_output.append_text(&buffer[..n], output_stream),
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });
    Ok(Some(OutputReader {
        handle,
        stop_requested,
    }))
}

fn spawn_stdin_writer<W>(stdin: W) -> mpsc::Sender<StdinCommand>
where
    W: Write + Send + 'static,
{
    let (tx, rx) = mpsc::channel::<StdinCommand>();
    thread::spawn(move || {
        let mut writer = std::io::BufWriter::new(stdin);
        for command in rx {
            match command {
                StdinCommand::Write { payload, reply } => {
                    let result = writer
                        .write_all(&payload)
                        .and_then(|_| writer.flush())
                        .map_err(WorkerError::Io);
                    let _ = reply.send(result);
                }
                StdinCommand::Close { reply } => {
                    let result = writer.flush().map_err(WorkerError::Io);
                    let _ = reply.send(result);
                    break;
                }
            }
        }
    });
    tx
}

fn duration_to_millis(duration: Duration) -> u64 {
    let millis = duration.as_millis();
    if millis > u64::MAX as u128 {
        u64::MAX
    } else {
        millis as u64
    }
}

fn shutdown_term_delay(timeout: Duration) -> Duration {
    if timeout.is_zero() {
        return Duration::from_secs(0);
    }
    let by_fraction = timeout.mul_f64(0.75);
    let by_remaining = timeout.saturating_sub(Duration::from_secs(10));
    by_fraction.min(by_remaining)
}

#[cfg(target_family = "windows")]
fn handle_windows_ipc_connect_result(
    connect_result: Result<(), std::io::Error>,
    child: &mut Child,
) -> Result<(), WorkerError> {
    match connect_result {
        Ok(()) => Ok(()),
        // The child here is the sandbox wrapper process. Give it a short grace
        // period to unwind ACL changes before forcing termination/reap.
        Err(err) => {
            const WRAPPER_EXIT_GRACE: Duration = Duration::from_secs(2);
            let deadline = std::time::Instant::now() + WRAPPER_EXIT_GRACE;
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) => {
                        if std::time::Instant::now() >= deadline {
                            let _ = child.kill();
                            let _ = child.wait();
                            break;
                        }
                        thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                }
            }
            Err(WorkerError::Io(err))
        }
    }
}

#[cfg(target_family = "windows")]
fn request_soft_termination(_child: &mut Child) -> Result<(), WorkerError> {
    // The Windows child is the sandbox wrapper. Let it exit naturally so it can
    // roll back temporary ACL state before process teardown.
    Ok(())
}

#[cfg(target_family = "unix")]
fn set_command_arg0(command: &mut Command, arg0: &str) {
    command.arg0(arg0);
}

#[cfg(not(target_family = "unix"))]
fn set_command_arg0(_command: &mut Command, _arg0: &str) {}

fn format_exit_status_message(status: &std::process::ExitStatus) -> String {
    #[cfg(target_family = "unix")]
    if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(status) {
        return format!("[repl] worker exited with signal {signal}");
    }
    match status.code() {
        Some(code) => format!("[repl] worker exited with status {code}"),
        None => "[repl] worker exited with unknown status".to_string(),
    }
}

fn worker_error_code(err: &WorkerError) -> Option<WorkerErrorCode> {
    match err {
        WorkerError::Timeout(_) => Some(WorkerErrorCode::Timeout),
        WorkerError::Protocol(_)
        | WorkerError::Io(_)
        | WorkerError::Sandbox(_)
        | WorkerError::Guardrail(_) => Some(WorkerErrorCode::WorkerExecutionFailed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output_capture::{
        OUTPUT_RING_CAPACITY_BYTES, OutputEventKind, OutputRing, ensure_output_ring,
        reset_last_reply_marker_offset, reset_output_ring,
    };
    use crate::sandbox::SandboxPolicy;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    fn cwd_test_mutex() -> &'static Mutex<()> {
        static TEST_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
        TEST_MUTEX.get_or_init(|| Mutex::new(()))
    }

    fn env_test_mutex() -> &'static Mutex<()> {
        static TEST_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
        TEST_MUTEX.get_or_init(|| Mutex::new(()))
    }

    fn output_ring_test_guard() -> MutexGuard<'static, ()> {
        crate::output_capture::output_ring_test_mutex()
            .lock()
            .expect("output ring test lock")
    }

    fn echo_event(prompt: &str, line: &str) -> IpcEchoEvent {
        IpcEchoEvent {
            prompt: prompt.to_string(),
            line: line.to_string(),
        }
    }

    fn contents_text(contents: &[WorkerContent]) -> String {
        contents
            .iter()
            .filter_map(|content| match content {
                WorkerContent::ContentText { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    #[cfg(target_family = "unix")]
    fn sleeping_test_child() -> Child {
        Command::new("sh")
            .args(["-c", "sleep 30"])
            .spawn()
            .expect("spawn sleeping test child")
    }

    #[cfg(target_family = "windows")]
    fn sleeping_test_child() -> Child {
        Command::new("powershell.exe")
            .args(["-NoProfile", "-Command", "Start-Sleep -Seconds 30"])
            .spawn()
            .expect("spawn sleeping test child")
    }

    #[cfg(target_family = "unix")]
    fn successful_test_child() -> Child {
        Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn exiting test child")
    }

    #[cfg(target_family = "unix")]
    fn failing_test_status() -> std::process::ExitStatus {
        Command::new("sh")
            .args(["-c", "exit 7"])
            .status()
            .expect("collect failing exit status")
    }

    #[cfg(target_family = "unix")]
    fn test_worker_process(child: Child) -> WorkerProcess {
        let (stdin_tx, _stdin_rx) = mpsc::channel();
        WorkerProcess {
            child,
            stdin_tx,
            session_tmpdir: None,
            ipc: IpcHandle::new(),
            stdout_reader: None,
            stderr_reader: None,
            expected_exit: false,
            exit_status: None,
            guardrail_stop: Arc::new(AtomicBool::new(false)),
            guardrail_thread: None,
            guardrail_thread_handle: None,
            #[cfg(target_os = "macos")]
            denial_logger: None,
        }
    }

    #[cfg(target_family = "windows")]
    fn test_worker_process(child: Child) -> WorkerProcess {
        let (stdin_tx, _stdin_rx) = mpsc::channel();
        WorkerProcess {
            child,
            stdin_tx,
            session_tmpdir: None,
            ipc: IpcHandle::new(),
            stdout_reader: None,
            stderr_reader: None,
            expected_exit: false,
            exit_status: None,
        }
    }

    #[cfg(target_family = "unix")]
    fn capture_recorded_unix_kills<F, R>(f: F) -> (R, Vec<(i32, i32)>)
    where
        F: FnOnce() -> R,
    {
        TEST_UNIX_KILL_RECORDER.with(|recorder| {
            assert!(
                recorder.borrow().is_none(),
                "did not expect nested unix kill recorder"
            );
            *recorder.borrow_mut() = Some(Vec::new());
        });
        let result = f();
        let kills = TEST_UNIX_KILL_RECORDER
            .with(|recorder| recorder.borrow_mut().take().expect("recorded kills"));
        (result, kills)
    }

    #[test]
    fn trims_echo_prefix_across_text_chunks() {
        let mut contents = vec![
            WorkerContent::stdout("> x <- 1\n"),
            WorkerContent::stdout("> y <- 2\n[1] 2\n"),
        ];
        maybe_trim_echo_prefix(&mut contents, Some("> x <- 1\n> y <- 2\n"), true);
        let text = match &contents[0] {
            WorkerContent::ContentText { text, .. } => text.as_str(),
            _ => "",
        };
        assert_eq!(text, "[1] 2\n");
    }

    #[test]
    fn does_not_trim_on_mismatch() {
        let mut contents = vec![WorkerContent::stdout("> x <- 1\n[1] 1\n")];
        maybe_trim_echo_prefix(&mut contents, Some("> y <- 2\n"), true);
        let text = match &contents[0] {
            WorkerContent::ContentText { text, .. } => text.as_str(),
            _ => "",
        };
        assert_eq!(text, "> x <- 1\n[1] 1\n");
    }

    #[test]
    fn does_not_trim_when_leading_stderr() {
        let mut contents = vec![
            WorkerContent::stderr("stderr: boom\n"),
            WorkerContent::stdout("> x <- 1\n[1] 1\n"),
        ];
        maybe_trim_echo_prefix(&mut contents, Some("> x <- 1\n"), true);
        let text = match &contents[0] {
            WorkerContent::ContentText { text, .. } => text.as_str(),
            _ => "",
        };
        assert_eq!(text, "stderr: boom\n");
    }

    #[test]
    fn trim_echo_then_append_protocol_warnings_drops_echo_only_multiline_input() {
        let warning = "ReadlineResult after RequestEnd".to_string();
        let echo = "> x <- 1\n> y <- 2\n";
        let mut contents = vec![WorkerContent::stdout(echo)];

        trim_echo_then_append_protocol_warnings(
            &mut contents,
            Some(echo),
            false,
            true,
            std::slice::from_ref(&warning),
        );

        assert_eq!(
            contents,
            vec![WorkerContent::server_stderr(format!("[repl] {warning}"))]
        );
    }

    #[test]
    fn trim_echo_then_append_protocol_warnings_keeps_output_before_warning() {
        let warning = "ReadlineResult after RequestEnd".to_string();
        let mut contents = vec![WorkerContent::stdout("> x <- 1\n[1] 1\n")];

        trim_echo_then_append_protocol_warnings(
            &mut contents,
            Some("> x <- 1\n"),
            true,
            true,
            std::slice::from_ref(&warning),
        );

        assert_eq!(
            contents,
            vec![
                WorkerContent::stdout("[1] 1\n"),
                WorkerContent::server_stderr(format!("[repl] {warning}")),
            ]
        );
    }

    #[test]
    fn trim_decision_respects_continuation_prompts() {
        let single = vec![echo_event("> ", "1+1\n")];
        assert!(should_trim_echo_prefix(&single));

        let continuation = vec![echo_event("> ", "1+\n"), echo_event("+ ", "1\n")];
        assert!(should_trim_echo_prefix(&continuation));

        let multi = vec![echo_event("> ", "1+1\n"), echo_event("> ", "2+2\n")];
        assert!(!should_trim_echo_prefix(&multi));

        let browser = vec![echo_event("Browse[1]> ", "n\n")];
        assert!(!should_trim_echo_prefix(&browser));

        let readline = vec![echo_event("FIRST> ", "alpha\n")];
        assert!(!should_trim_echo_prefix(&readline));
    }

    #[test]
    fn collapse_echo_with_attribution_drops_leading_multi_expression_echo_prefix() {
        let range = OutputRange {
            start_offset: 0,
            end_offset: 27,
            bytes: b"> x <- 1\n> y <- 2\n[1] 2\n> ".to_vec(),
            events: Vec::new(),
            text_spans: vec![OutputTextSpan {
                start_byte: 0,
                end_byte: 27,
                is_stderr: false,
                origin: ContentOrigin::Worker,
            }],
        };

        let collapsed = collapse_echo_with_attribution(
            range,
            &[echo_event("> ", "x <- 1\n"), echo_event("> ", "y <- 2\n")],
            0,
            &["> ".to_string()],
            EchoCollapseMode::CollapseForFinalReply,
        );

        assert_eq!(
            String::from_utf8(collapsed.bytes).expect("utf8"),
            "[1] 2\n> "
        );
        assert!(
            collapsed.events.is_empty(),
            "did not expect sideband events"
        );
        assert_eq!(
            collapsed.text_spans.len(),
            1,
            "expected collapsed output to stay in one stdout span"
        );
        assert_eq!(collapsed.text_spans[0].start_byte, 0);
        assert_eq!(collapsed.text_spans[0].end_byte, 8);
        assert!(!collapsed.text_spans[0].is_stderr);
    }

    #[test]
    fn collapse_echo_with_attribution_drops_leading_echo_prefix_without_separator_newline() {
        let range = OutputRange {
            start_offset: 0,
            end_offset: 42,
            bytes: b"> xstderr: Error: object 'x' not found\n> ".to_vec(),
            events: Vec::new(),
            text_spans: vec![OutputTextSpan {
                start_byte: 0,
                end_byte: 42,
                is_stderr: false,
                origin: ContentOrigin::Worker,
            }],
        };

        let collapsed = collapse_echo_with_attribution(
            range,
            &[echo_event("> ", "x\n")],
            0,
            &["> ".to_string()],
            EchoCollapseMode::CollapseForFinalReply,
        );

        assert_eq!(
            String::from_utf8(collapsed.bytes).expect("utf8"),
            "stderr: Error: object 'x' not found\n> "
        );
        assert!(
            collapsed.events.is_empty(),
            "did not expect sideband events"
        );
        assert_eq!(
            collapsed.text_spans.len(),
            1,
            "expected collapsed output to stay in one stdout span"
        );
        assert_eq!(collapsed.text_spans[0].start_byte, 0);
        assert_eq!(collapsed.text_spans[0].end_byte, 38);
        assert!(!collapsed.text_spans[0].is_stderr);
    }

    #[test]
    fn trim_matching_echo_event_suffix_from_contents_trims_late_top_level_echo() {
        let mut contents = vec![WorkerContent::worker_stdout(
            "> cat(\"TAIL_ONLY\\n\")\nTAIL_ONLY\n> ",
        )];

        let trimmed = trim_matching_echo_event_suffix_from_contents(
            &mut contents,
            &[
                echo_event("> ", "cat(\"HEAD_ONLY\\n\")\n"),
                echo_event("> ", "flush.console()\n"),
                echo_event("> ", "cat(\"TAIL_ONLY\\n\")\n"),
            ],
        );

        assert!(trimmed, "expected late top-level echo to be trimmed");
        assert_eq!(contents_text(&contents), "TAIL_ONLY\n> ");
    }

    #[test]
    fn trim_matching_echo_event_suffix_from_contents_trims_python_multiline_crlf_echo() {
        let mut contents = vec![WorkerContent::worker_stdout(
            ">>> def f():\r\n...     return 3\r\n... \r\n>>> f()\r\n3\r\n>>> ... ",
        )];

        let trimmed = trim_matching_echo_event_suffix_from_contents(
            &mut contents,
            &[
                echo_event(">>> ", "def f():\n"),
                echo_event("... ", "    return 3\n"),
                echo_event("... ", "\n"),
                echo_event(">>> ", "f()\n"),
            ],
        );

        assert!(trimmed, "expected multiline Python echo to be trimmed");
        assert_eq!(contents_text(&contents), "3\r\n>>> ... ");
    }

    #[test]
    fn trim_matching_echo_event_suffix_from_contents_keeps_unmatched_prompt_tail() {
        let mut contents = vec![WorkerContent::worker_stdout("FIRST> alpha\nSECOND> ")];

        let trimmed = trim_matching_echo_event_suffix_from_contents(
            &mut contents,
            &[
                echo_event("FIRST> ", "alpha\n"),
                echo_event("SECOND> ", "beta\n"),
            ],
        );

        assert!(
            !trimmed,
            "did not expect partial prompt transcript to be trimmed without an exact match"
        );
        assert_eq!(contents_text(&contents), "FIRST> alpha\nSECOND> ");
    }

    #[test]
    fn control_prefix_accepts_immediate_tail_without_newline() {
        let (action, remaining) =
            split_write_stdin_control_prefix("\u{3}1+1").expect("expected control prefix");
        assert!(matches!(action, WriteStdinControlAction::Interrupt));
        assert_eq!(remaining, "1+1");
    }

    #[test]
    fn control_prefix_strips_single_separator_newline() {
        let (action, remaining) =
            split_write_stdin_control_prefix("\u{4}\nprint(1)").expect("expected control prefix");
        assert!(matches!(action, WriteStdinControlAction::Restart));
        assert_eq!(remaining, "print(1)");
    }

    #[test]
    fn completion_waits_for_request_end_when_primary_prompt_reappears() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");
        driver_on_input_start("value <- readline(prompt = \"> \")", &server);
        let prompt = "> ".to_string();
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: prompt.clone(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineResult {
            prompt: prompt.clone(),
            line: "value <- readline(prompt = \"> \")\n".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: prompt.clone(),
        });

        let result = driver_wait_for_completion(Duration::from_millis(75), server.clone());
        assert!(
            matches!(result, Err(WorkerError::Timeout(_))),
            "expected timeout before request-end when a nested prompt reuses the primary prompt text"
        );

        let _ = worker.send(WorkerToServerIpcMessage::RequestEnd);
        let completion = driver_wait_for_completion(Duration::from_millis(200), server)
            .expect("expected completion after request-end");
        assert_eq!(completion.prompt.as_deref(), Some("> "));
        assert_eq!(completion.echo_events.len(), 1);
        assert_eq!(completion.echo_events[0].prompt, "> ");
        assert_eq!(
            completion.echo_events[0].line,
            "value <- readline(prompt = \"> \")\n"
        );
    }

    #[test]
    fn completion_still_waits_when_only_continuation_prompt_arrives() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");
        driver_on_input_start("1+\n1", &server);
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: "> ".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineResult {
            prompt: "> ".to_string(),
            line: "1+\n".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: "+ ".to_string(),
        });

        let result = driver_wait_for_completion(Duration::from_millis(75), server.clone());
        assert!(
            matches!(result, Err(WorkerError::Timeout(_))),
            "expected timeout before request-end when only a continuation prompt is visible"
        );

        let _ = worker.send(WorkerToServerIpcMessage::RequestEnd);
        let completion = driver_wait_for_completion(Duration::from_millis(200), server)
            .expect("expected completion after request-end");
        assert_eq!(completion.prompt.as_deref(), Some("+ "));
    }

    #[test]
    fn completion_settle_waits_for_late_echo_events() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");
        driver_on_input_start("1+\n1", &server);
        let prompt = "> ".to_string();
        let delayed_worker = worker.clone();

        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: prompt.clone(),
        });

        let late_sender = thread::spawn(move || {
            thread::sleep(Duration::from_millis(1));
            let _ = delayed_worker.send(WorkerToServerIpcMessage::ReadlineResult {
                prompt: "> ".to_string(),
                line: "1+\n".to_string(),
            });
            thread::sleep(Duration::from_millis(21));
            let _ = delayed_worker.send(WorkerToServerIpcMessage::ReadlineResult {
                prompt: "+ ".to_string(),
                line: "1\n".to_string(),
            });
            let _ = delayed_worker.send(WorkerToServerIpcMessage::RequestEnd);
        });

        let completion = driver_wait_for_completion(Duration::from_millis(200), server)
            .expect("expected completion after request-end");
        late_sender.join().expect("late sender should join");

        assert_eq!(completion.prompt.as_deref(), Some("> "));
        assert_eq!(completion.echo_events.len(), 2);
        assert!(completion.protocol_warnings.is_empty());
        assert_eq!(completion.echo_events[0].prompt, "> ");
        assert_eq!(completion.echo_events[0].line, "1+\n");
        assert_eq!(completion.echo_events[1].prompt, "+ ");
        assert_eq!(completion.echo_events[1].line, "1\n");
    }

    #[test]
    fn merge_completion_info_from_ipc_appends_late_echo_events() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: ">>> ".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineResult {
            prompt: ">>> ".to_string(),
            line: "def f():\n".to_string(),
        });

        let mut completion = completion_info_from_ipc(&server, false);
        let delayed_worker = worker.clone();
        let late_sender = thread::spawn(move || {
            thread::sleep(Duration::from_millis(1));
            let _ = delayed_worker.send(WorkerToServerIpcMessage::ReadlineResult {
                prompt: "... ".to_string(),
                line: "    return 3\n".to_string(),
            });
        });
        late_sender.join().expect("late sender should join");
        merge_completion_info_from_ipc(&mut completion, &server, false);

        assert_eq!(completion.prompt.as_deref(), Some(">>> "));
        assert_eq!(completion.echo_events.len(), 2);
        assert_eq!(completion.echo_events[0].prompt, ">>> ");
        assert_eq!(completion.echo_events[0].line, "def f():\n");
        assert_eq!(completion.echo_events[1].prompt, "... ");
        assert_eq!(completion.echo_events[1].line, "    return 3\n");
    }

    #[test]
    fn completion_warns_when_readline_result_arrives_after_request_end() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");
        driver_on_input_start("1+1", &server);

        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: "> ".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::RequestEnd);

        let delayed_worker = worker.clone();
        let late_sender = thread::spawn(move || {
            thread::sleep(Duration::from_millis(1));
            let _ = delayed_worker.send(WorkerToServerIpcMessage::ReadlineResult {
                prompt: "> ".to_string(),
                line: "1+1\n".to_string(),
            });
        });

        let completion = driver_wait_for_completion(Duration::from_millis(200), server)
            .expect("expected completion after request-end");
        late_sender.join().expect("late sender should join");

        assert!(
            completion
                .protocol_warnings
                .iter()
                .any(|warning| warning.contains("ReadlineResult after RequestEnd")),
            "expected protocol warning, got: {:?}",
            completion.protocol_warnings
        );
    }

    #[test]
    fn next_request_result_is_retained_when_prompt_is_already_active() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");

        driver_on_input_start("first()", &server);
        let _ = worker.send(WorkerToServerIpcMessage::RequestEnd);
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: "> ".to_string(),
        });
        let first = driver_wait_for_completion(Duration::from_millis(200), server.clone())
            .expect("expected first completion");
        assert_eq!(first.prompt.as_deref(), Some("> "));

        driver_on_input_start("second()", &server);
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineResult {
            prompt: "> ".to_string(),
            line: "second()\n".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::RequestEnd);

        let second = driver_wait_for_completion(Duration::from_millis(200), server)
            .expect("expected second completion");

        assert!(second.protocol_warnings.is_empty());
        assert_eq!(second.echo_events.len(), 1);
        assert_eq!(second.echo_events[0].prompt, "> ");
        assert_eq!(second.echo_events[0].line, "second()\n");
    }

    #[test]
    fn completion_preserves_echo_events_when_next_prompt_arrives_immediately() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");

        driver_on_input_start("first()", &server);
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: "> ".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineResult {
            prompt: "> ".to_string(),
            line: "first()\n".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::RequestEnd);
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: "> ".to_string(),
        });

        let completion = driver_wait_for_completion(Duration::from_millis(200), server)
            .expect("expected completion after request-end");

        assert_eq!(completion.prompt.as_deref(), Some("> "));
        assert!(completion.protocol_warnings.is_empty());
        assert_eq!(completion.echo_events.len(), 1);
        assert_eq!(completion.echo_events[0].prompt, "> ");
        assert_eq!(completion.echo_events[0].line, "first()\n");
    }

    #[test]
    fn completion_retains_echo_events_when_session_ends_before_request_end() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");
        driver_on_input_start("quit()", &server);

        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: "> ".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineResult {
            prompt: "> ".to_string(),
            line: "quit()\n".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::SessionEnd);

        let completion = driver_wait_for_completion(Duration::from_millis(200), server)
            .expect("expected completion after session end");

        assert!(completion.session_end_seen);
        assert_eq!(completion.echo_events.len(), 1);
        assert_eq!(completion.echo_events[0].prompt, "> ");
        assert_eq!(completion.echo_events[0].line, "quit()\n");
    }

    #[test]
    fn send_worker_request_error_preserves_detached_prefix_count() {
        let mut manager = WorkerManager::new(
            Backend::R,
            SandboxCliPlan::default(),
            crate::oversized_output::OversizedOutputMode::Files,
        )
        .expect("worker manager");
        manager
            .pending_output_tape
            .append_stdout_bytes(b"detached output\n");

        let reply = manager
            .write_stdin(
                "1+1".to_string(),
                Duration::from_millis(50),
                Duration::ZERO,
                None,
                false,
            )
            .expect("reply");

        if let Some(process) = manager.process.take() {
            let _ = process.kill();
        }

        assert!(
            manager.detached_prefix_item_count() >= 1,
            "detached-prefix metadata must survive reset until server-side finalization"
        );
        let WorkerReply::Output { .. } = reply;
    }

    #[test]
    fn busy_follow_up_reply_sets_busy_error_code_when_missing() {
        let mut reply = WorkerReply::Output {
            contents: vec![WorkerContent::worker_stdout("tail\n")],
            is_error: false,
            error_code: None,
            prompt: None,
            prompt_variants: None,
        };

        mark_busy_follow_up_reply(&mut reply);

        let WorkerReply::Output {
            contents,
            is_error,
            error_code,
            ..
        } = reply;
        let text = contents
            .into_iter()
            .filter_map(|content| match content {
                WorkerContent::ContentText { text, .. } => Some(text),
                WorkerContent::ContentImage { .. } => None,
            })
            .collect::<String>();

        assert!(
            is_error,
            "expected busy follow-up replies to be marked as errors"
        );
        assert_eq!(error_code, Some(WorkerErrorCode::Busy));
        assert!(
            text.contains("[repl] input discarded while worker busy"),
            "expected busy follow-up marker, got: {text:?}"
        );
    }

    #[test]
    fn busy_follow_up_reply_preserves_timeout_error_code() {
        let mut reply = WorkerReply::Output {
            contents: vec![WorkerContent::server_stdout("<<repl status: busy>>\n")],
            is_error: false,
            error_code: Some(WorkerErrorCode::Timeout),
            prompt: None,
            prompt_variants: None,
        };

        mark_busy_follow_up_reply(&mut reply);

        let WorkerReply::Output {
            contents,
            is_error,
            error_code,
            ..
        } = reply;
        let text = contents
            .into_iter()
            .filter_map(|content| match content {
                WorkerContent::ContentText { text, .. } => Some(text),
                WorkerContent::ContentImage { .. } => None,
            })
            .collect::<String>();

        assert!(
            is_error,
            "expected timed-out busy follow-up replies to be marked as errors"
        );
        assert_eq!(
            error_code,
            Some(WorkerErrorCode::Timeout),
            "expected timed-out busy follow-up replies to preserve Timeout"
        );
        assert!(
            text.contains("[repl] input discarded while worker busy"),
            "expected busy follow-up marker, got: {text:?}"
        );
    }

    #[test]
    fn session_end_reset_preserves_detached_prefix_count() {
        let mut manager = WorkerManager::new(
            Backend::R,
            SandboxCliPlan::default(),
            crate::oversized_output::OversizedOutputMode::Files,
        )
        .expect("worker manager");
        manager.last_detached_prefix_item_count = 2;
        manager.session_end_seen = true;

        manager.maybe_reset_after_session_end();

        if let Some(process) = manager.process.take() {
            let _ = process.kill();
        }

        assert_eq!(
            manager.detached_prefix_item_count(),
            2,
            "session-end cleanup must preserve detached-prefix metadata until server finalization"
        );
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn finish_exited_does_not_signal_reaped_root_pid() {
        let _guard = env_test_mutex().lock().expect("env mutex");
        let child = successful_test_child();
        let (result, kills) =
            capture_recorded_unix_kills(|| test_worker_process(child).finish_exited());

        assert!(
            result.is_ok(),
            "expected finish_exited to succeed: {result:?}"
        );
        assert!(
            kills.is_empty(),
            "did not expect finish_exited to signal an already reaped root pid, got: {kills:?}"
        );
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn failing_session_end_notice_flushes_partial_stdout_in_files_mode() {
        let _guard = env_test_mutex().lock().expect("env mutex");
        let mut manager = WorkerManager::new(
            Backend::R,
            SandboxCliPlan::default(),
            crate::oversized_output::OversizedOutputMode::Files,
        )
        .expect("worker manager");
        manager.pending_output_tape.append_stdout_bytes(&[0xC3]);

        let mut process = test_worker_process(sleeping_test_child());
        process.exit_status = Some(failing_test_status());
        manager.process = Some(process);

        manager.note_session_end(true);
        let formatted = manager.drain_final_formatted_output();
        let text = contents_text(&formatted.contents);

        if let Some(process) = manager.process.take() {
            let _ = process.kill();
        }

        assert!(
            text.contains("\\xC3"),
            "expected the partial stdout tail to survive the exit-status notice, got: {text:?}"
        );
        assert!(
            text.contains("worker exited with status 7"),
            "expected the exit-status notice to stay visible, got: {text:?}"
        );
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn timed_out_request_end_with_exited_worker_reports_session_end_immediately() {
        let _guard = env_test_mutex().lock().expect("env mutex");
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");
        let mut manager = WorkerManager::new(
            Backend::Python,
            SandboxCliPlan::default(),
            crate::oversized_output::OversizedOutputMode::Files,
        )
        .expect("worker manager");
        let process = test_worker_process(successful_test_child());
        process.ipc.set(server);
        manager.process = Some(process);
        manager.pending_request = true;
        manager.pending_request_started_at = Some(std::time::Instant::now());

        let prompt = ">>> ".to_string();
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: prompt.clone(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineResult {
            prompt,
            line: "quit()\n".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::RequestEnd);
        drop(worker);
        thread::sleep(Duration::from_millis(20));

        manager.resolve_timeout_marker_with_wait(Duration::from_millis(0));
        let formatted = manager.drain_final_formatted_output();
        let text = contents_text(&formatted.contents);

        assert!(
            manager.session_end_seen,
            "expected timed-out completion resolution to notice the exited session"
        );
        assert!(
            manager
                .settled_pending_completion
                .as_ref()
                .is_some_and(|completion| completion.session_end_seen),
            "expected queued completion metadata to be marked as session-ended"
        );
        assert!(
            text.contains("[repl] session ended"),
            "expected timed-out completion resolution to record the session-end notice, got: {text:?}"
        );
        assert!(
            !text.contains(">>> "),
            "did not expect the exited session to keep advertising its prompt, got: {text:?}"
        );
    }

    #[test]
    fn files_prepare_input_context_keeps_echo_when_echo_events_missing() {
        let mut manager = WorkerManager::new(
            Backend::Python,
            SandboxCliPlan::default(),
            crate::oversized_output::OversizedOutputMode::Files,
        )
        .expect("worker manager");
        manager
            .pending_output_tape
            .append_stdout_bytes(b">>> import time; time.sleep(0.2)\nDETACHED_OK\n");
        manager.settled_pending_completion = Some(CompletionInfo {
            prompt: Some(">>> ".to_string()),
            prompt_variants: Some(vec![">>> ".to_string()]),
            echo_events: Vec::new(),
            protocol_warnings: Vec::new(),
            session_end_seen: false,
        });

        let context = manager.prepare_input_context_files();
        let text = contents_text(&context.prefix_contents);

        assert!(
            text.contains("DETACHED_OK\n"),
            "expected the settled files-mode output to survive without trimming, got: {text:?}"
        );
        assert!(
            text.contains(">>> import time; time.sleep(0.2)\n"),
            "expected the Python prompt echo to remain when no echo events were emitted, got: {text:?}"
        );
        assert!(
            manager.settled_pending_completion.is_none(),
            "expected settled completion metadata to be consumed with the detached prefix"
        );
    }

    #[test]
    fn files_prepare_input_context_seals_split_utf8_at_request_boundary() {
        let mut manager = WorkerManager::new(
            Backend::Python,
            SandboxCliPlan::default(),
            crate::oversized_output::OversizedOutputMode::Files,
        )
        .expect("worker manager");
        manager.pending_output_tape.append_stdout_bytes(&[0xC3]);

        let first = manager.prepare_input_context_files();
        assert_eq!(
            contents_text(&first.prefix_contents),
            "\\xC3",
            "expected an accepted request to seal the detached utf-8 lead byte into the prefix"
        );

        manager
            .pending_output_tape
            .append_stdout_bytes(&[0xA9, b'\n']);
        let second = manager.prepare_input_context_files();

        assert_eq!(
            contents_text(&second.prefix_contents),
            "\\xA9\n",
            "expected the next request output to stay split after the detached prefix was sealed"
        );
    }

    #[test]
    fn files_nonfinal_drain_preserves_echo_only_input() {
        let manager = WorkerManager::new(
            Backend::R,
            SandboxCliPlan::default(),
            crate::oversized_output::OversizedOutputMode::Files,
        )
        .expect("worker manager");

        manager
            .pending_output_tape
            .append_stdout_bytes(b"> Sys.sleep(5)\n");
        manager
            .pending_output_tape
            .append_sideband(PendingSidebandKind::ReadlineResult {
                prompt: "> ".to_string(),
                line: "Sys.sleep(5)\n".to_string(),
            });

        let formatted = manager.drain_formatted_output();

        assert_eq!(
            formatted.contents,
            vec![WorkerContent::stdout("> Sys.sleep(5)\n")],
            "expected an in-flight files-mode drain to keep the echoed command visible"
        );
    }

    #[test]
    fn files_nonfinal_drain_drops_leading_repl_echo_after_worker_output() {
        let manager = WorkerManager::new(
            Backend::R,
            SandboxCliPlan::default(),
            crate::oversized_output::OversizedOutputMode::Files,
        )
        .expect("worker manager");

        manager
            .pending_output_tape
            .append_stdout_bytes(b"> Sys.sleep(5)\n");
        manager
            .pending_output_tape
            .append_sideband(PendingSidebandKind::ReadlineResult {
                prompt: "> ".to_string(),
                line: "Sys.sleep(5)\n".to_string(),
            });
        manager.pending_output_tape.append_stdout_bytes(b"start\n");

        let formatted = manager.drain_formatted_output();

        assert_eq!(
            formatted.contents,
            vec![WorkerContent::stdout("start\n")],
            "expected worker output to hide the leading timed-out REPL echo again"
        );
    }

    #[test]
    fn files_prepare_input_context_preserves_unsettled_echo_prefix() {
        let mut manager = WorkerManager::new(
            Backend::R,
            SandboxCliPlan::default(),
            crate::oversized_output::OversizedOutputMode::Files,
        )
        .expect("worker manager");

        manager
            .pending_output_tape
            .append_stdout_bytes(b"> Sys.sleep(5)\n");
        manager
            .pending_output_tape
            .append_sideband(PendingSidebandKind::ReadlineResult {
                prompt: "> ".to_string(),
                line: "Sys.sleep(5)\n".to_string(),
            });

        let context = manager.prepare_input_context_files();

        assert_eq!(
            context.prefix_contents,
            vec![WorkerContent::stdout("> Sys.sleep(5)\n")],
            "expected a sealed files-mode prefix without settled completion metadata to keep echoed input"
        );
    }

    #[test]
    fn pager_prepare_input_context_trims_echo_from_settled_completion() {
        let _guard = output_ring_test_guard();
        let _output_ring = ensure_output_ring(OUTPUT_RING_CAPACITY_BYTES);
        reset_output_ring();
        reset_last_reply_marker_offset();

        let mut manager = WorkerManager::new(
            Backend::R,
            SandboxCliPlan::default(),
            crate::oversized_output::OversizedOutputMode::Pager,
        )
        .expect("worker manager");
        manager.output.start_capture();
        manager.output_timeline.append_text(
            b"> Sys.sleep(0.2); 1+1\n[1] 2\n",
            false,
            ContentOrigin::Worker,
        );
        manager.settled_pending_completion = Some(CompletionInfo {
            prompt: Some("> ".to_string()),
            prompt_variants: Some(vec!["> ".to_string()]),
            echo_events: vec![echo_event("> ", "Sys.sleep(0.2); 1+1\n")],
            protocol_warnings: Vec::new(),
            session_end_seen: false,
        });

        let context = manager.prepare_input_context_pager("3+3", false);
        let text = contents_text(&context.prefix_contents);

        assert!(
            text.contains("[1] 2\n"),
            "expected settled pager output to be preserved, got: {text:?}"
        );
        assert!(
            !text.contains("Sys.sleep(0.2); 1+1"),
            "did not expect settled pager echo to leak into the next input context, got: {text:?}"
        );
        assert!(
            manager.settled_pending_completion.is_none(),
            "expected settled completion metadata to be consumed with the detached prefix"
        );
    }

    #[test]
    fn pager_empty_input_polls_pending_output_before_pager_commands() {
        let _guard = output_ring_test_guard();
        let _output_ring = ensure_output_ring(OUTPUT_RING_CAPACITY_BYTES);
        reset_output_ring();
        reset_last_reply_marker_offset();

        let mut manager = WorkerManager::new(
            Backend::R,
            SandboxCliPlan::default(),
            crate::oversized_output::OversizedOutputMode::Pager,
        )
        .expect("worker manager");
        manager.process = Some(test_worker_process(sleeping_test_child()));

        manager.output.start_capture();
        manager.output_timeline.append_text(
            b"line0001\nline0002\nline0003\nline0004\n",
            false,
            ContentOrigin::Worker,
        );
        let end_offset = manager.output.end_offset().expect("output end offset");
        let SnapshotWithImages { buffer, .. } =
            snapshot_page_with_images(&manager.output, end_offset, 16);
        manager.pager.activate(buffer.expect("pager buffer"), false);

        manager
            .output_timeline
            .append_text(b"detached\n", false, ContentOrigin::Worker);

        let reply = manager
            .write_stdin_pager(
                String::new(),
                Duration::from_millis(0),
                Duration::from_millis(0),
                Some(16),
                true,
            )
            .expect("empty poll reply");

        let WorkerReply::Output { contents, .. } = reply;
        let text = contents_text(&contents);
        assert!(
            text.contains("detached\n"),
            "expected empty input to poll newly appended output before pager navigation, got: {text:?}"
        );
    }

    #[test]
    fn pager_empty_input_advances_page_after_worker_exit() {
        let _guard = output_ring_test_guard();
        let _output_ring = ensure_output_ring(OUTPUT_RING_CAPACITY_BYTES);
        reset_output_ring();
        reset_last_reply_marker_offset();

        let mut manager = WorkerManager::new(
            Backend::R,
            SandboxCliPlan::default(),
            crate::oversized_output::OversizedOutputMode::Pager,
        )
        .expect("worker manager");
        manager.process = Some(test_worker_process(sleeping_test_child()));
        manager.exe_path = PathBuf::from("definitely-missing-worker-exe");

        manager.output.start_capture();
        let output = (1..=24).map(|n| format!("L{n:04}\n")).collect::<String>();
        manager
            .output_timeline
            .append_text(output.as_bytes(), false, ContentOrigin::Worker);
        let end_offset = manager.output.end_offset().expect("output end offset");
        let SnapshotWithImages { buffer, .. } =
            snapshot_page_with_images(&manager.output, end_offset, 16);
        manager.pager.activate(buffer.expect("pager buffer"), false);

        {
            let process = manager.process.as_mut().expect("worker process");
            process.child.kill().expect("kill test child");
            process.exit_status = Some(process.child.wait().expect("wait test child"));
        }

        let reply = manager
            .write_stdin_pager(
                String::new(),
                Duration::from_millis(0),
                Duration::from_millis(0),
                Some(16),
                true,
            )
            .expect("empty pager reply");
        let WorkerReply::Output { contents, .. } = reply;
        let text = contents_text(&contents);

        if let Some(process) = manager.process.take() {
            let _ = process.finish_exited();
        }

        assert!(
            text.contains("L0002")
                || text.contains("L0003")
                || text.contains("L0010")
                || text.contains("L0014"),
            "expected blank pager input to advance to the next page after worker exit, got: {text:?}"
        );
        assert!(
            !text.contains("worker io error:"),
            "expected pager navigation instead of a respawn error after worker exit, got: {text:?}"
        );
    }

    #[test]
    fn pager_empty_input_preserves_idle_guardrail_notice() {
        let _guard = output_ring_test_guard();
        let _output_ring = ensure_output_ring(OUTPUT_RING_CAPACITY_BYTES);
        reset_output_ring();
        reset_last_reply_marker_offset();

        let mut manager = WorkerManager::new(
            Backend::R,
            SandboxCliPlan::default(),
            crate::oversized_output::OversizedOutputMode::Pager,
        )
        .expect("worker manager");
        manager.process = Some(test_worker_process(sleeping_test_child()));
        {
            let mut slot = manager
                .guardrail
                .event
                .lock()
                .expect("guardrail event mutex poisoned");
            *slot = Some(GuardrailEvent {
                message: "[repl] worker was idle; new session started\n".to_string(),
                was_busy: false,
            });
        }

        let reply = manager
            .write_stdin_pager(
                String::new(),
                Duration::from_millis(0),
                Duration::from_millis(0),
                Some(256),
                true,
            )
            .expect("empty poll reply");
        let WorkerReply::Output { contents, .. } = reply;
        let text = contents_text(&contents);

        if let Some(process) = manager.process.take() {
            let _ = process.kill();
        }

        assert!(
            text.contains("[repl] worker was idle; new session started"),
            "expected empty pager polls to preserve idle guardrail restart notices, got: {text:?}"
        );
    }

    #[test]
    fn pager_output_capture_skips_pending_output_tape() {
        let output_ring = Arc::new(OutputRing::with_capacity(OUTPUT_RING_CAPACITY_BYTES));
        let tape = PendingOutputTape::new();
        let capture = LiveOutputCapture::new(
            OversizedOutputMode::Pager,
            tape.clone(),
            OutputTimeline::new(output_ring.clone()),
        );
        capture.append_text(b"pager output\n", TextStream::Stdout);
        capture.append_image(IpcPlotImage {
            id: "img-1".to_string(),
            data: "AA==".to_string(),
            mime_type: "image/png".to_string(),
            is_new: true,
            readline_results_seen: 0,
        });
        capture.append_sideband(PendingSidebandKind::RequestEnd);

        assert!(
            tape.drain_final_snapshot().events.is_empty(),
            "pager mode should not mirror text, images, or sideband events into the pending tape"
        );
        let output_end = output_ring.end_offset();
        let output_range = output_ring.read_range(0, output_end);
        assert!(
            output_end > 0,
            "pager mode should still append text to the output timeline"
        );
        assert_eq!(
            output_range.bytes, b"pager output\n",
            "pager mode should keep stdout text in the output timeline"
        );
        assert!(
            output_range.events.iter().any(|event| {
                matches!(
                    &event.kind,
                    OutputEventKind::Image { id, mime_type, .. }
                    if id == "img-1" && mime_type == "image/png"
                )
            }),
            "pager mode should keep image events in the output timeline"
        );
    }

    #[test]
    fn python_driver_uses_small_ipc_request_start_signal() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");
        let mut driver = PythonBackendDriver::new();
        let big_input = "x".repeat(256 * 1024);

        driver.on_input_start(&big_input, &server);

        let msg = worker
            .recv(Some(Duration::from_millis(200)))
            .expect("expected stdin_write control message");
        match msg {
            ServerToWorkerIpcMessage::StdinWrite { text } => {
                assert!(
                    text.is_empty(),
                    "expected request-start signal without copying stdin payload"
                );
            }
            _ => panic!("expected stdin_write control message"),
        }
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn worker_manager_new_does_not_panic_for_non_utf8_tmpdir_env() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let _guard = env_test_mutex().lock().expect("env mutex");
        let _guard = cwd_test_mutex().lock().expect("cwd mutex");
        let original_tmpdir = std::env::var_os("TMPDIR");
        let non_utf8_tmpdir = OsString::from_vec(b"/tmp/non-utf8-\xFF-tmp".to_vec());

        unsafe {
            std::env::set_var("TMPDIR", &non_utf8_tmpdir);
        }
        let result = std::panic::catch_unwind(|| {
            WorkerManager::new(
                Backend::Python,
                SandboxCliPlan::default(),
                crate::oversized_output::OversizedOutputMode::Files,
            )
        });

        match original_tmpdir {
            Some(value) => unsafe {
                std::env::set_var("TMPDIR", value);
            },
            None => unsafe {
                std::env::remove_var("TMPDIR");
            },
        }

        assert!(result.is_ok(), "WorkerManager::new should not panic");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_bwrap_startup_retry_disables_bwrap_and_announces_fallback() {
        let mut manager = WorkerManager::new(
            Backend::R,
            SandboxCliPlan::default(),
            crate::oversized_output::OversizedOutputMode::Files,
        )
        .expect("worker manager");
        manager.sandbox_state.use_linux_sandbox_bwrap = true;
        manager.sandbox_defaults.use_linux_sandbox_bwrap = true;
        manager.inherited_sandbox_state = Some(SandboxState {
            use_linux_sandbox_bwrap: true,
            ..manager.sandbox_state.clone()
        });

        let retry = manager.maybe_retry_spawn_without_linux_bwrap(
            &WorkerError::Protocol("ipc disconnected while waiting for backend info".to_string()),
            false,
        );

        assert!(
            retry,
            "expected backend-info disconnect to trigger bwrap fallback"
        );
        assert!(
            !manager.sandbox_state.use_linux_sandbox_bwrap,
            "expected effective sandbox state to disable bwrap after fallback"
        );
        assert!(
            !manager.sandbox_defaults.use_linux_sandbox_bwrap,
            "expected sandbox defaults to disable bwrap after fallback"
        );
        assert!(
            manager
                .inherited_sandbox_state
                .as_ref()
                .is_some_and(|state| !state.use_linux_sandbox_bwrap),
            "expected inherited sandbox state to disable bwrap after fallback"
        );

        let snapshot = manager.pending_output_tape.drain_final_snapshot();
        let text = contents_text(&snapshot.format_contents().contents);
        assert!(
            text.contains("continuing without bwrap"),
            "expected fallback notice in visible output, got: {text:?}"
        );
    }

    #[test]
    fn failed_sandbox_update_does_not_commit_inherited_state() {
        let _guard = env_test_mutex().lock().expect("env mutex");
        let _guard = cwd_test_mutex().lock().expect("cwd mutex");
        let original_initial = std::env::var_os(crate::sandbox::INITIAL_SANDBOX_STATE_ENV);
        let initial = serde_json::json!({
            "sandboxPolicy": {
                "type": "workspace-write",
                "writable_roots": [],
                "network_access": false,
                "exclude_tmpdir_env_var": false,
                "exclude_slash_tmp": false
            }
        })
        .to_string();
        unsafe {
            std::env::set_var(crate::sandbox::INITIAL_SANDBOX_STATE_ENV, initial);
        }

        let plan = crate::sandbox_cli::SandboxCliPlan {
            operations: vec![
                crate::sandbox_cli::SandboxCliOperation::SetMode(
                    crate::sandbox_cli::SandboxModeArg::Inherit,
                ),
                crate::sandbox_cli::SandboxCliOperation::Config(
                    crate::sandbox_cli::SandboxConfigOperation::SetWorkspaceNetworkAccess(true),
                ),
            ],
        };
        let mut manager = WorkerManager::new(
            Backend::Python,
            plan,
            crate::oversized_output::OversizedOutputMode::Files,
        )
        .expect("worker manager");
        let inherited_before = manager
            .inherited_sandbox_state
            .clone()
            .expect("inherited state should be present");

        let err = manager
            .update_sandbox_state(
                SandboxStateUpdate {
                    sandbox_policy: SandboxPolicy::DangerFullAccess,
                    sandbox_cwd: None,
                    use_linux_sandbox_bwrap: None,
                    use_legacy_landlock: None,
                },
                Duration::from_millis(1),
            )
            .expect_err("danger-full-access should fail workspace-write-only config");
        assert!(
            matches!(err, WorkerError::Sandbox(ref msg) if msg.contains("requires workspace-write mode")),
            "unexpected error: {err}"
        );
        assert_eq!(
            manager.inherited_sandbox_state,
            Some(inherited_before),
            "failed updates must not mutate inherited sandbox baseline"
        );

        match original_initial {
            Some(value) => unsafe {
                std::env::set_var(crate::sandbox::INITIAL_SANDBOX_STATE_ENV, value);
            },
            None => unsafe {
                std::env::remove_var(crate::sandbox::INITIAL_SANDBOX_STATE_ENV);
            },
        }
    }

    #[test]
    fn apply_debug_startup_env_uses_mcp_repl_vars() {
        let _guard = env_test_mutex().lock().expect("env mutex");
        let original = std::env::var_os(crate::debug_logs::DEBUG_SESSION_DIR_ENV);
        let original_startup_path = std::env::var_os(crate::diagnostics::STARTUP_LOG_PATH_ENV);
        unsafe {
            std::env::set_var(
                crate::debug_logs::DEBUG_SESSION_DIR_ENV,
                "/tmp/mcp-repl-debug-session",
            );
            std::env::remove_var(crate::diagnostics::STARTUP_LOG_PATH_ENV);
        }

        let mut command = Command::new("env");
        apply_debug_startup_env(&mut command, None);
        let envs: std::collections::BTreeMap<_, _> = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect();

        match original {
            Some(value) => unsafe {
                std::env::set_var(crate::debug_logs::DEBUG_SESSION_DIR_ENV, value);
            },
            None => unsafe {
                std::env::remove_var(crate::debug_logs::DEBUG_SESSION_DIR_ENV);
            },
        }
        match original_startup_path {
            Some(value) => unsafe {
                std::env::set_var(crate::diagnostics::STARTUP_LOG_PATH_ENV, value);
            },
            None => unsafe {
                std::env::remove_var(crate::diagnostics::STARTUP_LOG_PATH_ENV);
            },
        }

        assert_eq!(
            envs.get(crate::debug_logs::DEBUG_SESSION_DIR_ENV),
            Some(&Some("/tmp/mcp-repl-debug-session".to_string()))
        );
        assert_eq!(envs.get(crate::diagnostics::STARTUP_LOG_PATH_ENV), None);
    }

    #[test]
    fn normalize_input_newlines_canonicalizes_crlf_and_cr() {
        assert_eq!(normalize_input_newlines("a\r\nb\rc\n"), "a\nb\nc\n");
    }

    #[test]
    fn apply_debug_startup_env_uses_session_tmpdir_for_worker_log() {
        let _guard = env_test_mutex().lock().expect("env mutex");
        let original = std::env::var_os(crate::debug_logs::DEBUG_SESSION_DIR_ENV);
        let original_startup_path = std::env::var_os(crate::diagnostics::STARTUP_LOG_PATH_ENV);
        unsafe {
            std::env::set_var(
                crate::debug_logs::DEBUG_SESSION_DIR_ENV,
                "/tmp/mcp-repl-debug-session",
            );
            std::env::remove_var(crate::diagnostics::STARTUP_LOG_PATH_ENV);
        }

        let mut command = Command::new("env");
        let session_tmpdir = PathBuf::from("/tmp/mcp-repl-session-tmp");
        apply_debug_startup_env(&mut command, Some(&session_tmpdir));
        let envs: std::collections::BTreeMap<_, _> = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect();

        match original {
            Some(value) => unsafe {
                std::env::set_var(crate::debug_logs::DEBUG_SESSION_DIR_ENV, value);
            },
            None => unsafe {
                std::env::remove_var(crate::debug_logs::DEBUG_SESSION_DIR_ENV);
            },
        }
        match original_startup_path {
            Some(value) => unsafe {
                std::env::set_var(crate::diagnostics::STARTUP_LOG_PATH_ENV, value);
            },
            None => unsafe {
                std::env::remove_var(crate::diagnostics::STARTUP_LOG_PATH_ENV);
            },
        }

        assert_eq!(
            envs.get(crate::debug_logs::DEBUG_SESSION_DIR_ENV),
            Some(&Some("/tmp/mcp-repl-debug-session".to_string()))
        );
        assert_eq!(
            envs.get(crate::diagnostics::STARTUP_LOG_PATH_ENV),
            Some(&Some(
                session_tmpdir
                    .join(crate::diagnostics::WORKER_STARTUP_LOG_FILE_NAME)
                    .display()
                    .to_string()
            ))
        );
    }

    #[test]
    fn persist_worker_startup_log_copies_into_debug_session_dir() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_tmpdir = temp.path().join("session-tmp");
        let debug_session_dir = temp.path().join("debug-session");
        std::fs::create_dir_all(&session_tmpdir).expect("create session tmpdir");
        std::fs::create_dir_all(&debug_session_dir).expect("create debug session dir");

        let source = session_tmpdir.join(crate::diagnostics::WORKER_STARTUP_LOG_FILE_NAME);
        let destination = debug_session_dir.join(crate::diagnostics::WORKER_STARTUP_LOG_FILE_NAME);
        std::fs::write(&source, "worker startup log\n").expect("write source log");

        persist_worker_startup_log(&session_tmpdir, Some(destination.clone()));

        assert_eq!(
            std::fs::read_to_string(&destination).expect("read destination log"),
            "worker startup log\n"
        );
    }

    #[test]
    fn cleanup_worker_session_tmpdir_persists_log_when_keep_tmpdir_is_set() {
        let _guard = env_test_mutex().lock().expect("env mutex");
        let temp = tempfile::tempdir().expect("tempdir");
        let session_tmpdir = temp.path().join("session-tmp");
        let debug_session_dir = temp.path().join("debug-session");
        std::fs::create_dir_all(&session_tmpdir).expect("create session tmpdir");
        std::fs::create_dir_all(&debug_session_dir).expect("create debug session dir");

        let source = session_tmpdir.join(crate::diagnostics::WORKER_STARTUP_LOG_FILE_NAME);
        let destination = debug_session_dir.join(crate::diagnostics::WORKER_STARTUP_LOG_FILE_NAME);
        std::fs::write(&source, "worker startup log\n").expect("write source log");

        let original_keep = std::env::var_os("MCP_REPL_KEEP_SESSION_TMPDIR");
        unsafe {
            std::env::set_var("MCP_REPL_KEEP_SESSION_TMPDIR", "1");
        }

        cleanup_worker_session_tmpdir(&session_tmpdir, Some(destination.clone()));

        match original_keep {
            Some(value) => unsafe {
                std::env::set_var("MCP_REPL_KEEP_SESSION_TMPDIR", value);
            },
            None => unsafe {
                std::env::remove_var("MCP_REPL_KEEP_SESSION_TMPDIR");
            },
        }

        assert!(
            session_tmpdir.is_dir(),
            "session tmpdir should be preserved"
        );
        assert_eq!(
            std::fs::read_to_string(&destination).expect("read destination log"),
            "worker startup log\n"
        );
    }

    #[cfg(target_family = "windows")]
    #[test]
    fn windows_ipc_connect_error_reaps_wrapper_process() {
        let mut child = Command::new("powershell.exe")
            .args(["-NoProfile", "-Command", "Start-Sleep -Seconds 30"])
            .spawn()
            .expect("spawn test child process");

        let result = handle_windows_ipc_connect_result(
            Err(std::io::Error::other("ipc connect failed")),
            &mut child,
        );
        assert!(matches!(result, Err(WorkerError::Io(_))));

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let status = child.try_wait().expect("query child status");
            if status.is_some() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("connect-error handler should reap child wrapper");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(target_family = "windows")]
    #[test]
    fn windows_soft_termination_does_not_kill_child() {
        let mut child = Command::new("powershell.exe")
            .args(["-NoProfile", "-Command", "Start-Sleep -Seconds 30"])
            .spawn()
            .expect("spawn test child process");

        request_soft_termination(&mut child).expect("soft terminate call should succeed");

        let status = child.try_wait().expect("query child status");
        assert!(
            status.is_none(),
            "child should still be running after soft termination request"
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(target_family = "windows")]
    #[test]
    fn windows_ipc_connect_timeout_is_bounded() {
        assert!(
            WINDOWS_IPC_CONNECT_MAX_WAIT <= Duration::from_secs(120),
            "windows IPC connect max wait should fail fast, got {:?}",
            WINDOWS_IPC_CONNECT_MAX_WAIT
        );
    }

    #[cfg(target_family = "windows")]
    #[test]
    fn windows_sandbox_prepare_access_denied_fails_fast() {
        let _guard = crate::windows_sandbox::prepare_sandbox_launch_test_mutex()
            .lock()
            .expect("windows sandbox test mutex");
        crate::windows_sandbox::set_prepare_sandbox_launch_test_error(Some(
            "failed to prepare writable ACL on 'C:\\workspace': SetNamedSecurityInfoW failed: 5"
                .to_string(),
        ));

        let mut manager = WorkerManager::new(
            Backend::R,
            SandboxCliPlan::default(),
            crate::oversized_output::OversizedOutputMode::Files,
        )
        .expect("worker manager");
        let result = manager.ensure_windows_sandbox_launch();

        crate::windows_sandbox::set_prepare_sandbox_launch_test_error(None);

        assert!(
            matches!(
                result,
                Err(WorkerError::Sandbox(ref message))
                    if message.contains("SetNamedSecurityInfoW failed: 5")
            ),
            "access-denied prepare failures should abort launch preparation, got: {result:?}"
        );
        assert!(
            manager.windows_sandbox_launch.is_none(),
            "failed launch preparation should not cache a prepared launch"
        );
    }

    #[cfg(target_family = "windows")]
    #[test]
    fn windows_sandbox_cache_hit_refreshes_prepared_launch_acl_state_before_reuse() {
        let _guard = crate::windows_sandbox::prepare_sandbox_launch_test_mutex()
            .lock()
            .expect("windows sandbox test mutex");

        let mut manager = WorkerManager::new(
            Backend::R,
            SandboxCliPlan::default(),
            crate::oversized_output::OversizedOutputMode::Files,
        )
        .expect("worker manager");

        let first = manager
            .ensure_windows_sandbox_launch()
            .expect("initial launch preparation should succeed");
        assert!(
            first.is_some(),
            "initial launch preparation should populate the prepared-launch cache"
        );

        crate::windows_sandbox::set_apply_prepared_launch_acl_state_test_error(Some(
            "cache hit should refresh ACL state".to_string(),
        ));

        let second = manager.ensure_windows_sandbox_launch();

        crate::windows_sandbox::set_apply_prepared_launch_acl_state_test_error(None);

        assert!(
            matches!(
                second,
                Err(WorkerError::Sandbox(ref err))
                    if err.contains("cache hit should refresh ACL state")
            ),
            "cache hits should refresh ACL state before reusing the prepared launch, got: {second:?}"
        );
        assert_eq!(
            manager.windows_sandbox_launch, first,
            "cache-hit refresh failures should preserve the cached launch for later retries"
        );
    }
}
