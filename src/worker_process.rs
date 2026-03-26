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
#[cfg(feature = "pager")]
use crate::output_capture::{
    OUTPUT_RING_CAPACITY_BYTES, OutputBuffer, OutputEventKind, OutputRange, OutputTextSpan,
    OutputTimeline, ensure_output_ring, reset_last_reply_marker_offset, reset_output_ring,
    set_last_reply_marker_offset, update_last_reply_marker_offset_max,
};
use crate::oversized_output::OversizedOutputMode;
#[cfg(feature = "pager")]
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
    TextStream, WORKER_MODE_ARG, WorkerContent, WorkerErrorCode, WorkerReply,
};

#[cfg(target_family = "unix")]
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
#[cfg(target_family = "unix")]
use std::os::unix::process::CommandExt;
#[cfg(target_family = "unix")]
use sysinfo::{Pid, ProcessesToUpdate, System};

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

#[cfg(target_family = "unix")]
const WORKER_MEM_GUARDRAIL_RATIO: f64 = 0.75;
#[cfg(target_family = "unix")]
const WORKER_MEM_GUARDRAIL_ACTIVE_INTERVAL: Duration = Duration::from_secs(10);
#[cfg(target_family = "unix")]
const WORKER_MEM_GUARDRAIL_IDLE_INTERVAL: Duration = Duration::from_secs(60);

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
    ipc.clear_request_end_events();
    ipc.clear_readline_tracking();
    ipc.clear_prompt_history();
    ipc.clear_echo_events();
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
                let (prompt, prompt_variants) = collect_completion_metadata(&ipc);
                return Ok(CompletionInfo {
                    prompt,
                    prompt_variants: Some(prompt_variants),
                    #[cfg(feature = "pager")]
                    echo_events: ipc.take_echo_events(),
                    session_end_seen: false,
                });
            }
            Err(IpcWaitError::Timeout) => {
                if ipc.waiting_for_next_input(REQUEST_END_FALLBACK_WAIT)
                    && ipc.try_take_request_end()
                {
                    let (prompt, prompt_variants) = collect_completion_metadata(&ipc);
                    return Ok(CompletionInfo {
                        prompt,
                        prompt_variants: Some(prompt_variants),
                        #[cfg(feature = "pager")]
                        echo_events: ipc.take_echo_events(),
                        session_end_seen: false,
                    });
                }
                continue;
            }
            Err(IpcWaitError::SessionEnd) => {
                return Ok(CompletionInfo {
                    prompt: None,
                    prompt_variants: None,
                    #[cfg(feature = "pager")]
                    echo_events: Vec::new(),
                    session_end_seen: true,
                });
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
        driver_interrupt(process)
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

fn collect_completion_metadata(ipc: &ServerIpcConnection) -> (Option<String>, Vec<String>) {
    let mut prompt = ipc.try_take_prompt().filter(|value| !value.is_empty());
    let mut prompt_variants = ipc.take_prompt_history();
    let mut echo_event_count = ipc.pending_echo_event_count();

    let start = std::time::Instant::now();
    let mut stable_for = Duration::from_millis(0);
    while start.elapsed() < COMPLETION_METADATA_SETTLE_MAX {
        thread::sleep(COMPLETION_METADATA_SETTLE_POLL);
        let next_prompt = ipc.try_take_prompt().filter(|value| !value.is_empty());
        let mut next_prompt_variants = ipc.take_prompt_history();
        let next_echo_event_count = ipc.pending_echo_event_count();
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
            if stable_for >= COMPLETION_METADATA_STABLE {
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
    session_end_seen: bool,
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
        "codex_linux_sandbox_exe": sandbox_state
            .codex_linux_sandbox_exe
            .as_ref()
            .map(|path| path.to_string_lossy().to_string()),
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
    oversized_output: OversizedOutputMode,
    pending_output_tape: PendingOutputTape,
    #[cfg(feature = "pager")]
    output: OutputBuffer,
    #[cfg(feature = "pager")]
    pager: Pager,
    #[cfg(feature = "pager")]
    output_timeline: OutputTimeline,
    driver: Box<dyn BackendDriver>,
    pending_request: bool,
    pending_request_started_at: Option<std::time::Instant>,
    session_end_seen: bool,
    last_detached_prefix_item_count: usize,
    #[cfg(feature = "pager")]
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
        #[cfg(feature = "pager")]
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
            oversized_output,
            pending_output_tape: PendingOutputTape::new(),
            #[cfg(feature = "pager")]
            output: OutputBuffer::default(),
            #[cfg(feature = "pager")]
            pager: Pager::default(),
            #[cfg(feature = "pager")]
            output_timeline,
            driver: match backend {
                Backend::R => Box::new(RBackendDriver::new()),
                Backend::Python => Box::new(PythonBackendDriver::new()),
            },
            pending_request: false,
            pending_request_started_at: None,
            session_end_seen: false,
            last_detached_prefix_item_count: 0,
            #[cfg(feature = "pager")]
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

    #[cfg(feature = "pager")]
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
            OversizedOutputMode::Pager => {
                #[cfg(feature = "pager")]
                {
                    self.write_stdin_pager(
                        text,
                        worker_timeout,
                        server_timeout,
                        page_bytes_override,
                        echo_input,
                    )
                }
                #[cfg(not(feature = "pager"))]
                {
                    let _ = (page_bytes_override, echo_input);
                    Err(WorkerError::Protocol(
                        "pager mode is unavailable in this build".to_string(),
                    ))
                }
            }
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
            return self.write_stdin_files(remaining.to_string(), worker_timeout, server_timeout);
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
            if self.pending_request || self.pending_output_tape.has_pending() {
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
            let WorkerReply::Output {
                contents,
                is_error,
                error_code,
                ..
            } = &mut reply.reply;
            contents.push(WorkerContent::server_stderr(
                "[repl] input discarded while worker busy",
            ));
            *is_error = true;
            if error_code.is_none() {
                *error_code = Some(WorkerErrorCode::Busy);
            }
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

    #[cfg(feature = "pager")]
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
            return self.write_stdin_pager(
                remaining.to_string(),
                worker_timeout,
                server_timeout,
                page_bytes_override,
                echo_input,
            );
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

        if self.pager.is_active() {
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

        let page_bytes = pager::resolve_page_bytes(page_bytes_override);
        if let Err(err) = self.ensure_process() {
            let input_context = self.prepare_input_context_pager(&text, echo_input);
            let reply = self.build_reply_from_worker_error_pager(&err, input_context, page_bytes);
            let reply = self.finalize_reply(reply);
            self.maybe_reset_after_session_end();
            return Ok(reply);
        }
        self.output.start_capture();
        self.maybe_emit_guardrail_notice();
        self.resolve_timeout_marker();
        if text.is_empty() {
            if self.pending_request || self.output.has_pending_output() {
                let reply = self.poll_pending_output_pager(worker_timeout, page_bytes)?;
                let reply = self.finalize_reply(reply);
                self.maybe_reset_after_session_end();
                return Ok(reply);
            }
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
            let WorkerReply::Output {
                contents,
                is_error,
                error_code,
                ..
            } = &mut reply.reply;
            contents.push(WorkerContent::server_stderr(
                "[repl] input discarded while worker busy",
            ));
            *is_error = true;
            if error_code.is_none() {
                *error_code = Some(WorkerErrorCode::Busy);
            }
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

    #[cfg(feature = "pager")]
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
        let mut completion = CompletionInfo {
            prompt: None,
            prompt_variants: None,
            echo_events: Vec::new(),
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
                    } else {
                        timed_out = true;
                    }
                }
                Err(err) => return Err(err),
            }
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

    #[cfg(feature = "pager")]
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
        let FormattedPendingOutput {
            contents,
            saw_stderr,
        } = self.drain_final_formatted_output();
        InputContext {
            prefix_contents: contents,
            prefix_is_error: saw_stderr,
            start_offset: 0,
            prefix_bytes: 0,
            input_echo: None,
        }
    }

    #[cfg(feature = "pager")]
    fn prepare_input_context_pager(&mut self, text: &str, echo_input: bool) -> InputContext {
        self.output.start_capture();

        let had_pending_output = self.output.has_pending_output();
        let saw_background_output = self.output.pending_output_since_last_reply();

        let mut input_echo = echo_input
            .then(|| text.to_string())
            .and_then(|value| pager::build_input_echo(&value));

        let mut prefix_contents = Vec::new();
        let mut prefix_bytes: u64 = 0;
        let mut prefix_is_error = false;

        if had_pending_output {
            let pending_end = self.output.end_offset().unwrap_or(0);
            let pending_start = self.output.current_offset().unwrap_or(pending_end);
            let pending_bytes = pending_end.saturating_sub(pending_start);

            prefix_is_error = self
                .output
                .saw_stderr_in_range(pending_start.min(pending_end), pending_end);
            prefix_contents = pager::take_range_from_ring(&self.output, pending_end);
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
        let formatted = self.drain_final_formatted_output();
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

    #[cfg(feature = "pager")]
    fn build_reply_from_worker_error_pager(
        &mut self,
        err: &WorkerError,
        context: InputContext,
        page_bytes: u64,
    ) -> ReplyWithOffset {
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

    #[cfg(feature = "pager")]
    fn build_reply_from_request_pager(
        &mut self,
        request: RequestState,
        context: InputContext,
        page_bytes: u64,
    ) -> Result<ReplyWithOffset, WorkerError> {
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
        let mut result = self.driver.wait_for_completion(timeout, ipc);
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
                    session_end_seen: true,
                });
            }
        }
        // Best-effort: after IPC completion, give the output reader threads a brief window to
        // drain any bytes already written by the worker before we snapshot the ring.
        let elapsed = start.elapsed();
        let remaining = timeout.saturating_sub(elapsed);
        self.settle_output_after_request_end(remaining);
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
        let poll = Duration::from_millis(5);
        let start = std::time::Instant::now();

        let mut last = match self.oversized_output {
            OversizedOutputMode::Files => self.pending_output_tape.current_seq(),
            OversizedOutputMode::Pager => {
                #[cfg(feature = "pager")]
                {
                    self.output.end_offset().unwrap_or(0)
                }
                #[cfg(not(feature = "pager"))]
                {
                    0
                }
            }
        };
        let mut stable_for = Duration::from_millis(0);
        while start.elapsed() < total {
            thread::sleep(poll);
            let now = match self.oversized_output {
                OversizedOutputMode::Files => self.pending_output_tape.current_seq(),
                OversizedOutputMode::Pager => {
                    #[cfg(feature = "pager")]
                    {
                        self.output.end_offset().unwrap_or(0)
                    }
                    #[cfg(not(feature = "pager"))]
                    {
                        0
                    }
                }
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
            OversizedOutputMode::Pager => {
                #[cfg(feature = "pager")]
                self.output_timeline
                    .append_text(event.message.as_bytes(), true);
            }
        }
    }

    fn finalize_reply(&self, reply: ReplyWithOffset) -> WorkerReply {
        if matches!(self.oversized_output, OversizedOutputMode::Pager) {
            #[cfg(feature = "pager")]
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
                            .append_server_stderr_bytes(message.as_bytes()),
                        OversizedOutputMode::Pager => {
                            #[cfg(feature = "pager")]
                            self.output_timeline.append_text(message.as_bytes(), true);
                        }
                    }
                } else {
                    let message = "[repl] session ended\n".to_string();
                    match self.oversized_output {
                        OversizedOutputMode::Files => self
                            .pending_output_tape
                            .append_stdout_status_line(message.as_bytes()),
                        OversizedOutputMode::Pager => {
                            #[cfg(feature = "pager")]
                            self.output_timeline.append_text(message.as_bytes(), false);
                        }
                    }
                }
            }
        }
    }

    fn maybe_reset_after_session_end(&mut self) {
        if self.session_end_seen {
            let _ = match self.oversized_output {
                OversizedOutputMode::Files => self.reset(),
                OversizedOutputMode::Pager => {
                    #[cfg(feature = "pager")]
                    {
                        self.reset_with_pager(self.pager.is_active())
                    }
                    #[cfg(not(feature = "pager"))]
                    {
                        Err(WorkerError::Protocol(
                            "pager mode is unavailable in this build".to_string(),
                        ))
                    }
                }
            };
            self.session_end_seen = false;
        }
    }

    pub fn interrupt(&mut self, timeout: Duration) -> Result<WorkerReply, WorkerError> {
        match self.oversized_output {
            OversizedOutputMode::Files => self.interrupt_files(timeout),
            OversizedOutputMode::Pager => {
                #[cfg(feature = "pager")]
                {
                    self.interrupt_pager(timeout)
                }
                #[cfg(not(feature = "pager"))]
                {
                    Err(WorkerError::Protocol(
                        "pager mode is unavailable in this build".to_string(),
                    ))
                }
            }
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
        if let Err(err) = self.driver.interrupt(
            self.process
                .as_mut()
                .expect("worker process should be available"),
        ) {
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
            OversizedOutputMode::Pager => {
                #[cfg(feature = "pager")]
                {
                    self.restart_pager(timeout)
                }
                #[cfg(not(feature = "pager"))]
                {
                    Err(WorkerError::Protocol(
                        "pager mode is unavailable in this build".to_string(),
                    ))
                }
            }
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
        self.reset_output_state_files();
        crate::event_log::log("worker_restart_end", serde_json::json!({"status": "ok"}));
        Ok(self.finalize_reply(reply))
    }

    #[cfg(feature = "pager")]
    fn interrupt_pager(&mut self, timeout: Duration) -> Result<WorkerReply, WorkerError> {
        crate::event_log::log(
            "worker_interrupt_begin",
            serde_json::json!({
                "timeout_ms": timeout.as_millis(),
            }),
        );
        self.ensure_process()?;
        if let Err(err) = self.driver.interrupt(
            self.process
                .as_mut()
                .expect("worker process should be available"),
        ) {
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

    #[cfg(feature = "pager")]
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
        self.reset_output_state_pager(false);
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
                process.cleanup_session_tmpdir();
            }
            self.process = Some(match self.oversized_output {
                OversizedOutputMode::Files => self.spawn_process_files()?,
                OversizedOutputMode::Pager => {
                    #[cfg(feature = "pager")]
                    {
                        self.spawn_process_with_pager(false)?
                    }
                    #[cfg(not(feature = "pager"))]
                    {
                        return Err(WorkerError::Protocol(
                            "pager mode is unavailable in this build".to_string(),
                        ));
                    }
                }
            });
        }

        Ok(())
    }

    fn reset(&mut self) -> Result<(), WorkerError> {
        crate::event_log::log("worker_reset_begin", serde_json::json!({}));
        if let Some(process) = self.process.take() {
            let _ = process.kill();
        }
        self.guardrail.busy.store(false, Ordering::Relaxed);
        if self.awaiting_initial_sandbox_state_update {
            return Err(WorkerError::Sandbox(
                MISSING_INHERITED_SANDBOX_STATE_MESSAGE.to_string(),
            ));
        }
        self.process = Some(match self.oversized_output {
            OversizedOutputMode::Files => self.spawn_process_files()?,
            OversizedOutputMode::Pager => {
                #[cfg(feature = "pager")]
                {
                    self.spawn_process_with_pager(false)?
                }
                #[cfg(not(feature = "pager"))]
                {
                    return Err(WorkerError::Protocol(
                        "pager mode is unavailable in this build".to_string(),
                    ));
                }
            }
        });
        crate::event_log::log("worker_reset_end", serde_json::json!({"status": "ok"}));
        Ok(())
    }

    #[cfg(feature = "pager")]
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
        self.guardrail.busy.store(false, Ordering::Relaxed);
        if self.awaiting_initial_sandbox_state_update {
            return Err(WorkerError::Sandbox(
                MISSING_INHERITED_SANDBOX_STATE_MESSAGE.to_string(),
            ));
        }
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
                self.guardrail.busy.store(false, Ordering::Relaxed);
                self.process = Some(match self.oversized_output {
                    OversizedOutputMode::Files => self.spawn_process_files()?,
                    OversizedOutputMode::Pager => {
                        #[cfg(feature = "pager")]
                        {
                            self.spawn_process_with_pager(false)?
                        }
                        #[cfg(not(feature = "pager"))]
                        {
                            return Err(WorkerError::Protocol(
                                "pager mode is unavailable in this build".to_string(),
                            ));
                        }
                    }
                });
                return Ok(true);
            }
            return Ok(false);
        }

        if let Some(process) = self.process.take() {
            let _ = process.shutdown_graceful(timeout);
        }
        self.guardrail.busy.store(false, Ordering::Relaxed);
        self.process = Some(match self.oversized_output {
            OversizedOutputMode::Files => self.spawn_process_files()?,
            OversizedOutputMode::Pager => {
                #[cfg(feature = "pager")]
                {
                    self.spawn_process_with_pager(false)?
                }
                #[cfg(not(feature = "pager"))]
                {
                    return Err(WorkerError::Protocol(
                        "pager mode is unavailable in this build".to_string(),
                    ));
                }
            }
        });
        Ok(true)
    }

    fn reset_output_state_files(&mut self) {
        self.pending_output_tape.clear();
        self.pending_request = false;
        self.pending_request_started_at = None;
        self.session_end_seen = false;
        self.last_detached_prefix_item_count = 0;
        self.last_prompt = None;
        self.guardrail.busy.store(false, Ordering::Relaxed);
    }

    #[cfg(feature = "pager")]
    fn reset_output_state_pager(&mut self, preserve_pager: bool) {
        self.pending_output_tape.clear();
        reset_output_ring();
        reset_last_reply_marker_offset();
        self.output = OutputBuffer::default();
        if !preserve_pager {
            self.pager = Pager::default();
        }
        self.pending_request = false;
        self.pending_request_started_at = None;
        self.session_end_seen = false;
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

    #[cfg(feature = "pager")]
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
        self.reset_output_state_files();
        crate::event_log::log_lazy("worker_spawn_begin", || {
            worker_context_event_payload(self.backend, &self.sandbox_state)
        });
        let process = WorkerProcess::spawn(
            self.backend,
            &self.exe_path,
            &self.sandbox_state,
            self.pending_output_tape.clone(),
            #[cfg(feature = "pager")]
            self.output_timeline.clone(),
            self.guardrail.clone(),
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

    #[cfg(feature = "pager")]
    fn spawn_process_with_pager(
        &mut self,
        preserve_pager: bool,
    ) -> Result<WorkerProcess, WorkerError> {
        self.reset_output_state_pager(preserve_pager);
        crate::event_log::log_lazy("worker_spawn_begin", || {
            worker_context_event_payload(self.backend, &self.sandbox_state)
        });
        let process = WorkerProcess::spawn(
            self.backend,
            &self.exe_path,
            &self.sandbox_state,
            self.pending_output_tape.clone(),
            self.output_timeline.clone(),
            self.guardrail.clone(),
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
                self.settle_output_after_request_end(Duration::from_millis(120));
                if matches!(self.oversized_output, OversizedOutputMode::Pager) {
                    #[cfg(feature = "pager")]
                    update_last_reply_marker_offset_max(self.output.end_offset().unwrap_or(0));
                }
                self.clear_pending_request_state();
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
        self.guardrail.busy.store(false, Ordering::Relaxed);
    }

    fn build_session_reset_reply_files(&mut self, meta: &str) -> ReplyWithOffset {
        let FormattedPendingOutput {
            mut contents,
            saw_stderr,
        } = self.drain_final_formatted_output();
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

    #[cfg(feature = "pager")]
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

#[cfg(feature = "pager")]
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

#[cfg(feature = "pager")]
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

#[cfg(feature = "pager")]
fn snapshot_after_completion(
    output: &OutputBuffer,
    start_offset: u64,
    end_offset: u64,
    target_bytes: u64,
    completion: &CompletionInfo,
) -> CompletionSnapshot {
    let trim_enabled = should_trim_echo_prefix(&completion.echo_events);
    if !trim_enabled {
        let saw_stderr = output.saw_stderr_in_range(start_offset.min(end_offset), end_offset);
        let range = output.read_range(start_offset, end_offset);
        output.advance_offset_to(end_offset);
        let prompt_variants = completion.prompt_variants.clone().unwrap_or_default();
        let (bytes, events, text_spans) =
            collapse_echo_with_attribution(range, &completion.echo_events, &prompt_variants);
        let snapshot = snapshot_page_with_images_from_collapsed(
            bytes,
            events,
            text_spans,
            end_offset,
            target_bytes,
        );
        return CompletionSnapshot {
            snapshot,
            saw_stderr,
        };
    }

    let echo_transcript = echo_transcript_from_events(&completion.echo_events);
    if let Some(echo) = echo_transcript.as_deref() {
        let _ = drop_echo_only_output(output, start_offset, end_offset, echo);
    }

    let _ = trim_echo_prefix_in_output(output, echo_transcript.as_deref(), trim_enabled);
    let effective_start = output.current_offset().unwrap_or(start_offset);
    let saw_stderr = output.saw_stderr_in_range(effective_start.min(end_offset), end_offset);

    let mut snapshot = snapshot_page_with_images(output, end_offset, target_bytes);
    maybe_trim_echo_prefix(&mut snapshot.contents, echo_transcript.as_deref(), true);
    CompletionSnapshot {
        snapshot,
        saw_stderr,
    }
}

#[cfg(feature = "pager")]
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

#[cfg(feature = "pager")]
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

#[cfg(feature = "pager")]
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

#[cfg(feature = "pager")]
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

#[cfg(feature = "pager")]
fn line_matches_echo_event(line: &[u8], event: &IpcEchoEvent) -> bool {
    let prompt = event.prompt.as_bytes();
    let consumed = event.line.as_bytes();
    if line.len() != prompt.len().saturating_add(consumed.len()) {
        return false;
    }
    let (prefix, suffix) = line.split_at(prompt.len());
    prefix == prompt && suffix == consumed
}

#[cfg(feature = "pager")]
#[derive(Default)]
struct PendingEchoRun {
    lines: usize,
    bytes: usize,
    head: Option<Vec<u8>>,
    tail: Option<Vec<u8>>,
}

#[cfg(feature = "pager")]
impl PendingEchoRun {
    fn push(&mut self, line: &[u8]) {
        self.lines = self.lines.saturating_add(1);
        self.bytes = self.bytes.saturating_add(line.len());
        if self.head.is_none() {
            self.head = Some(line.to_vec());
        }
        self.tail = Some(line.to_vec());
    }

    fn take(&mut self) -> PendingEchoRun {
        std::mem::take(self)
    }

    fn is_empty(&self) -> bool {
        self.lines == 0
    }
}

#[cfg(feature = "pager")]
fn strip_trailing_newlines_bytes(bytes: &[u8]) -> &[u8] {
    let mut end = bytes.len();
    while end > 0 && matches!(bytes[end - 1], b'\n' | b'\r') {
        end -= 1;
    }
    &bytes[..end]
}

#[cfg(feature = "pager")]
fn is_ascii_whitespace_only(bytes: &[u8]) -> bool {
    bytes.iter().all(|b| b.is_ascii_whitespace())
}

#[cfg(feature = "pager")]
fn prompt_variants_bytes(prompt_variants: &[String]) -> Vec<Vec<u8>> {
    prompt_variants
        .iter()
        .filter_map(|prompt| {
            let trimmed = prompt.trim_end_matches(['\n', '\r']);
            (!trimmed.is_empty()).then_some(trimmed.as_bytes().to_vec())
        })
        .collect()
}

#[cfg(feature = "pager")]
fn is_prompt_only_fragment(bytes: &[u8], prompt_variants: &[Vec<u8>]) -> bool {
    let trimmed = strip_trailing_newlines_bytes(bytes);
    if trimmed.is_empty() {
        return false;
    }
    prompt_variants.iter().any(|p| p.as_slice() == trimmed)
}

#[cfg(feature = "pager")]
fn summarize_middle(text: &str, head_chars: usize, tail_chars: usize) -> String {
    let total = text.chars().count();
    if total <= head_chars.saturating_add(tail_chars).saturating_add(8) {
        return text.to_string();
    }
    let head = text.chars().take(head_chars).collect::<String>();
    let tail = text
        .chars()
        .rev()
        .take(tail_chars)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    format!("{head} .... [ELIDED] .... {tail}")
}

#[cfg(feature = "pager")]
fn summarize_echo_line_for_marker(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(strip_trailing_newlines_bytes(bytes));
    summarize_middle(&text, 80, 40)
}

#[cfg(feature = "pager")]
fn summarize_echo_line_for_output(bytes: &[u8]) -> Vec<u8> {
    const MAX_CHARS: usize = 220;
    let had_newline = bytes.ends_with(b"\n");
    let text = String::from_utf8_lossy(strip_trailing_newlines_bytes(bytes));
    let summarized = if text.chars().count() > MAX_CHARS {
        summarize_middle(&text, 120, 60)
    } else {
        text.to_string()
    };
    let mut out = summarized.into_bytes();
    if had_newline {
        out.push(b'\n');
    }
    out
}

#[cfg(feature = "pager")]
fn collapse_echo_with_attribution(
    range: OutputRange,
    echo_events: &[IpcEchoEvent],
    prompt_variants: &[String],
) -> (Vec<u8>, Vec<(u64, OutputEventKind)>, Vec<OutputTextSpan>) {
    const ECHO_MARKER_MIN_BYTES: usize = 512;

    let mut out_bytes: Vec<u8> = Vec::new();
    let mut out_events: Vec<(u64, OutputEventKind)> = Vec::new();
    let mut out_text_spans: Vec<OutputTextSpan> = Vec::new();

    let prompt_variants = prompt_variants_bytes(prompt_variants);
    let mut pending = PendingEchoRun::default();
    let mut echo_idx = 0usize;

    let base_offset = range.start_offset;
    let end_offset = range.end_offset;
    let bytes = range.bytes;
    let text_spans = range.text_spans;

    // Convert ring offsets to byte indices within `bytes`.
    let mut events: Vec<(usize, OutputEventKind)> = range
        .events
        .into_iter()
        .filter_map(|event| {
            if event.offset < base_offset || event.offset > end_offset {
                return None;
            }
            let rel = event.offset.saturating_sub(base_offset) as usize;
            Some((rel.min(bytes.len()), event.kind))
        })
        .collect();
    events.sort_by_key(|(offset, _)| *offset);

    let mut flush_pending = |out_bytes: &mut Vec<u8>,
                             out_text_spans: &mut Vec<OutputTextSpan>,
                             pending: &mut PendingEchoRun| {
        if pending.is_empty() {
            return;
        }
        let pending = pending.take();
        let head = pending.head.as_deref().unwrap_or_default();
        let tail = pending.tail.as_deref().unwrap_or_default();
        if pending.lines >= 2 || pending.bytes >= ECHO_MARKER_MIN_BYTES {
            let head_snip = summarize_echo_line_for_marker(head);
            let tail_snip = summarize_echo_line_for_marker(tail);
            let marker = format!(
                "[repl] echoed input elided: {} lines ({} bytes); head: {}; tail: {}\n",
                pending.lines, pending.bytes, head_snip, tail_snip
            );
            append_text_with_span(out_bytes, out_text_spans, marker.as_bytes(), false);
        } else {
            append_text_with_span(
                out_bytes,
                out_text_spans,
                &summarize_echo_line_for_output(tail),
                false,
            );
        }
    };

    let mut cursor = 0usize;
    for (event_offset, kind) in events {
        let event_offset = event_offset.min(bytes.len());
        if event_offset > cursor {
            consume_text_segment_with_spans(
                &bytes[cursor..event_offset],
                cursor,
                &text_spans,
                echo_events,
                &mut echo_idx,
                &prompt_variants,
                &mut pending,
                &mut flush_pending,
                &mut out_bytes,
                &mut out_text_spans,
            );
            cursor = event_offset;
        }

        // Image events can race with stdout capture and land at slightly different byte offsets.
        // Treat text events as hard boundaries, but avoid splitting echo runs on image markers.
        if matches!(kind, OutputEventKind::Text { .. }) {
            flush_pending(&mut out_bytes, &mut out_text_spans, &mut pending);
        }
        out_events.push((out_bytes.len() as u64, kind));
    }

    if cursor < bytes.len() {
        consume_text_segment_with_spans(
            &bytes[cursor..],
            cursor,
            &text_spans,
            echo_events,
            &mut echo_idx,
            &prompt_variants,
            &mut pending,
            &mut flush_pending,
            &mut out_bytes,
            &mut out_text_spans,
        );
    }

    // Drop any trailing echo-only run (no output followed it).
    (out_bytes, out_events, out_text_spans)
}

#[cfg(feature = "pager")]
fn append_text_with_span(
    out_bytes: &mut Vec<u8>,
    out_text_spans: &mut Vec<OutputTextSpan>,
    bytes: &[u8],
    is_stderr: bool,
) {
    if bytes.is_empty() {
        return;
    }
    let start_byte = out_bytes.len();
    out_bytes.extend_from_slice(bytes);
    let end_byte = out_bytes.len();
    if let Some(last) = out_text_spans.last_mut()
        && last.is_stderr == is_stderr
        && last.end_byte == start_byte
    {
        last.end_byte = end_byte;
    } else {
        out_text_spans.push(OutputTextSpan {
            start_byte,
            end_byte,
            is_stderr,
        });
    }
}

#[cfg(feature = "pager")]
#[allow(clippy::too_many_arguments)]
fn consume_text_segment_with_spans(
    segment: &[u8],
    segment_start: usize,
    text_spans: &[OutputTextSpan],
    echo_events: &[IpcEchoEvent],
    echo_idx: &mut usize,
    prompt_variants: &[Vec<u8>],
    pending: &mut PendingEchoRun,
    flush_pending: &mut impl FnMut(&mut Vec<u8>, &mut Vec<OutputTextSpan>, &mut PendingEchoRun),
    out_bytes: &mut Vec<u8>,
    out_text_spans: &mut Vec<OutputTextSpan>,
) {
    let segment_end = segment_start.saturating_add(segment.len());
    let mut cursor = segment_start;
    for span in text_spans {
        if span.end_byte <= segment_start {
            continue;
        }
        if span.start_byte >= segment_end {
            break;
        }
        let start = span.start_byte.max(segment_start);
        let end = span.end_byte.min(segment_end);
        if cursor < start {
            consume_text_segment(
                &segment[cursor - segment_start..start - segment_start],
                false,
                echo_events,
                echo_idx,
                prompt_variants,
                pending,
                flush_pending,
                out_bytes,
                out_text_spans,
            );
        }
        consume_text_segment(
            &segment[start - segment_start..end - segment_start],
            span.is_stderr,
            echo_events,
            echo_idx,
            prompt_variants,
            pending,
            flush_pending,
            out_bytes,
            out_text_spans,
        );
        cursor = end;
    }
    if cursor < segment_end {
        consume_text_segment(
            &segment[cursor - segment_start..],
            false,
            echo_events,
            echo_idx,
            prompt_variants,
            pending,
            flush_pending,
            out_bytes,
            out_text_spans,
        );
    }
}

#[cfg(feature = "pager")]
#[allow(clippy::too_many_arguments)]
fn consume_text_segment(
    segment: &[u8],
    is_stderr: bool,
    echo_events: &[IpcEchoEvent],
    echo_idx: &mut usize,
    prompt_variants: &[Vec<u8>],
    pending: &mut PendingEchoRun,
    flush_pending: &mut impl FnMut(&mut Vec<u8>, &mut Vec<OutputTextSpan>, &mut PendingEchoRun),
    out_bytes: &mut Vec<u8>,
    out_text_spans: &mut Vec<OutputTextSpan>,
) {
    let mut start = 0usize;
    while start < segment.len() {
        let mut end = start;
        while end < segment.len() && segment[end] != b'\n' {
            end += 1;
        }
        if end < segment.len() && segment[end] == b'\n' {
            end += 1;
        }
        let line = &segment[start..end];
        start = end;

        let is_echo =
            *echo_idx < echo_events.len() && line_matches_echo_event(line, &echo_events[*echo_idx]);
        if is_echo {
            pending.push(line);
            *echo_idx = echo_idx.saturating_add(1);
            continue;
        }

        let substantive =
            !is_ascii_whitespace_only(line) && !is_prompt_only_fragment(line, prompt_variants);
        if substantive {
            flush_pending(out_bytes, out_text_spans, pending);
        }
        append_text_with_span(out_bytes, out_text_spans, line, is_stderr);
    }
}

#[cfg(feature = "pager")]
fn should_trim_echo_prefix(events: &[IpcEchoEvent]) -> bool {
    if events.is_empty() {
        return false;
    }
    if events.len() == 1 {
        return true;
    }
    events
        .iter()
        .skip(1)
        .all(|event| is_continuation_prompt(&event.prompt))
}

#[cfg(feature = "pager")]
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

#[cfg(feature = "pager")]
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
    if echo_prefix.is_empty() {
        return;
    }

    let mut remaining = echo_prefix;
    for content in contents.iter() {
        if remaining.is_empty() {
            break;
        }
        let WorkerContent::ContentText { text, stream, .. } = content else {
            return;
        };
        if !matches!(stream, TextStream::Stdout) {
            return;
        }
        if remaining.len() >= text.len() {
            if !remaining.starts_with(text.as_str()) {
                return;
            }
            remaining = &remaining[text.len()..];
        } else {
            if !text.starts_with(remaining) {
                return;
            }
            remaining = "";
        }
    }

    if !remaining.is_empty() {
        return;
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
            _ => return,
        };

        if remove_current {
            contents.remove(idx);
            continue;
        }
        idx = idx.saturating_add(1);
    }
}

#[cfg(feature = "pager")]
fn trim_echo_prefix_in_output(
    output: &OutputBuffer,
    echo_prefix: Option<&str>,
    trim_enabled: bool,
) -> bool {
    if !trim_enabled {
        return false;
    }
    let Some(echo_prefix) = echo_prefix else {
        return false;
    };
    if echo_prefix.is_empty() {
        return false;
    }
    let start_offset = output.current_offset().unwrap_or(0);
    let end_offset = output.end_offset().unwrap_or(start_offset);
    let prefix_len = echo_prefix.len() as u64;
    if start_offset.saturating_add(prefix_len) > end_offset {
        return false;
    }
    let range = output.read_range(start_offset, start_offset.saturating_add(prefix_len));
    if !range.events.is_empty() {
        return false;
    }
    if range.bytes != echo_prefix.as_bytes() {
        return false;
    }
    output.advance_offset_to(start_offset.saturating_add(prefix_len));
    true
}

#[cfg(feature = "pager")]
fn drop_echo_only_output(
    output: &OutputBuffer,
    start_offset: u64,
    end_offset: u64,
    echo: &str,
) -> bool {
    if echo.is_empty() {
        return false;
    }
    let total_len = end_offset.saturating_sub(start_offset);
    let echo_len = echo.len() as u64;
    if total_len != echo_len {
        return false;
    }
    let range = output.read_range(start_offset, end_offset);
    if !range.events.is_empty() {
        return false;
    }
    if range.bytes != echo.as_bytes() {
        return false;
    }
    output.advance_offset_to(end_offset);
    true
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
        "<<console status: busy, write_stdin timeout reached; elapsed_ms={elapsed_ms}>>"
    ))
}

fn idle_status_content() -> WorkerContent {
    WorkerContent::server_stdout("<<console status: idle>>")
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

struct WorkerProcess {
    child: Child,
    stdin_tx: mpsc::Sender<StdinCommand>,
    session_tmpdir: Option<PathBuf>,
    ipc: IpcHandle,
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
    #[cfg(target_os = "macos")]
    denial_logger: Option<crate::sandbox::DenialLogger>,
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
        pending_output_tape: PendingOutputTape,
        #[cfg(feature = "pager")] output_timeline: OutputTimeline,
        guardrail: GuardrailShared,
    ) -> Result<Self, WorkerError> {
        #[cfg(not(target_family = "unix"))]
        let _ = &guardrail;

        let mut ipc_server = IpcServer::bind().map_err(WorkerError::Io)?;
        let SpawnedWorker {
            child,
            stdin_tx,
            session_tmpdir,
            #[cfg(target_os = "macos")]
            denial_logger,
        } = match backend {
            Backend::R => Self::spawn_r_worker(
                exe_path,
                sandbox_state,
                pending_output_tape.clone(),
                #[cfg(feature = "pager")]
                output_timeline.clone(),
                &mut ipc_server,
            )?,
            Backend::Python => Self::spawn_python_worker(
                sandbox_state,
                pending_output_tape.clone(),
                #[cfg(feature = "pager")]
                output_timeline.clone(),
                &mut ipc_server,
            )?,
        };
        #[allow(unused_mut)]
        let mut child = child;

        let ipc = IpcHandle::new();
        #[cfg(any(target_family = "unix", target_family = "windows"))]
        {
            let image_tape = pending_output_tape.clone();
            #[cfg(feature = "pager")]
            let image_timeline = output_timeline.clone();
            let sideband_tape = pending_output_tape.clone();
            let handlers = IpcHandlers {
                on_plot_image: Some(Arc::new(move |image: IpcPlotImage| {
                    #[cfg(feature = "pager")]
                    image_timeline.append_image(
                        image.id.clone(),
                        image.mime_type.clone(),
                        image.data.clone(),
                        image.is_new,
                    );
                    image_tape.append_image(image.id, image.mime_type, image.data, image.is_new);
                })),
                on_readline_start: Some(Arc::new(move |prompt: String| {
                    sideband_tape.append_sideband(PendingSidebandKind::ReadlineStart { prompt });
                })),
                on_readline_result: {
                    let sideband_tape = pending_output_tape.clone();
                    Some(Arc::new(move |event: IpcEchoEvent| {
                        sideband_tape.append_sideband(PendingSidebandKind::ReadlineResult {
                            prompt: event.prompt,
                            line: event.line,
                        });
                    }))
                },
                on_request_end: {
                    let sideband_tape = pending_output_tape.clone();
                    Some(Arc::new(move || {
                        sideband_tape.append_sideband(PendingSidebandKind::RequestEnd);
                    }))
                },
                on_session_end: {
                    let sideband_tape = pending_output_tape.clone();
                    Some(Arc::new(move || {
                        sideband_tape.append_sideband(PendingSidebandKind::SessionEnd);
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
        pending_output_tape: PendingOutputTape,
        #[cfg(feature = "pager")] output_timeline: OutputTimeline,
        ipc_server: &mut IpcServer,
    ) -> Result<SpawnedWorker, WorkerError> {
        let prepared =
            prepare_worker_command(exe_path, vec![WORKER_MODE_ARG.to_string()], sandbox_state)
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
        spawn_output_reader(
            child.stdout.take(),
            TextStream::Stdout,
            pending_output_tape.clone(),
            #[cfg(feature = "pager")]
            output_timeline.clone(),
        );
        spawn_output_reader(
            child.stderr.take(),
            TextStream::Stderr,
            pending_output_tape.clone(),
            #[cfg(feature = "pager")]
            output_timeline.clone(),
        );

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
        pending_output_tape: PendingOutputTape,
        #[cfg(feature = "pager")] output_timeline: OutputTimeline,
        ipc_server: &mut IpcServer,
    ) -> Result<SpawnedWorker, WorkerError> {
        #[cfg(not(target_family = "unix"))]
        {
            let _ = sandbox_state;
            let _ = pending_output_tape;
            #[cfg(feature = "pager")]
            let _ = output_timeline;
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
            spawn_output_reader(
                Some(master_reader),
                TextStream::Stdout,
                pending_output_tape.clone(),
                #[cfg(feature = "pager")]
                output_timeline.clone(),
            );

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

    fn send_sigterm(&mut self) -> Result<(), WorkerError> {
        #[cfg(target_family = "unix")]
        {
            self.send_signal(libc::SIGTERM)
        }
        #[cfg(not(target_family = "unix"))]
        {
            request_soft_termination(&mut self.child)
        }
    }

    fn send_sigkill(&mut self) -> Result<(), WorkerError> {
        #[cfg(target_family = "unix")]
        {
            self.send_signal(libc::SIGKILL)
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
        let result = unsafe { libc::kill(-pid, signal) };
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
    fn kill_process_tree_scan(&self, signal: i32) {
        let pid = self.child.id() as i32;
        let root = Pid::from_u32(pid as u32);
        let mut system = System::new();
        system.refresh_processes(ProcessesToUpdate::All, true);
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

        for pid in seen {
            let _ = unsafe { libc::kill(pid.as_u32() as i32, signal) };
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
            let _sig_ok = self.send_sigterm().is_ok();
            #[cfg(target_family = "unix")]
            if !_sig_ok {
                self.kill_process_tree_scan(libc::SIGTERM);
            }
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
                    let _sig_ok = self.send_sigkill().is_ok();
                    #[cfg(target_family = "unix")]
                    if !_sig_ok {
                        self.kill_process_tree_scan(libc::SIGKILL);
                    }
                    let _ = self.child.wait();
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        }

        self.cleanup_session_tmpdir();
        self.report_denials();
        Ok(())
    }

    fn kill(mut self) -> Result<(), WorkerError> {
        let _sig_ok = self.send_sigkill().is_ok();
        #[cfg(target_family = "unix")]
        if !_sig_ok {
            self.kill_process_tree_scan(libc::SIGKILL);
        }
        let _ = self.child.wait();
        self.cleanup_session_tmpdir();
        self.report_denials();
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

    let mut total_kb: u64 = 0;
    let mut pids = Vec::new();
    for pid in seen {
        if let Some(process) = system.process(pid) {
            total_kb = total_kb.saturating_add(process.memory());
            pids.push(pid);
        }
    }
    (total_kb, pids)
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

fn spawn_output_reader<R>(
    stream: Option<R>,
    output_stream: TextStream,
    pending_output_tape: PendingOutputTape,
    #[cfg(feature = "pager")] output_timeline: OutputTimeline,
) where
    R: Read + Send + 'static,
{
    let Some(mut stream) = stream else {
        return;
    };
    thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => {
                    break;
                }
                Ok(n) => match output_stream {
                    TextStream::Stdout => {
                        #[cfg(feature = "pager")]
                        output_timeline.append_text(&buffer[..n], false);
                        pending_output_tape.append_stdout_bytes(&buffer[..n]);
                    }
                    TextStream::Stderr => {
                        #[cfg(feature = "pager")]
                        output_timeline.append_text(&buffer[..n], true);
                        pending_output_tape.append_stderr_bytes(&buffer[..n]);
                    }
                },
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });
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
    use crate::sandbox::SandboxPolicy;
    use std::sync::{Mutex, OnceLock};

    fn cwd_test_mutex() -> &'static Mutex<()> {
        static TEST_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
        TEST_MUTEX.get_or_init(|| Mutex::new(()))
    }

    fn env_test_mutex() -> &'static Mutex<()> {
        static TEST_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
        TEST_MUTEX.get_or_init(|| Mutex::new(()))
    }

    #[cfg(feature = "pager")]
    fn echo_event(prompt: &str, line: &str) -> IpcEchoEvent {
        IpcEchoEvent {
            prompt: prompt.to_string(),
            line: line.to_string(),
        }
    }

    #[cfg(feature = "pager")]
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

    #[cfg(feature = "pager")]
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

    #[cfg(feature = "pager")]
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

    #[cfg(feature = "pager")]
    #[test]
    fn trim_decision_respects_continuation_prompts() {
        let single = vec![echo_event("> ", "1+1\n")];
        assert!(should_trim_echo_prefix(&single));

        let continuation = vec![echo_event("> ", "1+\n"), echo_event("+ ", "1\n")];
        assert!(should_trim_echo_prefix(&continuation));

        let multi = vec![echo_event("> ", "1+1\n"), echo_event("> ", "2+2\n")];
        assert!(!should_trim_echo_prefix(&multi));
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
    fn completion_waits_for_request_end_event() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");
        driver_on_input_start("1+1", &server);
        let prompt = "> ".to_string();
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: prompt.clone(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineResult {
            prompt: prompt.clone(),
            line: "1+1\n".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: prompt.clone(),
        });

        let result = driver_wait_for_completion(Duration::from_millis(75), server.clone());
        assert!(
            matches!(result, Err(WorkerError::Timeout(_))),
            "expected timeout before request-end"
        );

        let _ = worker.send(WorkerToServerIpcMessage::RequestEnd);
        let completion = driver_wait_for_completion(Duration::from_millis(200), server)
            .expect("expected completion after request-end");
        assert_eq!(completion.prompt.as_deref(), Some("> "));
    }

    #[cfg(feature = "pager")]
    #[test]
    fn completion_settle_waits_for_late_echo_events() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");
        driver_on_input_start("1+\n1", &server);
        let prompt = "> ".to_string();
        let delayed_worker = worker.clone();

        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: prompt.clone(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::RequestEnd);

        let late_sender = thread::spawn(move || {
            thread::sleep(Duration::from_millis(1));
            let _ = delayed_worker.send(WorkerToServerIpcMessage::ReadlineResult {
                prompt: "> ".to_string(),
                line: "1+\n".to_string(),
            });
            thread::sleep(Duration::from_millis(11));
            let _ = delayed_worker.send(WorkerToServerIpcMessage::ReadlineResult {
                prompt: "+ ".to_string(),
                line: "1\n".to_string(),
            });
        });

        let completion = driver_wait_for_completion(Duration::from_millis(200), server)
            .expect("expected completion after request-end");
        late_sender.join().expect("late sender should join");

        assert_eq!(completion.prompt.as_deref(), Some("> "));
        assert_eq!(completion.echo_events.len(), 2);
        assert_eq!(completion.echo_events[0].prompt, "> ");
        assert_eq!(completion.echo_events[0].line, "1+\n");
        assert_eq!(completion.echo_events[1].prompt, "+ ");
        assert_eq!(completion.echo_events[1].line, "1\n");
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

        assert_eq!(
            manager.detached_prefix_item_count(),
            1,
            "detached-prefix metadata must survive reset until server-side finalization"
        );
        let WorkerReply::Output { .. } = reply;
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
                    codex_linux_sandbox_exe: None,
                    use_linux_sandbox_bwrap: None,
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
}
