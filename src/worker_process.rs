#[cfg(test)]
use base64::Engine as _;
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::Duration;

use crate::backend::{Backend, WorkerLaunch};
use crate::completion_reply::{
    CompletionInfo, CompletionReplyMode, InputContext, InputFallback, ReplyWithOffset,
    build_completed_reply, build_timeout_reply, idle_status_content, stdin_wait_status_content,
    timeout_status_content,
};
use crate::ipc::{IpcEchoEvent, IpcWaitError, ServerIpcConnection, ServerToWorkerIpcMessage};
use crate::output_capture::{
    OUTPUT_RING_CAPACITY_BYTES, OutputBuffer, OutputTextSource, OutputTimeline, ensure_output_ring,
    reset_last_reply_marker_offset, reset_output_ring, set_last_reply_marker_offset,
    update_last_reply_marker_offset_max,
};
use crate::output_snapshot::{
    SnapshotWithImages, snapshot_after_completion, snapshot_page_with_images,
    take_range_from_ring_after_completion,
};
#[cfg(test)]
use crate::output_timeline::{EchoCollapseMode, collapse_echo_with_attribution};
use crate::oversized_output::OversizedOutputMode;
use crate::pager::{self, Pager};
use crate::pending_output_tape::{FormattedPendingOutput, PendingOutputTape, PendingSidebandKind};
#[cfg(test)]
use crate::reply_presentation::trim_echo_prefix_after_leading_nonstdout_contents;
use crate::reply_presentation::{
    append_prompt_if_missing, build_input_transcript, drop_echo_only_contents,
    echo_transcript_from_events, fallback_prompt_variants, maybe_trim_echo_prefix,
    normalize_prompt, reconcile_polled_completion_prompt, reconcile_trailing_completion_prompt,
    should_drop_echo_only_contents, should_trim_echo_prefix, strip_trailing_prompt,
    trim_echo_then_append_protocol_warnings, trim_leading_input_echo_from_contents,
    trim_matching_echo_event_suffix_from_contents,
};
use crate::sandbox::{SandboxState, SandboxStateUpdate};
use crate::sandbox_cli::{
    MISSING_INHERITED_SANDBOX_STATE_MESSAGE, SandboxCliPlan,
    resolve_effective_sandbox_state_with_defaults, sandbox_plan_requests_inherited_state,
    validate_sandbox_plan_with_defaults,
};
use crate::stdin_payload::prepare_worker_stdin_payload;
pub(crate) use crate::stdin_payload::{WriteStdinControlAction, split_write_stdin_control_prefix};
use crate::worker_protocol::{ContentOrigin, WorkerContent, WorkerErrorCode, WorkerReply};
#[cfg(target_os = "linux")]
use crate::worker_supervisor::linux_sandbox_startup_retryable;
use crate::worker_supervisor::{
    GuardrailEvent, GuardrailShared, InitialWorkerPrompt, SupervisorSpawn, WorkerProcess,
    WorkerSpawnContext, WorkerSupervisor,
};

const WORKER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(target_os = "linux")]
const LINUX_BWRAP_FALLBACK_NOTICE: &str =
    "[repl] Linux bubblewrap sandbox unavailable; continuing without bwrap\n";
pub(crate) const PREVIOUS_IMAGE_UPDATE_NOTICE: &str =
    "[repl] image update from previous request shown as a new image\n";
const PRECHECKED_FOLLOW_UP_REQUIRES_META_MESSAGE: &str =
    "worker follow-up needs current sandbox metadata after precheck";
fn output_echo_source_for_backend(backend: Backend) -> OutputTextSource {
    match backend {
        Backend::R => OutputTextSource::Ipc,
        Backend::Python => OutputTextSource::Ipc,
    }
}

#[cfg(any(test, target_os = "windows"))]
fn backend_prepares_windows_sandbox_launch(backend: Backend) -> bool {
    matches!(backend, Backend::R | Backend::Python)
}

#[derive(Debug)]
pub enum WorkerError {
    Io(std::io::Error),
    Protocol(String),
    Timeout(Duration),
    Sandbox(String),
    Guardrail(String),
}

pub(crate) fn is_prechecked_follow_up_requires_meta(err: &WorkerError) -> bool {
    matches!(err, WorkerError::Protocol(message) if message == PRECHECKED_FOLLOW_UP_REQUIRES_META_MESSAGE)
}

fn prechecked_follow_up_requires_meta_error() -> WorkerError {
    WorkerError::Protocol(PRECHECKED_FOLLOW_UP_REQUIRES_META_MESSAGE.to_string())
}

trait BackendDriver: Send {
    fn prepare_input_text(&self, text: String) -> String {
        text
    }

    fn prepare_input_payload(&self, text: &str) -> Vec<u8> {
        prepare_worker_stdin_payload(text)
    }

    fn on_input_start(
        &mut self,
        text: &str,
        payload: &[u8],
        ipc: &ServerIpcConnection,
        timeout: Duration,
    ) -> Result<(), WorkerError>;
    fn on_input_written(&mut self, _ipc: &ServerIpcConnection) -> Result<(), WorkerError> {
        Ok(())
    }
    fn should_settle_output_after_timeout(
        &self,
        oversized_output: OversizedOutputMode,
        pending_input: Option<&str>,
    ) -> bool;
    fn should_write_stdin_payload(&self) -> bool {
        true
    }
    fn clear_active_turn(&mut self) {}
    fn wait_for_completion(
        &mut self,
        timeout: Duration,
        ipc: ServerIpcConnection,
    ) -> Result<CompletionInfo, WorkerError>;
    fn interrupt(&mut self, process: &mut WorkerProcess) -> Result<(), WorkerError>;
}

struct RBackendDriver;

impl RBackendDriver {
    fn new() -> Self {
        Self
    }
}

#[cfg_attr(
    any(target_family = "unix", target_family = "windows"),
    allow(dead_code)
)]
fn driver_on_input_start(_text: &str, ipc: &ServerIpcConnection) -> Result<(), WorkerError> {
    ipc.begin_request();
    if let Some(message) = ipc.take_protocol_error() {
        return Err(WorkerError::Protocol(message));
    }
    Ok(())
}

const REQUEST_COMPLETION_STABLE_WAIT: Duration = Duration::from_millis(20);
fn driver_wait_for_completion(
    timeout: Duration,
    ipc: ServerIpcConnection,
    echo_source: OutputTextSource,
) -> Result<CompletionInfo, WorkerError> {
    if timeout.is_zero() {
        return Err(WorkerError::Timeout(timeout));
    }
    match ipc.wait_for_request_completion(timeout, REQUEST_COMPLETION_STABLE_WAIT) {
        Ok(()) => Ok(completion_info_from_ipc(&ipc, false, echo_source)),
        Err(IpcWaitError::Timeout) => Err(WorkerError::Timeout(timeout)),
        Err(IpcWaitError::SessionEnd) => Ok(completion_info_from_ipc(&ipc, true, echo_source)),
        Err(IpcWaitError::Disconnected) => Err(WorkerError::Protocol(
            "ipc disconnected while waiting for request completion".to_string(),
        )),
        Err(IpcWaitError::Protocol(message)) => Err(WorkerError::Protocol(message)),
    }
}

#[cfg(not(any(target_family = "unix", target_family = "windows")))]
fn driver_interrupt(process: &mut WorkerProcess) -> Result<(), WorkerError> {
    if let Some(ipc) = process.ipc_connection() {
        let _ = ipc.send(ServerToWorkerIpcMessage::Interrupt { turn_id: None });
    }
    process.send_interrupt()
}

fn normalize_input_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

impl BackendDriver for RBackendDriver {
    fn prepare_input_text(&self, text: String) -> String {
        normalize_input_newlines(&text)
    }

    fn on_input_start(
        &mut self,
        _text: &str,
        payload: &[u8],
        ipc: &ServerIpcConnection,
        _timeout: Duration,
    ) -> Result<(), WorkerError> {
        ipc.begin_request_with_stdin(payload);
        if let Some(message) = ipc.take_protocol_error() {
            return Err(WorkerError::Protocol(message));
        }
        Ok(())
    }

    fn should_settle_output_after_timeout(
        &self,
        oversized_output: OversizedOutputMode,
        pending_input: Option<&str>,
    ) -> bool {
        if !matches!(oversized_output, OversizedOutputMode::Files) {
            return false;
        }
        pending_input
            .map(|input| input.trim_end_matches(['\r', '\n']).contains('\n'))
            .unwrap_or(false)
    }

    fn wait_for_completion(
        &mut self,
        timeout: Duration,
        ipc: ServerIpcConnection,
    ) -> Result<CompletionInfo, WorkerError> {
        driver_wait_for_completion(timeout, ipc, OutputTextSource::Ipc)
    }

    fn interrupt(&mut self, process: &mut WorkerProcess) -> Result<(), WorkerError> {
        if let Some(ipc) = process.ipc_connection() {
            let _ = ipc.send(ServerToWorkerIpcMessage::Interrupt { turn_id: None });
        }
        process.send_r_interrupt()
    }
}

struct ProtocolBackendDriver {
    next_turn_id: u64,
    active_turn_id: Option<u64>,
    is_builtin_python: bool,
}

impl ProtocolBackendDriver {
    fn new() -> Self {
        Self {
            next_turn_id: 1,
            active_turn_id: None,
            is_builtin_python: false,
        }
    }

    fn builtin_python() -> Self {
        Self {
            next_turn_id: 1,
            active_turn_id: None,
            is_builtin_python: true,
        }
    }

    fn next_turn_id(&mut self) -> u64 {
        let turn_id = self.next_turn_id;
        self.next_turn_id = self.next_turn_id.wrapping_add(1).max(1);
        turn_id
    }
}

impl BackendDriver for ProtocolBackendDriver {
    fn on_input_start(
        &mut self,
        text: &str,
        payload: &[u8],
        ipc: &ServerIpcConnection,
        timeout: Duration,
    ) -> Result<(), WorkerError> {
        let _ = payload;
        if let Some(message) = ipc.take_protocol_error() {
            return Err(WorkerError::Protocol(message));
        }
        let turn_id = self.next_turn_id();
        ipc.begin_turn(turn_id);
        match ipc.send_with_timeout(
            ServerToWorkerIpcMessage::TurnStart {
                turn_id,
                input: text.to_string(),
            },
            timeout,
        ) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::TimedOut => {
                return Err(WorkerError::Timeout(timeout));
            }
            Err(err) => return Err(WorkerError::Io(err)),
        }
        self.active_turn_id = Some(turn_id);
        if let Some(message) = ipc.take_protocol_error() {
            return Err(WorkerError::Protocol(message));
        }
        Ok(())
    }

    fn on_input_written(&mut self, ipc: &ServerIpcConnection) -> Result<(), WorkerError> {
        let _ = ipc;
        Ok(())
    }

    fn should_settle_output_after_timeout(
        &self,
        _oversized_output: OversizedOutputMode,
        _pending_input: Option<&str>,
    ) -> bool {
        false
    }

    fn should_write_stdin_payload(&self) -> bool {
        false
    }

    fn clear_active_turn(&mut self) {
        self.active_turn_id = None;
    }

    fn wait_for_completion(
        &mut self,
        timeout: Duration,
        ipc: ServerIpcConnection,
    ) -> Result<CompletionInfo, WorkerError> {
        let result = driver_wait_for_completion(timeout, ipc, OutputTextSource::Ipc);
        if matches!(result, Ok(_) | Err(WorkerError::Protocol(_))) {
            self.active_turn_id = None;
        }
        result
    }

    fn interrupt(&mut self, process: &mut WorkerProcess) -> Result<(), WorkerError> {
        if let Some(ipc) = process.ipc_connection() {
            if let Some(turn_id) = self.active_turn_id {
                ipc.send(ServerToWorkerIpcMessage::Interrupt {
                    turn_id: Some(turn_id),
                })
                .map_err(WorkerError::Io)?;
            } else if self.is_builtin_python {
                ipc.send(ServerToWorkerIpcMessage::Interrupt { turn_id: None })
                    .map_err(WorkerError::Io)?;
            }
        }
        process.send_interrupt()
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

const COMPLETION_METADATA_SETTLE_MAX: Duration = Duration::from_millis(30);
const COMPLETION_METADATA_SETTLE_POLL: Duration = Duration::from_millis(5);
const COMPLETION_METADATA_STABLE: Duration = Duration::from_millis(10);
const OUTPUT_READER_QUIESCE_GRACE: Duration = Duration::from_millis(120);
const OUTPUT_READER_COMPLETION_STABLE: Duration = if cfg!(target_os = "macos") {
    Duration::from_millis(80)
} else {
    Duration::from_millis(15)
};
const OUTPUT_READER_TIMEOUT_SETTLE_MAX: Duration = Duration::from_millis(900);

fn collect_completion_metadata(ipc: &ServerIpcConnection) -> (Option<String>, Vec<String>) {
    let mut prompt = ipc.try_take_prompt();
    let mut prompt_variants = ipc.take_prompt_history();
    let mut echo_event_count = ipc.pending_echo_event_count();
    let mut saw_late_echo_event = false;

    let start = std::time::Instant::now();
    let mut stable_for = Duration::from_millis(0);
    while start.elapsed() < COMPLETION_METADATA_SETTLE_MAX {
        thread::sleep(COMPLETION_METADATA_SETTLE_POLL);
        let next_prompt = ipc.try_take_prompt();
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

#[derive(Default)]
struct PrefixCapture {
    contents: Vec<WorkerContent>,
    is_error: bool,
    bytes: u64,
}

struct RequestState {
    timeout: Duration,
    started_at: std::time::Instant,
}

fn completion_info_from_ipc(
    ipc: &ServerIpcConnection,
    session_end_seen: bool,
    echo_source: OutputTextSource,
) -> CompletionInfo {
    let (prompt, prompt_variants) = if session_end_seen {
        (None, None)
    } else {
        let (prompt, prompt_variants) = collect_completion_metadata(ipc);
        (prompt, Some(prompt_variants))
    };

    let mut echo_events = ipc.take_echo_events();
    for event in &mut echo_events {
        event.source = echo_source;
    }

    CompletionInfo {
        prompt,
        stdin_wait_prompt: ipc.take_stdin_wait_prompt(),
        prompt_variants,
        echo_events,
        protocol_warnings: ipc.take_protocol_warnings(),
        session_end_seen,
    }
}

const DEFERRED_SANDBOX_UPDATE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Default)]
pub(crate) struct WriteStdinOptions {
    pub page_bytes_override: Option<u64>,
    pub echo_input: bool,
    pub pending_state_prechecked: bool,
    pub deferred_sandbox_state_update: Option<SandboxStateUpdate>,
    pub suppress_session_end_reset: bool,
}

impl WriteStdinOptions {
    fn control_tail(&self, deferred_sandbox_state_update: Option<SandboxStateUpdate>) -> Self {
        Self {
            page_bytes_override: self.page_bytes_override,
            echo_input: self.echo_input,
            pending_state_prechecked: false,
            deferred_sandbox_state_update,
            suppress_session_end_reset: false,
        }
    }
}

pub(crate) fn worker_context_event_payload(
    worker_launch: &WorkerLaunch,
    backend: Backend,
    sandbox_state: &SandboxState,
) -> serde_json::Value {
    let sandbox_policy = serde_json::to_value(&sandbox_state.sandbox_policy)
        .unwrap_or_else(|err| serde_json::json!({ "serialize_error": err.to_string() }));
    serde_json::json!({
        "backend": format!("{backend:?}"),
        "worker_launch": worker_launch.label(),
        "stdin_transport": worker_launch.stdin_transport().as_str(),
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
    worker_launch: WorkerLaunch,
    backend: Backend,
    process: Option<WorkerProcess>,
    sandbox_plan: SandboxCliPlan,
    inherited_sandbox_state: Option<SandboxState>,
    sandbox_defaults: SandboxState,
    sandbox_state: SandboxState,
    managed_network_proxy: Option<crate::managed_network::ManagedNetworkProxy>,
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
    settled_pending_error: Option<WorkerError>,
    preserved_detached_prefix: PrefixCapture,
    reply_owned_prefix: PrefixCapture,
    next_live_prefix_belongs_to_reply: bool,
    last_detached_prefix_item_count: usize,
    pager_prompt: Option<String>,
    last_prompt: Option<String>,
    stdin_waiting: bool,
    last_spawn: Option<std::time::Instant>,
    spawn_count: u64,
    guardrail: GuardrailShared,
    pending_server_notice: Option<GuardrailEvent>,
    write_in_progress: bool,
    last_write_respawned: bool,
    #[cfg(target_os = "linux")]
    linux_bwrap_fallback_disabled: bool,
}

struct PreparedSandboxStateUpdate {
    update_for_log: serde_json::Value,
    changed: bool,
    missing_before: bool,
}

impl WorkerManager {
    pub fn new(
        backend: Backend,
        sandbox_plan: SandboxCliPlan,
        oversized_output: OversizedOutputMode,
    ) -> Result<Self, WorkerError> {
        Self::new_with_launch(
            WorkerLaunch::Builtin(backend),
            sandbox_plan,
            oversized_output,
        )
    }

    pub fn new_with_launch(
        worker_launch: WorkerLaunch,
        sandbox_plan: SandboxCliPlan,
        oversized_output: OversizedOutputMode,
    ) -> Result<Self, WorkerError> {
        let exe_path = std::env::current_exe()?;
        let backend = worker_launch.builtin_backend().unwrap_or(Backend::R);
        let sandbox_defaults = crate::sandbox::sandbox_state_defaults_with_environment();
        let plan_requests_inherited_state = sandbox_plan_requests_inherited_state(&sandbox_plan);
        let sandbox_state = if plan_requests_inherited_state {
            validate_sandbox_plan_with_defaults(&sandbox_plan, &sandbox_defaults)
                .map_err(WorkerError::Sandbox)?;
            sandbox_defaults.clone()
        } else {
            resolve_effective_sandbox_state_with_defaults(&sandbox_plan, None, &sandbox_defaults)
                .map_err(WorkerError::Sandbox)?
        };
        if plan_requests_inherited_state {
            crate::event_log::log(
                "worker_manager_created",
                serde_json::json!({
                    "backend": format!("{backend:?}"),
                    "awaiting_initial_tool_call_sandbox_state_meta": true,
                }),
            );
        } else {
            crate::event_log::log_lazy("worker_manager_created", || {
                worker_context_event_payload(&worker_launch, backend, &sandbox_state)
            });
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
            worker_launch: worker_launch.clone(),
            backend,
            process: None,
            sandbox_plan,
            inherited_sandbox_state: None,
            sandbox_defaults,
            sandbox_state,
            managed_network_proxy: None,
            #[cfg(target_os = "windows")]
            windows_sandbox_launch: None,
            oversized_output,
            pending_output_tape: PendingOutputTape::new(),
            output: OutputBuffer::default(),
            pager: Pager::default(),
            output_timeline,
            driver: match worker_launch {
                WorkerLaunch::Builtin(Backend::R) => Box::new(RBackendDriver::new()),
                WorkerLaunch::Builtin(Backend::Python) => {
                    Box::new(ProtocolBackendDriver::builtin_python())
                }
                WorkerLaunch::Custom(_) => Box::new(ProtocolBackendDriver::new()),
            },
            pending_request: false,
            pending_request_started_at: None,
            pending_request_input: None,
            session_end_seen: false,
            settled_pending_completion: None,
            settled_pending_error: None,
            preserved_detached_prefix: PrefixCapture::default(),
            reply_owned_prefix: PrefixCapture::default(),
            next_live_prefix_belongs_to_reply: false,
            last_detached_prefix_item_count: 0,
            pager_prompt: None,
            last_prompt: None,
            stdin_waiting: false,
            last_spawn: None,
            spawn_count: 0,
            guardrail: GuardrailShared {
                event: Arc::new(Mutex::new(None)),
                busy: Arc::new(AtomicBool::new(false)),
            },
            pending_server_notice: None,
            write_in_progress: false,
            last_write_respawned: false,
            #[cfg(target_os = "linux")]
            linux_bwrap_fallback_disabled: false,
        })
    }

    pub fn warm_start(&mut self) -> Result<(), WorkerError> {
        if self.missing_inherited_sandbox_state() {
            return Ok(());
        }
        self.ensure_process()
    }

    fn ensure_managed_network_proxy(&mut self) -> Result<(), WorkerError> {
        let Some(config) = Self::managed_network_proxy_config_for_state(&self.sandbox_state)?
        else {
            self.managed_network_proxy = None;
            return Ok(());
        };

        if self
            .managed_network_proxy
            .as_ref()
            .is_some_and(|proxy| proxy.config() == &config)
        {
            return Ok(());
        }

        let proxy = crate::managed_network::ManagedNetworkProxy::start(config)
            .map_err(|err| WorkerError::Sandbox(err.to_string()))?;
        crate::event_log::log(
            "worker_managed_network_proxy_started",
            serde_json::json!({
                "http_addr": proxy.http_addr().to_string(),
                "socks_addr": proxy.socks_addr().to_string(),
            }),
        );
        self.managed_network_proxy = Some(proxy);
        Ok(())
    }

    fn managed_network_proxy_config_for_state(
        state: &SandboxState,
    ) -> Result<Option<crate::managed_network::ManagedProxyConfig>, WorkerError> {
        if !state.managed_network_policy.has_domain_restrictions() {
            return Ok(None);
        }
        if !state.sandbox_policy.has_full_network_access() {
            return Ok(None);
        }
        if !state.sandbox_policy.requires_sandbox() {
            return Err(WorkerError::Sandbox(
                "managed network domain restrictions require built-in sandbox enforcement"
                    .to_string(),
            ));
        }
        if !cfg!(target_os = "macos") {
            return Err(WorkerError::Sandbox(
                "managed network domain restrictions are currently supported only on macOS"
                    .to_string(),
            ));
        }
        crate::managed_network::ManagedProxyConfig::from_policy(&state.managed_network_policy)
            .map(Some)
            .map_err(|err| WorkerError::Sandbox(err.to_string()))
    }

    pub fn bootstrap_local_inherited_sandbox_state(&mut self) -> Result<bool, WorkerError> {
        if !self.missing_inherited_sandbox_state() {
            return Ok(false);
        }

        let update = SandboxStateUpdate {
            sandbox_policy: self.sandbox_defaults.sandbox_policy.clone(),
            sandbox_cwd: Some(self.sandbox_defaults.sandbox_cwd.clone()),
            use_linux_sandbox_bwrap: Some(self.sandbox_defaults.use_linux_sandbox_bwrap),
            use_legacy_landlock: None,
        };
        crate::event_log::log(
            "worker_local_inherit_bootstrap",
            serde_json::json!({
                "sandbox_policy": update.sandbox_policy.clone(),
                "sandbox_cwd": update.sandbox_cwd.clone(),
                "use_linux_sandbox_bwrap": update.use_linux_sandbox_bwrap,
            }),
        );
        self.stage_sandbox_state_update(update)?;
        Ok(true)
    }

    fn missing_inherited_sandbox_state(&self) -> bool {
        sandbox_plan_requests_inherited_state(&self.sandbox_plan)
            && self.inherited_sandbox_state.is_none()
    }

    /// Exposes whether a timed-out logical request still owns future empty-input polls.
    pub fn pending_request(&self) -> bool {
        self.pending_request
    }

    pub fn refresh_timeout_marker_with_wait(&mut self, wait: Duration) {
        self.resolve_timeout_marker_with_wait(wait);
    }

    pub fn empty_input_requires_spawn(&mut self) -> Result<bool, WorkerError> {
        if self.empty_input_uses_existing_state() {
            return Ok(false);
        }
        let needs_spawn = match self.process.as_mut() {
            Some(process) => !process.is_running()?,
            None => true,
        };
        Ok(needs_spawn)
    }

    pub fn empty_input_polls_existing_output(&self) -> bool {
        match self.oversized_output {
            OversizedOutputMode::Files => {
                self.pending_request
                    || self.pending_output_tape.has_pending()
                    || self.settled_pending_completion.is_some()
            }
            OversizedOutputMode::Pager => {
                self.pending_request
                    || self.output.has_pending_output()
                    || self.settled_pending_completion.is_some()
            }
        }
    }

    pub fn empty_input_uses_local_pager_state(&self) -> bool {
        matches!(self.oversized_output, OversizedOutputMode::Pager)
            && self.pager.is_active()
            && !self.empty_input_polls_existing_output()
    }

    pub fn empty_input_may_auto_reset_after_poll(&self) -> bool {
        self.empty_input_polls_existing_output()
            && (self.pending_request
                || self.settled_pending_completion.is_some()
                || self.session_end_seen)
    }

    pub fn missing_inherited_state_without_worker(&self) -> bool {
        self.missing_inherited_sandbox_state() && self.process.is_none()
    }

    pub fn nonexecuting_follow_up_uses_existing_state(
        &mut self,
        text: &str,
    ) -> Result<bool, WorkerError> {
        if let Some((control, remaining)) = split_write_stdin_control_prefix(text) {
            return match control {
                WriteStdinControlAction::Interrupt => {
                    if remaining.is_empty() {
                        Ok(true)
                    } else {
                        Ok(self.local_pager_follow_up_uses_existing_state(remaining)
                            && !self.control_only_interrupt_requires_spawn()?)
                    }
                }
                WriteStdinControlAction::Restart => Ok(false),
            };
        }

        Ok(self.local_pager_follow_up_uses_existing_state(text))
    }

    fn control_only_interrupt_requires_spawn(&mut self) -> Result<bool, WorkerError> {
        match self.process.as_mut() {
            Some(process) => Ok(!process.is_running()?),
            None => Ok(true),
        }
    }

    pub fn detached_prefix_item_count(&self) -> usize {
        self.last_detached_prefix_item_count
    }

    pub fn respawned_during_last_write(&self) -> bool {
        self.last_write_respawned
    }

    fn note_respawn_during_write(&mut self) {
        if self.write_in_progress {
            self.last_write_respawned = true;
        }
    }

    fn stage_deferred_sandbox_state_update(
        &mut self,
        update: Option<SandboxStateUpdate>,
    ) -> Result<(), WorkerError> {
        let Some(update) = update else {
            return Ok(());
        };
        self.stage_sandbox_state_update(update)
    }

    fn stage_session_end_sandbox_state_update(
        &mut self,
        update: Option<SandboxStateUpdate>,
        pending_state_prechecked: bool,
    ) -> Result<(), WorkerError> {
        if pending_state_prechecked
            && update.is_none()
            && sandbox_plan_requests_inherited_state(&self.sandbox_plan)
        {
            return Err(prechecked_follow_up_requires_meta_error());
        }

        self.stage_deferred_sandbox_state_update(update)
    }

    fn maybe_reset_after_session_end_with_options(
        &mut self,
        deferred_sandbox_state_update: Option<SandboxStateUpdate>,
        suppress_session_end_reset: bool,
        pending_state_prechecked: bool,
    ) -> Result<(), WorkerError> {
        if self.session_end_seen && !suppress_session_end_reset {
            self.stage_session_end_sandbox_state_update(
                deferred_sandbox_state_update,
                pending_state_prechecked,
            )?;
        }
        if !suppress_session_end_reset {
            self.maybe_reset_after_session_end();
        }
        Ok(())
    }

    fn apply_deferred_sandbox_state_update(
        &mut self,
        update: Option<SandboxStateUpdate>,
    ) -> Result<(), WorkerError> {
        let Some(update) = update else {
            return Ok(());
        };
        self.update_sandbox_state(update, DEFERRED_SANDBOX_UPDATE_TIMEOUT)?;
        Ok(())
    }

    fn empty_input_uses_existing_state(&self) -> bool {
        match self.oversized_output {
            OversizedOutputMode::Files => {
                self.pending_request
                    || self.pending_output_tape.has_pending()
                    || self.settled_pending_completion.is_some()
                    || self.guardrail_busy_event_pending()
            }
            OversizedOutputMode::Pager => {
                self.pending_request
                    || self.output.has_pending_output()
                    || self.settled_pending_completion.is_some()
                    || self.pager.is_active()
                    || self.guardrail_busy_event_pending()
            }
        }
    }

    pub(crate) fn local_pager_follow_up_uses_existing_state(&self, text: &str) -> bool {
        matches!(self.oversized_output, OversizedOutputMode::Pager) && self.pager.is_active() && {
            let trimmed = text.trim();
            trimmed.is_empty() || trimmed.starts_with(':')
        }
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
        options: WriteStdinOptions,
    ) -> Result<WorkerReply, WorkerError> {
        self.write_in_progress = true;
        self.last_write_respawned = false;
        let result = match self.oversized_output {
            OversizedOutputMode::Files => {
                self.write_stdin_files(text, worker_timeout, server_timeout, options)
            }
            OversizedOutputMode::Pager => {
                self.write_stdin_pager(text, worker_timeout, server_timeout, options)
            }
        };
        self.write_in_progress = false;
        result
    }

    /// Entry point for the public `repl` tool in default files mode.
    fn write_stdin_files(
        &mut self,
        text: String,
        worker_timeout: Duration,
        server_timeout: Duration,
        options: WriteStdinOptions,
    ) -> Result<WorkerReply, WorkerError> {
        let pending_state_prechecked = options.pending_state_prechecked;
        let deferred_sandbox_state_update = options.deferred_sandbox_state_update.clone();
        let suppress_session_end_reset = options.suppress_session_end_reset;
        self.last_detached_prefix_item_count = 0;
        if let Some((control, remaining)) = split_write_stdin_control_prefix(&text) {
            self.clear_guardrail_busy_event();
            let control_requires_spawn = matches!(control, WriteStdinControlAction::Interrupt)
                && self.control_only_interrupt_requires_spawn()?;
            if pending_state_prechecked
                && control_requires_spawn
                && deferred_sandbox_state_update.is_none()
                && !suppress_session_end_reset
            {
                return Err(prechecked_follow_up_requires_meta_error());
            }
            let stage_before_control =
                control_requires_spawn || matches!(control, WriteStdinControlAction::Restart);
            let stage_interrupt_after_session_end =
                matches!(control, WriteStdinControlAction::Interrupt) && !stage_before_control;
            let mut tail_sandbox_state_update = if stage_before_control {
                self.stage_deferred_sandbox_state_update(deferred_sandbox_state_update.clone())?;
                None
            } else {
                deferred_sandbox_state_update
            };
            let control_reply = match control {
                WriteStdinControlAction::Interrupt if stage_interrupt_after_session_end => {
                    self.interrupt_files(worker_timeout, None, true)
                }
                WriteStdinControlAction::Interrupt => self.interrupt_files(
                    worker_timeout,
                    tail_sandbox_state_update.clone(),
                    suppress_session_end_reset,
                ),
                WriteStdinControlAction::Restart => self.restart_files(worker_timeout),
            }?;
            if stage_interrupt_after_session_end
                && self.session_end_seen
                && !suppress_session_end_reset
            {
                self.stage_session_end_sandbox_state_update(
                    tail_sandbox_state_update.take(),
                    pending_state_prechecked,
                )?;
                self.maybe_reset_after_session_end();
            }
            if remaining.is_empty() {
                return Ok(control_reply);
            }
            let control_prefix_item_count = prefixed_worker_reply_item_count(&control_reply);
            let remaining_reply = self.write_stdin_files(
                remaining.to_string(),
                worker_timeout,
                server_timeout,
                options.control_tail(tail_sandbox_state_update),
            )?;
            self.last_detached_prefix_item_count += control_prefix_item_count;
            return Ok(prefix_worker_reply(control_reply, remaining_reply));
        }

        if self.guardrail_busy_event_pending() {
            // Don't execute new input; the previous request was aborted.
            self.maybe_emit_guardrail_notice();
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
            self.maybe_reset_after_session_end_with_options(None, false, false)?;
            return Ok(reply);
        }

        let empty_input = text.is_empty();
        self.maybe_emit_guardrail_notice();
        if !pending_state_prechecked {
            self.resolve_timeout_marker();
        }
        if empty_input {
            if self.pending_request
                || self.pending_output_tape.has_pending()
                || self.settled_pending_completion.is_some()
            {
                let reply = self.poll_pending_output_files(worker_timeout)?;
                let reply = self.finalize_reply(reply);
                self.maybe_reset_after_session_end_with_options(
                    deferred_sandbox_state_update,
                    suppress_session_end_reset,
                    pending_state_prechecked,
                )?;
                return Ok(reply);
            }
            if pending_state_prechecked && self.control_only_interrupt_requires_spawn()? {
                return Err(prechecked_follow_up_requires_meta_error());
            }
            if let Err(err) = self.ensure_process() {
                let input_context = self.prepare_input_context_files();
                let reply = self.build_reply_from_worker_error_files(&err, input_context);
                let reply = self.finalize_reply(reply);
                self.maybe_reset_after_session_end_with_options(None, false, false)?;
                return Ok(reply);
            }
            let reply = self.build_idle_poll_reply_files();
            let reply = self.finalize_reply(reply);
            self.maybe_reset_after_session_end_with_options(None, false, false)?;
            return Ok(reply);
        }
        if !pending_state_prechecked && self.pending_request {
            self.resolve_timeout_marker_with_wait(Duration::from_millis(25));
        }
        if self.pending_request {
            let mut reply = self.poll_pending_output_files(worker_timeout)?;
            let detached_prefix_item_count = match &reply.reply {
                WorkerReply::Output { contents, .. } => contents.len(),
            };
            self.last_detached_prefix_item_count = detached_prefix_item_count;
            mark_busy_follow_up_reply(&mut reply.reply);
            let reply = self.finalize_reply(reply);
            self.maybe_reset_after_session_end_with_options(
                deferred_sandbox_state_update,
                suppress_session_end_reset,
                pending_state_prechecked,
            )?;
            return Ok(reply);
        }
        self.apply_deferred_sandbox_state_update(deferred_sandbox_state_update)?;
        if let Err(err) = self.ensure_process() {
            let input_context = self.prepare_input_context_files();
            let reply = self.build_reply_from_worker_error_files(&err, input_context);
            let reply = self.finalize_reply(reply);
            self.maybe_reset_after_session_end_with_options(None, false, false)?;
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
        self.maybe_reset_after_session_end_with_options(None, false, false)?;
        Ok(reply)
    }

    fn write_stdin_pager(
        &mut self,
        text: String,
        worker_timeout: Duration,
        server_timeout: Duration,
        options: WriteStdinOptions,
    ) -> Result<WorkerReply, WorkerError> {
        let page_bytes_override = options.page_bytes_override;
        let echo_input = options.echo_input;
        let pending_state_prechecked = options.pending_state_prechecked;
        let deferred_sandbox_state_update = options.deferred_sandbox_state_update.clone();
        let suppress_session_end_reset = options.suppress_session_end_reset;
        self.last_detached_prefix_item_count = 0;
        if let Some((control, remaining)) = split_write_stdin_control_prefix(&text) {
            self.clear_guardrail_busy_event();
            let control_requires_spawn = matches!(control, WriteStdinControlAction::Interrupt)
                && self.control_only_interrupt_requires_spawn()?;
            if pending_state_prechecked
                && control_requires_spawn
                && deferred_sandbox_state_update.is_none()
                && !suppress_session_end_reset
            {
                return Err(prechecked_follow_up_requires_meta_error());
            }
            let stage_before_control =
                control_requires_spawn || matches!(control, WriteStdinControlAction::Restart);
            let stage_interrupt_after_session_end =
                matches!(control, WriteStdinControlAction::Interrupt) && !stage_before_control;
            let mut tail_sandbox_state_update = if stage_before_control {
                self.stage_deferred_sandbox_state_update(deferred_sandbox_state_update.clone())?;
                None
            } else {
                deferred_sandbox_state_update
            };
            let control_reply = match control {
                WriteStdinControlAction::Interrupt if stage_interrupt_after_session_end => {
                    self.interrupt_pager(worker_timeout, None, true)
                }
                WriteStdinControlAction::Interrupt => self.interrupt_pager(
                    worker_timeout,
                    tail_sandbox_state_update.clone(),
                    suppress_session_end_reset,
                ),
                WriteStdinControlAction::Restart => self.restart_pager(worker_timeout),
            }?;
            if stage_interrupt_after_session_end
                && self.session_end_seen
                && !suppress_session_end_reset
            {
                self.stage_session_end_sandbox_state_update(
                    tail_sandbox_state_update.take(),
                    pending_state_prechecked,
                )?;
                self.maybe_reset_after_session_end();
            }
            if remaining.is_empty() {
                return Ok(control_reply);
            }
            let control_prefix_item_count = prefixed_worker_reply_item_count(&control_reply);
            let remaining_reply = self.write_stdin_pager(
                remaining.to_string(),
                worker_timeout,
                server_timeout,
                options.control_tail(tail_sandbox_state_update),
            )?;
            self.last_detached_prefix_item_count += control_prefix_item_count;
            return Ok(prefix_worker_reply(control_reply, remaining_reply));
        }

        if self.guardrail_busy_event_pending() {
            self.maybe_emit_guardrail_notice();
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
            self.maybe_reset_after_session_end_with_options(None, false, false)?;
            return Ok(reply);
        }

        let page_bytes = pager::resolve_page_bytes(page_bytes_override);
        let empty_input = text.is_empty();
        if !empty_input && self.pager.is_active() {
            let trimmed = text.trim();
            if trimmed.is_empty() || trimmed.starts_with(':') {
                if let Some(reply) = self.handle_pager_command(&text) {
                    let reply = self.finalize_reply(reply);
                    self.maybe_reset_after_session_end_with_options(None, true, false)?;
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
            if !pending_state_prechecked {
                self.resolve_timeout_marker();
            }
            if self.pending_request
                || self.output.has_pending_output()
                || self.settled_pending_completion.is_some()
            {
                let reply = self.poll_pending_output_pager(worker_timeout, page_bytes)?;
                let reply = self.finalize_reply(reply);
                self.maybe_reset_after_session_end_with_options(
                    deferred_sandbox_state_update,
                    suppress_session_end_reset,
                    pending_state_prechecked,
                )?;
                return Ok(reply);
            }
            if self.pager.is_active()
                && let Some(reply) = self.handle_pager_command(&text)
            {
                let reply = self.finalize_reply(reply);
                self.maybe_reset_after_session_end_with_options(None, true, false)?;
                return Ok(reply);
            }
            if pending_state_prechecked && self.control_only_interrupt_requires_spawn()? {
                return Err(prechecked_follow_up_requires_meta_error());
            }
        }

        if let Err(err) = self.ensure_process() {
            let input_context = self.prepare_input_context_pager(&text, echo_input);
            let reply = self.build_reply_from_worker_error_pager(&err, input_context, page_bytes);
            let reply = self.finalize_reply(reply);
            self.maybe_reset_after_session_end_with_options(None, false, false)?;
            return Ok(reply);
        }
        if !empty_input {
            self.output.start_capture();
            self.maybe_emit_guardrail_notice();
            if !pending_state_prechecked {
                self.resolve_timeout_marker();
            }
        }
        if empty_input {
            let reply = self.build_idle_poll_reply_pager();
            let reply = self.finalize_reply(reply);
            self.maybe_reset_after_session_end_with_options(None, false, false)?;
            return Ok(reply);
        }
        if !pending_state_prechecked && self.pending_request {
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
            self.maybe_reset_after_session_end_with_options(
                deferred_sandbox_state_update,
                suppress_session_end_reset,
                pending_state_prechecked,
            )?;
            return Ok(reply);
        }
        self.apply_deferred_sandbox_state_update(deferred_sandbox_state_update)?;

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
        self.maybe_reset_after_session_end_with_options(None, false, false)?;
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
        let mut consumed_completion = false;
        let mut completion = CompletionInfo::empty();

        if let Some(err) = self.settled_pending_error.take() {
            let _ = self.reset_preserving_detached_prefix_item_count();
            return Err(err);
        }
        if self.pending_request {
            match self.wait_for_request_completion(timeout) {
                Ok(info) => {
                    self.clear_pending_request_state();
                    if info.session_end_seen {
                        self.note_session_end(true);
                    }
                    completion = info;
                    completed_request = true;
                    consumed_completion = true;
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
                        consumed_completion = true;
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
            if info.session_end_seen {
                self.note_session_end(false);
            }
            completion = info;
            consumed_completion = true;
        }
        let fallback_input = if !timed_out && consumed_completion {
            self.take_input_fallback(&completion)
        } else {
            InputFallback::default()
        };
        if !timed_out && consumed_completion {
            self.wait_for_late_files_output_after_settled_completion(timeout);
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
            return Ok(build_timeout_reply(contents, is_error, 0));
        }

        let session_end = completion.session_end_seen;
        let built = build_completed_reply(
            contents,
            is_error,
            0,
            &completion,
            session_end,
            CompletionReplyMode::Files {
                fallback_input,
                idle_status_if_empty: true,
            },
            self.backend,
        );
        self.remember_prompt(built.prompt_to_remember.clone());
        Ok(built.reply)
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
        let mut completion = CompletionInfo::empty();

        if let Some(err) = self.settled_pending_error.take() {
            let preserve_pager = self.pager.is_active();
            let _ = self.reset_with_pager_preserving_detached_prefix_item_count(preserve_pager);
            return Err(err);
        }
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
            if info.session_end_seen {
                self.note_session_end(false);
            }
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
                &completion.echo_events,
                completion.prompt_variants.as_deref(),
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

        if timed_out {
            return Ok(build_timeout_reply(contents, is_error, end_offset));
        }

        let session_end = completion.session_end_seen;
        let built = build_completed_reply(
            contents,
            is_error,
            end_offset,
            &completion,
            session_end,
            CompletionReplyMode::Pager {
                pager_active: self.pager.is_active(),
                fallback_input_transcript: None,
            },
            self.backend,
        );
        self.remember_prompt(built.prompt_to_remember.clone());
        if let Some(pager_prompt) = built.pager_prompt {
            self.pager_prompt = pager_prompt;
        }
        Ok(built.reply)
    }

    /// Drains detached output that arrived before the next accepted request so it can be prefixed
    /// into that request's visible reply.
    fn prepare_input_context_files(&mut self) -> InputContext {
        let reply_prefix = self.take_current_prefix_files();
        let (detached_prefix, reply_prefix) = self.take_prefixes_for_next_request(reply_prefix);
        InputContext {
            detached_prefix_contents: detached_prefix.contents,
            reply_prefix_contents: reply_prefix.contents,
            prefix_is_error: detached_prefix.is_error || reply_prefix.is_error,
            start_offset: 0,
            prefix_bytes: 0,
            input_echo: None,
            input_transcript: None,
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
        let input_transcript = build_input_transcript(prompt_hint.as_deref(), text);
        let reply_prefix = self.take_current_prefix_pager(had_pending_output);
        let (detached_prefix, reply_prefix) = self.take_prefixes_for_next_request(reply_prefix);

        let start_offset = self.output.end_offset().unwrap_or(0);
        if input_echo.is_none() && (echo_input || saw_background_output || had_pending_output) {
            input_echo = pager::build_input_echo(text);
        }

        InputContext {
            detached_prefix_contents: detached_prefix.contents,
            reply_prefix_contents: reply_prefix.contents,
            prefix_is_error: detached_prefix.is_error || reply_prefix.is_error,
            start_offset,
            prefix_bytes: detached_prefix.bytes.saturating_add(reply_prefix.bytes),
            input_echo,
            input_transcript,
        }
    }

    fn take_current_prefix_files(&mut self) -> PrefixCapture {
        let settled_completion = self.settled_pending_completion.take();
        let fallback_input = settled_completion
            .as_ref()
            .map(|completion| self.take_input_fallback(completion))
            .unwrap_or_default();
        let fallback_input_transcript = fallback_input.transcript.clone();
        // A new accepted request seals the detached prefix. Flush any incomplete UTF-8 tail now
        // so it stays with the detached transcript instead of merging into fresh request output.
        let FormattedPendingOutput {
            mut contents,
            saw_stderr,
        } = self.drain_sealed_formatted_output();
        if let Some(completion) = settled_completion.as_ref() {
            let has_fallback_input_transcript = fallback_input_transcript.is_some();
            let trim_enabled = if completion.echo_events.is_empty() {
                has_fallback_input_transcript
            } else {
                should_trim_echo_prefix(&completion.echo_events)
            };
            let echo_transcript = echo_transcript_from_events(&completion.echo_events)
                .or(fallback_input_transcript.clone());
            trim_echo_then_append_protocol_warnings(
                &mut contents,
                echo_transcript.as_deref(),
                trim_enabled,
                if completion.echo_events.is_empty() {
                    has_fallback_input_transcript
                } else {
                    should_drop_echo_only_contents(&completion.echo_events)
                },
                &completion.protocol_warnings,
            );
            if !trim_enabled {
                let _ = trim_matching_echo_event_suffix_from_contents(
                    &mut contents,
                    &completion.echo_events,
                );
            }
            if completion.echo_events.is_empty() && fallback_input_transcript.is_none() {
                let prompt_variants = fallback_prompt_variants(
                    completion.prompt.as_deref(),
                    completion.prompt_variants.as_deref(),
                );
                let _ = trim_leading_input_echo_from_contents(
                    &mut contents,
                    fallback_input.raw_input.as_deref(),
                    &prompt_variants,
                );
            }
        }
        PrefixCapture {
            contents,
            is_error: saw_stderr,
            bytes: 0,
        }
    }

    fn take_current_prefix_pager(&mut self, had_pending_output: bool) -> PrefixCapture {
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
                    &completion.echo_events,
                    completion.prompt_variants.as_deref(),
                    &completion.protocol_warnings,
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

        PrefixCapture {
            contents: prefix_contents,
            is_error: prefix_is_error,
            bytes: prefix_bytes,
        }
    }

    fn send_worker_request(
        &mut self,
        text: String,
        worker_timeout: Duration,
        server_timeout: Duration,
    ) -> Result<RequestState, WorkerError> {
        let text = self.driver.prepare_input_text(text);
        let started_at = std::time::Instant::now();
        let prompt = self.current_prompt_hint();
        self.remember_prompt(prompt);
        self.pending_request_input = Some(text.clone());
        let ipc = self
            .process
            .as_ref()
            .and_then(|process| process.ipc_connection())
            .ok_or_else(|| WorkerError::Protocol("worker ipc unavailable".to_string()))?;
        if server_timeout.is_zero() {
            return Err(WorkerError::Timeout(server_timeout));
        }
        let server_deadline = started_at + server_timeout;
        let remaining = server_deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(WorkerError::Timeout(server_timeout));
        }
        let payload = self.driver.prepare_input_payload(&text);
        self.driver
            .on_input_start(&text, &payload, &ipc, remaining)?;
        self.settled_pending_completion = None;
        self.guardrail.busy.store(true, Ordering::Relaxed);
        let remaining = server_deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(WorkerError::Timeout(server_timeout));
        }
        if self.driver.should_write_stdin_payload() {
            self.process
                .as_mut()
                .expect("worker process should be available")
                .write_stdin_payload(payload, remaining)?;
            self.driver.on_input_written(&ipc)?;
        }
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
        self.last_detached_prefix_item_count = context.detached_prefix_contents.len();
        let mut contents = context.detached_prefix_contents;
        contents.extend(context.reply_prefix_contents);
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
        self.last_detached_prefix_item_count = context.detached_prefix_contents.len();
        let end_offset = self.output.end_offset().unwrap_or(context.start_offset);
        let first_page_budget = page_bytes.saturating_sub(context.prefix_bytes);
        let mut contents = context.detached_prefix_contents;
        contents.extend(context.reply_prefix_contents);
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
        self.last_detached_prefix_item_count = context.detached_prefix_contents.len();
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
                let mut contents = context.detached_prefix_contents;
                contents.extend(context.reply_prefix_contents);
                let formatted = self.drain_final_formatted_output();
                let is_error = context.prefix_is_error || formatted.saw_stderr;
                contents.extend(formatted.contents);
                let fallback_input = self.take_input_fallback(&completion);
                let built = build_completed_reply(
                    contents,
                    is_error,
                    0,
                    &completion,
                    session_end,
                    CompletionReplyMode::Files {
                        fallback_input,
                        idle_status_if_empty: false,
                    },
                    self.backend,
                );
                self.remember_prompt(built.prompt_to_remember.clone());
                self.guardrail.busy.store(false, Ordering::Relaxed);
                Ok(built.reply)
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

                if self.should_settle_output_after_timeout() {
                    self.settle_output_after_timeout();
                }
                self.pending_request = true;
                self.pending_request_started_at = Some(request.started_at);
                let mut contents = context.detached_prefix_contents;
                contents.extend(context.reply_prefix_contents);
                let formatted = self.drain_formatted_output();
                contents.extend(formatted.contents);

                contents.push(timeout_status_content(request.started_at.elapsed()));

                let is_error = context.prefix_is_error || formatted.saw_stderr;

                Ok(build_timeout_reply(contents, is_error, 0))
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
        self.last_detached_prefix_item_count = context.detached_prefix_contents.len();
        match self.wait_for_request_completion(request.timeout) {
            Ok(completion) => {
                let fallback_input_transcript = context.input_transcript.clone();
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
                let mut contents = context.detached_prefix_contents;
                contents.extend(context.reply_prefix_contents);
                if let Some(echo) = context.input_echo {
                    contents.push(WorkerContent::stdout(echo));
                }
                let completion_snapshot = snapshot_after_completion(
                    &self.output,
                    context.start_offset,
                    end_offset,
                    first_page_budget,
                    &completion.echo_events,
                    completion.prompt_variants.as_deref(),
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
                let built = build_completed_reply(
                    contents,
                    is_error,
                    end_offset,
                    &completion,
                    session_end,
                    CompletionReplyMode::Pager {
                        pager_active: self.pager.is_active(),
                        fallback_input_transcript,
                    },
                    self.backend,
                );
                self.remember_prompt(built.prompt_to_remember.clone());
                if let Some(pager_prompt) = built.pager_prompt {
                    self.pager_prompt = pager_prompt;
                }
                self.guardrail.busy.store(false, Ordering::Relaxed);
                Ok(built.reply)
            }
            Err(WorkerError::Timeout(_)) => {
                let fallback_input_transcript = context.input_transcript.clone();
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
                let mut contents = context.detached_prefix_contents;
                contents.extend(context.reply_prefix_contents);
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
                maybe_trim_echo_prefix(&mut contents, fallback_input_transcript.as_deref(), true);
                if let Some(echo) = fallback_input_transcript.as_deref() {
                    let _ = drop_echo_only_contents(&mut contents, echo);
                }

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

                Ok(build_timeout_reply(contents, is_error, end_offset))
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
            .ipc_connection()
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
                    stdin_wait_prompt: None,
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
        if result.is_ok() {
            self.pending_output_tape
                .append_sideband(PendingSidebandKind::RequestBoundary);
        }
        self.settle_output_after_completion(remaining);
        if result.is_ok()
            && let Some(message) = ipc.take_protocol_error()
        {
            return Err(WorkerError::Protocol(message));
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

    fn settle_output_after_completion(&self, budget: Duration) {
        let total = budget.min(OUTPUT_READER_QUIESCE_GRACE);
        if total.is_zero() {
            return;
        }
        let stable_needed = OUTPUT_READER_COMPLETION_STABLE.min(total);
        self.settle_output_until_stable(total, stable_needed);
    }

    fn wait_for_late_files_output_after_settled_completion(&self, budget: Duration) {
        if self.pending_output_tape.has_pending() {
            return;
        }
        let total = budget.min(OUTPUT_READER_TIMEOUT_SETTLE_MAX);
        if total.is_zero() {
            return;
        }

        let poll = Duration::from_millis(5);
        let start = std::time::Instant::now();
        while start.elapsed() < total {
            let remaining = total.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                break;
            }
            thread::sleep(poll.min(remaining));
            if self.pending_output_tape.has_pending() {
                self.settle_output_after_completion(total.saturating_sub(start.elapsed()));
                return;
            }
        }
    }

    fn settle_output_after_timeout(&self) {
        let total = OUTPUT_READER_TIMEOUT_SETTLE_MAX;
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

    fn should_settle_output_after_timeout(&self) -> bool {
        self.driver.should_settle_output_after_timeout(
            self.oversized_output,
            self.pending_request_input.as_deref(),
        )
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
        self.maybe_emit_pending_server_notice();
        let event = {
            let mut slot = self
                .guardrail
                .event
                .lock()
                .expect("guardrail event mutex poisoned");
            if slot.as_ref().is_some_and(|event| event.was_busy) {
                return;
            }
            slot.take()
        };
        let Some(event) = event else {
            return;
        };
        self.append_server_notice(event);
    }

    fn maybe_emit_pending_server_notice(&mut self) {
        let Some(event) = self.pending_server_notice.take() else {
            return;
        };
        self.append_server_notice(event);
    }

    fn append_server_notice(&mut self, event: GuardrailEvent) {
        match self.oversized_output {
            OversizedOutputMode::Files => {
                if event.is_error {
                    self.pending_output_tape
                        .append_server_stderr_bytes(event.message.as_bytes());
                } else {
                    self.pending_output_tape
                        .append_stdout_status_line(event.message.as_bytes());
                }
            }
            OversizedOutputMode::Pager => {
                self.output_timeline.append_text(
                    event.message.as_bytes(),
                    event.is_error,
                    ContentOrigin::Server,
                );
            }
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
        self.stdin_waiting = false;
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
            let result = match self.oversized_output {
                OversizedOutputMode::Files => self.reset_preserving_detached_prefix_item_count(),
                OversizedOutputMode::Pager => self
                    .reset_with_pager_preserving_detached_prefix_item_count(self.pager.is_active()),
            };
            if result.is_ok() {
                self.note_respawn_during_write();
            }
            self.session_end_seen = false;
        }
    }

    pub fn interrupt(
        &mut self,
        timeout: Duration,
        deferred_sandbox_state_update: Option<SandboxStateUpdate>,
        suppress_session_end_reset: bool,
    ) -> Result<WorkerReply, WorkerError> {
        match self.oversized_output {
            OversizedOutputMode::Files => self.interrupt_files(
                timeout,
                deferred_sandbox_state_update,
                suppress_session_end_reset,
            ),
            OversizedOutputMode::Pager => self.interrupt_pager(
                timeout,
                deferred_sandbox_state_update,
                suppress_session_end_reset,
            ),
        }
    }

    fn interrupt_target_running(&mut self) -> Result<bool, WorkerError> {
        match self.process.as_mut() {
            Some(process) => process.is_running(),
            None => Ok(false),
        }
    }

    fn interrupt_files(
        &mut self,
        timeout: Duration,
        deferred_sandbox_state_update: Option<SandboxStateUpdate>,
        suppress_session_end_reset: bool,
    ) -> Result<WorkerReply, WorkerError> {
        crate::event_log::log(
            "worker_interrupt_begin",
            serde_json::json!({
                "timeout_ms": timeout.as_millis(),
            }),
        );
        let interrupt_drains_existing_completion =
            self.pending_request || self.settled_pending_completion.is_some();
        let should_interrupt_worker = self.interrupt_target_running()?;
        if should_interrupt_worker {
            let process = self
                .process
                .as_mut()
                .expect("worker process should be available");
            let interrupt_result = self.driver.interrupt(process);
            if let Err(err) = interrupt_result {
                self.reset()?;
                crate::event_log::log(
                    "worker_interrupt_error",
                    serde_json::json!({
                        "error": err.to_string(),
                    }),
                );
                return Err(err);
            }
        }

        if interrupt_drains_existing_completion {
            let mut reply = self.poll_pending_output_files(timeout)?;
            let prompt = match &reply.reply {
                WorkerReply::Output { prompt, .. } => prompt.clone(),
            };
            let WorkerReply::Output { contents, .. } = &mut reply.reply;
            reconcile_polled_completion_prompt(contents, prompt, self.backend);
            let reply = self.finalize_reply(reply);
            self.maybe_reset_after_session_end_with_options(
                deferred_sandbox_state_update,
                suppress_session_end_reset,
                false,
            )?;
            return Ok(reply);
        }

        let mut timed_out = false;
        let mut prompt: Option<String> = None;
        if let Some(process) = self.process.as_ref()
            && let Some(ipc) = process.ipc_connection()
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
                Err(IpcWaitError::Protocol(message)) => return Err(WorkerError::Protocol(message)),
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
        let raw_prompt = if session_end || timed_out {
            None
        } else {
            prompt.clone()
        };
        let resolved_prompt = normalize_prompt(raw_prompt.clone());
        self.remember_prompt(raw_prompt);
        if !session_end && !timed_out {
            reconcile_trailing_completion_prompt(
                &mut contents,
                resolved_prompt.clone(),
                self.backend,
            );
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
        let reply = self.finalize_reply(ReplyWithOffset {
            reply,
            end_offset: 0,
        });
        self.maybe_reset_after_session_end_with_options(
            deferred_sandbox_state_update,
            suppress_session_end_reset,
            false,
        )?;
        Ok(reply)
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
        if self.missing_inherited_sandbox_state() {
            return Err(WorkerError::Sandbox(
                MISSING_INHERITED_SANDBOX_STATE_MESSAGE.to_string(),
            ));
        }
        self.maybe_emit_pending_server_notice();
        let pre_shutdown_output = self
            .process
            .is_some()
            .then(|| self.drain_sealed_formatted_output());
        if let Some(process) = self.process.take() {
            let _ = process.shutdown_graceful(timeout);
            self.pending_output_tape.clear();
        }
        self.guardrail.busy.store(false, Ordering::Relaxed);

        let reply = match pre_shutdown_output {
            Some(output) => {
                self.build_session_reset_reply_files_from_formatted("new session started", output)
            }
            None => self.build_session_reset_reply_files("new session started"),
        };
        self.clear_preserved_prefixes();
        self.reset_output_state_files(true);
        self.note_respawn_during_write();
        crate::event_log::log("worker_restart_end", serde_json::json!({"status": "ok"}));
        Ok(self.finalize_reply(reply))
    }

    fn interrupt_pager(
        &mut self,
        timeout: Duration,
        deferred_sandbox_state_update: Option<SandboxStateUpdate>,
        suppress_session_end_reset: bool,
    ) -> Result<WorkerReply, WorkerError> {
        crate::event_log::log(
            "worker_interrupt_begin",
            serde_json::json!({
                "timeout_ms": timeout.as_millis(),
            }),
        );
        let interrupt_drains_existing_completion =
            self.pending_request || self.settled_pending_completion.is_some();
        let should_interrupt_worker = self.interrupt_target_running()?;
        if should_interrupt_worker {
            let process = self
                .process
                .as_mut()
                .expect("worker process should be available");
            let interrupt_result = self.driver.interrupt(process);
            if let Err(err) = interrupt_result {
                self.reset()?;
                crate::event_log::log(
                    "worker_interrupt_error",
                    serde_json::json!({
                        "error": err.to_string(),
                    }),
                );
                return Err(err);
            }
        }

        let page_bytes = pager::resolve_page_bytes(None);
        if interrupt_drains_existing_completion {
            let mut reply = self.poll_pending_output_pager(timeout, page_bytes)?;
            let pager_active = self.pager.is_active();
            let prompt = match &reply.reply {
                WorkerReply::Output { prompt, .. } => prompt.clone(),
            };
            let WorkerReply::Output { contents, .. } = &mut reply.reply;
            if !pager_active {
                reconcile_polled_completion_prompt(contents, prompt, self.backend);
            }
            let reply = self.finalize_reply(reply);
            self.maybe_reset_after_session_end_with_options(
                deferred_sandbox_state_update,
                suppress_session_end_reset,
                false,
            )?;
            return Ok(reply);
        }

        let mut timed_out = false;
        let mut prompt: Option<String> = None;
        if let Some(process) = self.process.as_ref()
            && let Some(ipc) = process.ipc_connection()
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
                Err(IpcWaitError::Protocol(message)) => return Err(WorkerError::Protocol(message)),
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
        let raw_prompt = if session_end || timed_out {
            None
        } else {
            prompt.clone()
        };
        let resolved_prompt = normalize_prompt(raw_prompt.clone());
        self.remember_prompt(raw_prompt);
        if self.pager.is_active() && !session_end {
            self.pager_prompt = resolved_prompt.clone();
        }
        if !session_end && !timed_out && !self.pager.is_active() {
            reconcile_trailing_completion_prompt(
                &mut contents,
                resolved_prompt.clone(),
                self.backend,
            );
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
        let reply = self.finalize_reply(ReplyWithOffset { reply, end_offset });
        self.maybe_reset_after_session_end_with_options(
            deferred_sandbox_state_update,
            suppress_session_end_reset,
            false,
        )?;
        Ok(reply)
    }

    fn restart_pager(&mut self, timeout: Duration) -> Result<WorkerReply, WorkerError> {
        crate::event_log::log(
            "worker_restart_begin",
            serde_json::json!({
                "timeout_ms": timeout.as_millis(),
            }),
        );
        if self.missing_inherited_sandbox_state() {
            return Err(WorkerError::Sandbox(
                MISSING_INHERITED_SANDBOX_STATE_MESSAGE.to_string(),
            ));
        }
        self.maybe_emit_pending_server_notice();
        let pre_shutdown_end_offset = self
            .process
            .is_some()
            .then(|| self.output.end_offset().unwrap_or(0));
        if let Some(process) = self.process.take() {
            let _ = process.shutdown_graceful(timeout);
        }
        let post_shutdown_end_offset = self.output.end_offset();
        self.guardrail.busy.store(false, Ordering::Relaxed);

        let page_bytes = pager::resolve_page_bytes(None);
        let reply = match pre_shutdown_end_offset {
            Some(end_offset) => {
                let reply = self.build_session_reset_reply_pager_to_offset(
                    page_bytes,
                    "new session started",
                    end_offset,
                );
                if let Some(end_offset) = post_shutdown_end_offset {
                    self.output.advance_offset_to(end_offset);
                }
                reply
            }
            None => self.build_session_reset_reply_pager(page_bytes, "new session started"),
        };
        self.clear_preserved_prefixes();
        self.reset_output_state_pager(true, false);
        self.note_respawn_during_write();
        crate::event_log::log("worker_restart_end", serde_json::json!({"status": "ok"}));
        Ok(self.finalize_reply(reply))
    }

    pub fn shutdown(&mut self) {
        crate::event_log::log("worker_shutdown", serde_json::json!({}));
        if let Some(process) = self.process.take() {
            let _ = process.shutdown_graceful(WORKER_SHUTDOWN_TIMEOUT);
        }
        self.guardrail.busy.store(false, Ordering::Relaxed);
    }

    fn ensure_process(&mut self) -> Result<(), WorkerError> {
        if self.missing_inherited_sandbox_state() {
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
        if self.missing_inherited_sandbox_state() {
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
        if self.missing_inherited_sandbox_state() {
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

    // Replaces the inherited sandbox snapshot for this tool call. The new
    // policy becomes effective in the worker only after the current process is
    // recycled and a replacement worker is spawned. Session temp handling is
    // separate: the server-owned temp dir is reset before each spawn, and
    // today that reset reuses the same configured path in place.
    fn prepare_sandbox_state_update(
        &mut self,
        update: SandboxStateUpdate,
    ) -> Result<PreparedSandboxStateUpdate, WorkerError> {
        let update_for_log = serde_json::to_value(&update)
            .unwrap_or_else(|err| serde_json::json!({"serialize_error": err.to_string()}));
        crate::sandbox::log_sandbox_policy_update(&update.sandbox_policy);
        let mut inherited_state = self.sandbox_defaults.clone();
        inherited_state.apply_update(update);
        #[cfg(target_os = "linux")]
        self.apply_linux_bwrap_fallback_override(&mut inherited_state);
        let resolved_state = resolve_effective_sandbox_state_with_defaults(
            &self.sandbox_plan,
            Some(&inherited_state),
            &self.sandbox_defaults,
        )
        .map_err(WorkerError::Sandbox)?;
        #[cfg(target_os = "linux")]
        let resolved_state = {
            let mut resolved_state = resolved_state;
            self.apply_linux_bwrap_fallback_override(&mut resolved_state);
            resolved_state
        };
        let missing_before = self.missing_inherited_sandbox_state();
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
        Ok(PreparedSandboxStateUpdate {
            update_for_log,
            changed,
            missing_before,
        })
    }

    #[cfg(target_os = "linux")]
    fn apply_linux_bwrap_fallback_override(&self, state: &mut SandboxState) {
        if self.linux_bwrap_fallback_disabled {
            state.use_linux_sandbox_bwrap = false;
        }
    }

    fn log_sandbox_state_update(
        prepared: &PreparedSandboxStateUpdate,
        timeout: Option<Duration>,
        respawned: bool,
    ) {
        crate::event_log::log(
            "worker_sandbox_state_update",
            serde_json::json!({
                "changed": prepared.changed,
                "timeout_ms": timeout.map(|timeout| timeout.as_millis()),
                "respawned": respawned,
                "update": prepared.update_for_log,
            }),
        );
    }

    pub fn stage_sandbox_state_update(
        &mut self,
        update: SandboxStateUpdate,
    ) -> Result<(), WorkerError> {
        let prepared = self.prepare_sandbox_state_update(update)?;
        Self::log_sandbox_state_update(&prepared, None, false);
        Ok(())
    }

    pub fn update_sandbox_state(
        &mut self,
        update: SandboxStateUpdate,
        timeout: Duration,
    ) -> Result<bool, WorkerError> {
        let prepared = self.prepare_sandbox_state_update(update)?;
        let mut respawned = false;
        if !prepared.changed {
            if prepared.missing_before && self.process.is_none() {
                match self.oversized_output {
                    OversizedOutputMode::Files => self.reset_output_state_files(true),
                    OversizedOutputMode::Pager => self.reset_output_state_pager(true, false),
                }
                self.process = Some(match self.oversized_output {
                    OversizedOutputMode::Files => self.spawn_process_files()?,
                    OversizedOutputMode::Pager => self.spawn_process_with_pager(false)?,
                });
                respawned = true;
                self.note_respawn_during_write();
            }
            Self::log_sandbox_state_update(&prepared, Some(timeout), respawned);
            return Ok(respawned);
        }

        let aborted_request = self.pending_request;
        let had_prior_session = self.last_spawn.is_some();
        if let Some(process) = self.process.take() {
            let _ = process.shutdown_graceful(timeout);
        }
        match self.oversized_output {
            OversizedOutputMode::Files if self.has_detached_output_to_preserve() => {
                self.reset_output_state_files_preserving_detached_output()
            }
            OversizedOutputMode::Files => self.reset_output_state_files(true),
            OversizedOutputMode::Pager if self.has_detached_output_to_preserve() => {
                self.reset_output_state_pager_preserving_detached_output(self.pager.is_active())
            }
            OversizedOutputMode::Pager => self.reset_output_state_pager(true, false),
        }
        self.process = Some(match self.oversized_output {
            OversizedOutputMode::Files => self.spawn_process_files()?,
            OversizedOutputMode::Pager => self.spawn_process_with_pager(false)?,
        });
        respawned = true;
        self.note_respawn_during_write();
        if had_prior_session {
            self.stage_sandbox_change_restart_notice(aborted_request);
            self.next_live_prefix_belongs_to_reply = true;
        }
        Self::log_sandbox_state_update(&prepared, Some(timeout), respawned);
        Ok(respawned)
    }

    fn stage_sandbox_change_restart_notice(&mut self, aborted_request: bool) {
        let policy = serde_json::to_string(&self.sandbox_state.sandbox_policy)
            .unwrap_or_else(|err| format!("{{\"serialize_error\":\"{}\"}}", err));
        let mut message = String::from("[repl] sandbox policy changed; new session started\n");
        if aborted_request {
            message.push_str("[repl] previous request aborted because sandbox policy changed\n");
        }
        message.push_str(&format!("[repl] new sandbox policy: {policy}\n"));
        let event = GuardrailEvent {
            message,
            was_busy: false,
            is_error: false,
        };
        match &mut self.pending_server_notice {
            Some(pending) => pending.message.push_str(&event.message),
            None => self.pending_server_notice = Some(event),
        }
    }

    fn has_detached_output_to_preserve(&self) -> bool {
        match self.oversized_output {
            OversizedOutputMode::Files => {
                self.pending_output_tape.has_pending() || self.settled_pending_completion.is_some()
            }
            OversizedOutputMode::Pager => {
                self.output.has_pending_output() || self.settled_pending_completion.is_some()
            }
        }
    }

    fn reset_output_state_files(&mut self, clear_pending_output: bool) {
        self.reset_output_state_files_inner(clear_pending_output, false);
    }

    fn reset_output_state_files_preserving_detached_output(&mut self) {
        self.seed_aborted_files_completion_for_respawn();
        let prefix = self.take_current_prefix_files();
        self.stage_prefix_before_respawn(prefix);
        self.reset_output_state_files_inner(true, false);
    }

    fn seed_aborted_files_completion_for_respawn(&mut self) {
        if !self.pending_request
            || self.settled_pending_completion.is_some()
            || self.pending_request_input.is_none()
        {
            return;
        }

        let prompt = self.last_prompt.clone();
        self.settled_pending_completion = Some(CompletionInfo {
            prompt: prompt.clone(),
            stdin_wait_prompt: None,
            prompt_variants: prompt.clone().map(|prompt| vec![prompt]),
            echo_events: Vec::new(),
            protocol_warnings: Vec::new(),
            session_end_seen: false,
        });
    }

    fn reset_output_state_files_inner(
        &mut self,
        clear_pending_output: bool,
        preserve_detached_output: bool,
    ) {
        if clear_pending_output {
            self.pending_output_tape.clear();
        }
        self.pending_request = false;
        self.pending_request_started_at = None;
        if !preserve_detached_output {
            self.pending_request_input = None;
        }
        self.driver.clear_active_turn();
        self.session_end_seen = false;
        if !preserve_detached_output {
            self.settled_pending_completion = None;
            self.settled_pending_error = None;
            self.last_detached_prefix_item_count = 0;
        }
        self.last_prompt = None;
        self.stdin_waiting = false;
        self.guardrail.busy.store(false, Ordering::Relaxed);
    }

    fn reset_output_state_pager(&mut self, clear_pending_output: bool, preserve_pager: bool) {
        self.reset_output_state_pager_inner(clear_pending_output, preserve_pager, false);
    }

    fn reset_output_state_pager_preserving_detached_output(&mut self, preserve_pager: bool) {
        self.seed_aborted_pager_completion_for_respawn();
        let had_pending_output = self.output.has_pending_output();
        let prefix = self.take_current_prefix_pager(had_pending_output);
        self.stage_prefix_before_respawn(prefix);
        self.reset_output_state_pager_inner(true, preserve_pager, false);
    }

    fn seed_aborted_pager_completion_for_respawn(&mut self) {
        if !self.pending_request
            || self.settled_pending_completion.is_some()
            || self.pending_request_input.is_none()
        {
            return;
        }

        let prompt = self.last_prompt.clone();
        let prompt_variants = prompt.clone().map(|prompt| vec![prompt]);
        let echo_events = match (prompt, self.pending_request_input.clone()) {
            (Some(prompt), Some(line)) => vec![IpcEchoEvent {
                prompt,
                line,
                source: output_echo_source_for_backend(self.backend),
            }],
            _ => Vec::new(),
        };
        self.settled_pending_completion = Some(CompletionInfo {
            prompt: self.last_prompt.clone(),
            stdin_wait_prompt: None,
            prompt_variants,
            echo_events,
            protocol_warnings: Vec::new(),
            session_end_seen: false,
        });
    }

    fn reset_output_state_pager_inner(
        &mut self,
        clear_pending_output: bool,
        preserve_pager: bool,
        preserve_detached_output: bool,
    ) {
        if clear_pending_output {
            self.pending_output_tape.clear();
        }
        if !preserve_detached_output {
            reset_output_ring();
            reset_last_reply_marker_offset();
            self.output = OutputBuffer::default();
        }
        if !preserve_pager {
            self.pager = Pager::default();
        }
        self.pending_request = false;
        self.pending_request_started_at = None;
        self.pending_request_input = None;
        self.driver.clear_active_turn();
        self.session_end_seen = false;
        if !preserve_detached_output {
            self.settled_pending_completion = None;
            self.settled_pending_error = None;
            self.last_detached_prefix_item_count = 0;
        }
        self.pager_prompt = None;
        self.last_prompt = None;
        self.stdin_waiting = false;
        self.guardrail.busy.store(false, Ordering::Relaxed);
    }

    fn append_prefix_capture(target: &mut PrefixCapture, mut prefix: PrefixCapture) {
        if prefix.contents.is_empty() {
            prefix.bytes = 0;
        }
        if prefix.contents.is_empty() && !prefix.is_error {
            return;
        }
        target.is_error |= prefix.is_error;
        target.bytes = target
            .bytes
            .saturating_add(prefix_worker_text_bytes(&prefix.contents));
        target.contents.append(&mut prefix.contents);
    }

    fn take_prefixes_for_next_request(
        &mut self,
        current_prefix: PrefixCapture,
    ) -> (PrefixCapture, PrefixCapture) {
        let mut detached_prefix = std::mem::take(&mut self.preserved_detached_prefix);
        let mut reply_prefix = std::mem::take(&mut self.reply_owned_prefix);
        if self.next_live_prefix_belongs_to_reply {
            Self::append_prefix_capture(&mut reply_prefix, current_prefix);
        } else {
            Self::append_prefix_capture(&mut detached_prefix, current_prefix);
        }
        self.next_live_prefix_belongs_to_reply = false;
        (detached_prefix, reply_prefix)
    }

    fn stage_prefix_before_respawn(&mut self, prefix: PrefixCapture) {
        if self.next_live_prefix_belongs_to_reply {
            Self::append_prefix_capture(&mut self.reply_owned_prefix, prefix);
            self.next_live_prefix_belongs_to_reply = false;
        } else {
            Self::append_prefix_capture(&mut self.preserved_detached_prefix, prefix);
        }
    }

    fn clear_preserved_prefixes(&mut self) {
        self.preserved_detached_prefix = PrefixCapture::default();
        self.reply_owned_prefix = PrefixCapture::default();
        self.next_live_prefix_belongs_to_reply = false;
    }

    fn remember_prompt(&mut self, prompt: Option<String>) {
        if prompt.as_deref() == Some("") {
            self.stdin_waiting = true;
            return;
        }
        let prompt = normalize_prompt(prompt);
        if let Some(prompt) = prompt {
            self.stdin_waiting = false;
            self.last_prompt = Some(prompt);
        }
    }

    fn current_prompt_hint(&self) -> Option<String> {
        if self.stdin_waiting {
            return None;
        }
        let prompt = self
            .process
            .as_ref()
            .and_then(|process| process.ipc_connection())
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
        if self.stdin_waiting {
            return ReplyWithOffset {
                reply: WorkerReply::Output {
                    contents: vec![stdin_wait_status_content()],
                    is_error: false,
                    error_code: None,
                    prompt: None,
                    prompt_variants: None,
                },
                end_offset: 0,
            };
        }
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
        if self.stdin_waiting {
            return ReplyWithOffset {
                reply: WorkerReply::Output {
                    contents: vec![stdin_wait_status_content()],
                    is_error: false,
                    error_code: None,
                    prompt: None,
                    prompt_variants: None,
                },
                end_offset: self.output.end_offset().unwrap_or(0),
            };
        }
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
                match self.spawn_process(false, false) {
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
        self.spawn_process(false, false)
    }

    fn spawn_process_with_pager(
        &mut self,
        preserve_pager: bool,
    ) -> Result<WorkerProcess, WorkerError> {
        #[cfg(target_os = "linux")]
        {
            loop {
                match self.spawn_process(true, preserve_pager) {
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
        self.spawn_process(true, preserve_pager)
    }

    fn spawn_process(
        &mut self,
        pager_spawn: bool,
        preserve_pager: bool,
    ) -> Result<WorkerProcess, WorkerError> {
        self.ensure_managed_network_proxy()?;
        #[cfg(target_os = "windows")]
        let prepared_windows_launch = self.ensure_windows_sandbox_launch()?;
        let SupervisorSpawn {
            process,
            initial_prompt,
        } = WorkerSupervisor::spawn(
            self.worker_launch.clone(),
            &self.exe_path,
            self.backend,
            &self.sandbox_state,
            WorkerSpawnContext {
                oversized_output: self.oversized_output,
                pending_output_tape: self.pending_output_tape.clone(),
                output_timeline: self.output_timeline.clone(),
                guardrail: self.guardrail.clone(),
                managed_network_proxy: self.managed_network_proxy.as_ref(),
                #[cfg(target_os = "windows")]
                prepared_windows_launch,
            },
        )?;
        self.remember_spawned_initial_prompt(initial_prompt);
        self.record_spawn();
        let payload = if pager_spawn {
            serde_json::json!({
                "backend": format!("{:?}", self.backend),
                "spawn_count": self.spawn_count,
                "preserve_pager": preserve_pager,
            })
        } else {
            serde_json::json!({
                "backend": format!("{:?}", self.backend),
                "spawn_count": self.spawn_count,
            })
        };
        crate::event_log::log("worker_spawn_end", payload);
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

        self.linux_bwrap_fallback_disabled = true;
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

    fn remember_spawned_initial_prompt(&mut self, initial_prompt: Option<InitialWorkerPrompt>) {
        match initial_prompt {
            Some(InitialWorkerPrompt::Immediate(raw_prompt)) => {
                self.remember_prompt(Some(raw_prompt));
            }
            Some(InitialWorkerPrompt::Waited(raw_prompt)) => {
                if let Some(prompt) = normalize_prompt(Some(raw_prompt)) {
                    self.last_prompt = Some(prompt);
                }
            }
            None => {}
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
        if !backend_prepares_windows_sandbox_launch(self.backend)
            || !self.sandbox_state.sandbox_policy.requires_sandbox()
        {
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
            worker_context_event_payload(&self.worker_launch, self.backend, &self.sandbox_state)
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

    pub fn refresh_timeout_marker(&mut self) {
        self.resolve_timeout_marker();
    }

    fn resolve_timeout_marker_with_wait(&mut self, wait: Duration) {
        if !self.pending_request {
            return;
        }
        if self.settled_pending_error.is_some() {
            return;
        }
        let Some(ipc) = self
            .process
            .as_ref()
            .and_then(|process| process.ipc_connection())
        else {
            return;
        };
        let status = if wait.is_zero() {
            ipc.wait_for_request_completion(Duration::ZERO, REQUEST_COMPLETION_STABLE_WAIT)
        } else {
            ipc.wait_for_request_completion(wait, REQUEST_COMPLETION_STABLE_WAIT)
        };
        match status {
            Ok(()) => {
                let mut settled_completion = completion_info_from_ipc(
                    &ipc,
                    false,
                    output_echo_source_for_backend(self.backend),
                );
                self.pending_output_tape
                    .append_sideband(PendingSidebandKind::RequestBoundary);
                self.settle_output_after_completion(Duration::from_millis(120));
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
                self.settle_pending_session_end(&ipc);
            }
            Err(IpcWaitError::Protocol(message)) => {
                self.driver.clear_active_turn();
                self.settled_pending_error = Some(WorkerError::Protocol(message));
            }
            Err(IpcWaitError::Timeout | IpcWaitError::Disconnected) => {
                let worker_exited = self
                    .process
                    .as_mut()
                    .and_then(|process| process.is_running().ok())
                    .is_some_and(|running| !running);
                if worker_exited {
                    self.settle_pending_session_end(&ipc);
                }
            }
        }
    }

    fn settle_pending_session_end(&mut self, ipc: &ServerIpcConnection) {
        let settled_completion =
            completion_info_from_ipc(ipc, true, output_echo_source_for_backend(self.backend));
        self.pending_output_tape
            .append_sideband(PendingSidebandKind::RequestBoundary);
        self.settle_output_after_completion(Duration::from_millis(120));
        self.note_session_end(true);
        self.clear_pending_request_state();
        self.settled_pending_completion = Some(settled_completion);
    }

    fn clear_pending_request_state(&mut self) {
        self.pending_request = false;
        self.pending_request_started_at = None;
        self.driver.clear_active_turn();
        self.settled_pending_completion = None;
        self.settled_pending_error = None;
        self.guardrail.busy.store(false, Ordering::Relaxed);
    }

    fn take_input_fallback(&mut self, completion: &CompletionInfo) -> InputFallback {
        let raw_input = completion
            .echo_events
            .is_empty()
            .then(|| self.pending_request_input.take())
            .flatten();
        let transcript = raw_input
            .as_deref()
            .and_then(|input| build_input_transcript(completion.prompt.as_deref(), input));
        InputFallback {
            transcript,
            raw_input,
        }
    }

    fn build_session_reset_reply_files(&mut self, meta: &str) -> ReplyWithOffset {
        let formatted = self.drain_sealed_formatted_output();
        self.build_session_reset_reply_files_from_formatted(meta, formatted)
    }

    fn build_session_reset_reply_files_from_formatted(
        &mut self,
        meta: &str,
        formatted: FormattedPendingOutput,
    ) -> ReplyWithOffset {
        let FormattedPendingOutput {
            mut contents,
            saw_stderr,
        } = formatted;
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
        self.build_session_reset_reply_pager_to_offset(page_bytes, meta, end_offset)
    }

    fn build_session_reset_reply_pager_to_offset(
        &mut self,
        page_bytes: u64,
        meta: &str,
        end_offset: u64,
    ) -> ReplyWithOffset {
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

fn prefix_worker_text_bytes(contents: &[WorkerContent]) -> u64 {
    contents
        .iter()
        .map(|content| match content {
            WorkerContent::ContentText {
                text,
                origin: ContentOrigin::Worker,
                ..
            } => text.len() as u64,
            WorkerContent::ContentText { .. } | WorkerContent::ContentImage { .. } => 0,
        })
        .sum()
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
    use crate::ipc::WorkerToServerIpcMessage;
    #[cfg(any(target_family = "unix", target_family = "windows"))]
    use crate::ipc::{IpcHandlers, IpcPlotImage};
    use crate::output_capture::{
        OUTPUT_RING_CAPACITY_BYTES, OutputEventKind, OutputRange, OutputRing, OutputTextSpan,
        ensure_output_ring, reset_last_reply_marker_offset, reset_output_ring,
    };
    use crate::pending_output_tape::PendingOutputEvent;
    use crate::pending_output_tape::PendingTextSource;
    use crate::sandbox::SandboxPolicy;
    #[cfg(target_os = "linux")]
    use crate::sandbox::sandbox_state_update_from_codex_meta;
    use crate::worker_protocol::TextStream;
    #[cfg(target_family = "unix")]
    use crate::worker_supervisor::capture_recorded_unix_kills;
    #[cfg(target_os = "linux")]
    use crate::worker_supervisor::linux_sandbox_startup_retryable;
    use crate::worker_supervisor::{
        LiveOutputCapture, apply_debug_startup_env, cleanup_worker_session_tmpdir,
        persist_worker_startup_log,
    };
    #[cfg(target_family = "windows")]
    use crate::worker_supervisor::{
        WINDOWS_IPC_CONNECT_MAX_WAIT, handle_windows_ipc_connect_result, request_soft_termination,
    };
    #[cfg(target_os = "linux")]
    use serde_json::json;
    use std::process::{Child, Command};
    use std::sync::mpsc;
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
            .unwrap_or_else(|err| err.into_inner())
    }

    #[cfg(target_family = "unix")]
    fn worker_process_test_temp_parent(label: &str) -> PathBuf {
        let root = std::env::temp_dir()
            .join("mcp-repl-test-scratch")
            .join(label);
        std::fs::create_dir_all(&root).expect("create worker process test temp parent");
        root
    }

    #[test]
    fn python_backend_prepares_windows_sandbox_launch() {
        assert!(
            backend_prepares_windows_sandbox_launch(Backend::Python),
            "Python uses the embedded worker wrapper and needs the prepared Windows capability SID"
        );
    }

    fn echo_event(prompt: &str, line: &str) -> IpcEchoEvent {
        IpcEchoEvent {
            prompt: prompt.to_string(),
            line: line.to_string(),
            source: OutputTextSource::Ipc,
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

    fn pager_buffer_from_worker_text(text: &str) -> crate::pager::PagerBuffer {
        pager_buffer_from_worker_text_with_source_end(text, text.len() as u64)
    }

    fn static_pager_buffer_from_worker_text(text: &str) -> crate::pager::PagerBuffer {
        pager_buffer_from_worker_text_with_source_end(text, u64::MAX)
    }

    fn pager_buffer_from_worker_text_with_source_end(
        text: &str,
        source_end: u64,
    ) -> crate::pager::PagerBuffer {
        crate::pager::PagerBuffer::from_bytes_and_events(
            text.as_bytes().to_vec(),
            Vec::new(),
            vec![OutputTextSpan {
                start_byte: 0,
                end_byte: text.len(),
                is_stderr: false,
                origin: ContentOrigin::Worker,
                source: crate::output_capture::OutputTextSource::Raw,
            }],
            source_end,
        )
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

    #[cfg(target_family = "windows")]
    fn successful_test_child() -> Child {
        Command::new("powershell.exe")
            .args(["-NoProfile", "-Command", "exit 0"])
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
        WorkerProcess::new_for_test(child)
    }

    #[cfg(target_family = "windows")]
    fn test_worker_process(child: Child) -> WorkerProcess {
        WorkerProcess::new_for_test(child)
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
        let warning = "late readline result".to_string();
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
        let warning = "late readline result".to_string();
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
    fn trim_echo_prefix_after_leading_nonstdout_contents_removes_prompt_fallback_echo() {
        let mut contents = vec![
            WorkerContent::stderr("stderr: Error: object 'x' not found\n"),
            WorkerContent::stdout("> x\n"),
            WorkerContent::stdout("> "),
        ];

        let trimmed =
            trim_echo_prefix_after_leading_nonstdout_contents(&mut contents, Some("> x\n"));

        assert!(trimmed, "expected prompt fallback echo to be trimmed");
        assert_eq!(
            contents,
            vec![
                WorkerContent::stderr("stderr: Error: object 'x' not found\n"),
                WorkerContent::stdout("> "),
            ]
        );
    }

    #[test]
    fn trim_decision_applies_to_any_sideband_echo() {
        let single = vec![echo_event("> ", "1+1\n")];
        assert!(should_trim_echo_prefix(&single));

        let continuation = vec![echo_event("> ", "1+\n"), echo_event("+ ", "1\n")];
        assert!(should_trim_echo_prefix(&continuation));

        let multi = vec![echo_event("> ", "1+1\n"), echo_event("> ", "2+2\n")];
        assert!(should_trim_echo_prefix(&multi));

        let browser = vec![echo_event("Browse[1]> ", "n\n")];
        assert!(should_trim_echo_prefix(&browser));

        let readline = vec![echo_event("FIRST> ", "alpha\n")];
        assert!(should_trim_echo_prefix(&readline));
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
                source: crate::output_capture::OutputTextSource::Ipc,
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
                source: crate::output_capture::OutputTextSource::Ipc,
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
    fn control_prefix_preserves_immediate_newline_tail() {
        let (action, remaining) =
            split_write_stdin_control_prefix("\u{4}\nprint(1)").expect("expected control prefix");
        assert!(matches!(action, WriteStdinControlAction::Restart));
        assert_eq!(remaining, "\nprint(1)");
    }

    #[test]
    fn completion_infers_nested_waiting_prompt_that_reuses_primary_prompt_text() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");
        driver_on_input_start("value <- readline(prompt = \"> \")", &server)
            .expect("begin request");
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

        let completion =
            driver_wait_for_completion(Duration::from_millis(200), server, OutputTextSource::Ipc)
                .expect("expected stable waiting prompt to complete request");
        assert_eq!(completion.prompt.as_deref(), Some("> "));
        assert_eq!(completion.echo_events.len(), 1);
        assert_eq!(completion.echo_events[0].prompt, "> ");
        assert_eq!(
            completion.echo_events[0].line,
            "value <- readline(prompt = \"> \")\n"
        );
    }

    #[test]
    fn completion_infers_stable_waiting_prompt_without_worker_completion_event() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");
        driver_on_input_start("1+1", &server).expect("begin request");
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

        let completion =
            driver_wait_for_completion(Duration::from_millis(200), server, OutputTextSource::Ipc)
                .expect("expected stable waiting prompt to complete request");

        assert_eq!(completion.prompt.as_deref(), Some("> "));
        assert_eq!(completion.echo_events.len(), 1);
        assert_eq!(completion.echo_events[0].line, "1+1\n");
    }

    #[test]
    fn completion_settle_after_prompt_does_not_count_as_execution_timeout() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");
        driver_on_input_start("1+1", &server).expect("begin request");
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
        thread::sleep(Duration::from_millis(1));

        let completion =
            driver_wait_for_completion(Duration::from_millis(5), server, OutputTextSource::Ipc)
                .expect("expected prompt seen before timeout to complete after stable settle");

        assert_eq!(completion.prompt.as_deref(), Some("> "));
        assert_eq!(completion.echo_events.len(), 1);
        assert_eq!(completion.echo_events[0].line, "1+1\n");
    }

    #[test]
    fn completion_infers_stable_continuation_prompt_when_input_is_consumed() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");
        driver_on_input_start("1+\n1", &server).expect("begin request");
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

        let completion =
            driver_wait_for_completion(Duration::from_millis(200), server, OutputTextSource::Ipc)
                .expect("expected stable continuation prompt to complete request");
        assert_eq!(completion.prompt.as_deref(), Some("+ "));
    }

    #[test]
    fn completion_settle_waits_for_late_echo_events() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");
        driver_on_input_start("1+\n1", &server).expect("begin request");
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
            let _ = delayed_worker.send(WorkerToServerIpcMessage::ReadlineStart {
                prompt: "> ".to_string(),
            });
        });

        let completion =
            driver_wait_for_completion(Duration::from_millis(200), server, OutputTextSource::Ipc)
                .expect("expected completion after stable waiting prompt");
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
    fn completion_waits_for_active_stdin_accounting_before_prompt_completion() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");
        server.begin_request_with_stdin(b"1+\n1\n");

        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: "> ".to_string(),
        });
        thread::sleep(REQUEST_COMPLETION_STABLE_WAIT + Duration::from_millis(5));
        let early = server
            .wait_for_request_completion(Duration::from_millis(1), REQUEST_COMPLETION_STABLE_WAIT);
        assert!(
            matches!(early, Err(IpcWaitError::Timeout)),
            "did not expect buffered readline start to complete request, got {early:?}"
        );

        let _ = worker.send(WorkerToServerIpcMessage::ReadlineInputBytes {
            data_b64: base64::engine::general_purpose::STANDARD.encode(b"1+\n"),
        });
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineResult {
            prompt: "> ".to_string(),
            line: "1+\n".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: "+ ".to_string(),
        });
        thread::sleep(REQUEST_COMPLETION_STABLE_WAIT + Duration::from_millis(5));
        let continuation = server
            .wait_for_request_completion(Duration::from_millis(1), REQUEST_COMPLETION_STABLE_WAIT);
        assert!(
            matches!(continuation, Err(IpcWaitError::Timeout)),
            "did not expect buffered continuation start to complete request, got {continuation:?}"
        );

        let _ = worker.send(WorkerToServerIpcMessage::ReadlineInputBytes {
            data_b64: base64::engine::general_purpose::STANDARD.encode(b"1\n"),
        });
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineResult {
            prompt: "+ ".to_string(),
            line: "1\n".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: "> ".to_string(),
        });

        let completion =
            driver_wait_for_completion(Duration::from_millis(200), server, OutputTextSource::Ipc)
                .expect("expected completion after final unsatisfied prompt");

        assert_eq!(completion.prompt.as_deref(), Some("> "));
        assert_eq!(completion.echo_events.len(), 2);
        assert_eq!(completion.echo_events[0].line, "1+\n");
        assert_eq!(completion.echo_events[1].line, "1\n");
    }

    #[test]
    fn next_request_result_is_retained_when_prompt_is_already_active() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");

        driver_on_input_start("first()", &server).expect("begin request");
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: "> ".to_string(),
        });
        let first = driver_wait_for_completion(
            Duration::from_millis(200),
            server.clone(),
            OutputTextSource::Ipc,
        )
        .expect("expected first completion");
        assert_eq!(first.prompt.as_deref(), Some("> "));

        driver_on_input_start("second()", &server).expect("begin request");
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineResult {
            prompt: "> ".to_string(),
            line: "second()\n".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: "> ".to_string(),
        });

        let second =
            driver_wait_for_completion(Duration::from_millis(200), server, OutputTextSource::Ipc)
                .expect("expected second completion");

        assert!(second.protocol_warnings.is_empty());
        assert_eq!(second.echo_events.len(), 1);
        assert_eq!(second.echo_events[0].prompt, "> ");
        assert_eq!(second.echo_events[0].line, "second()\n");
    }

    #[test]
    fn completion_preserves_echo_events_when_next_prompt_arrives_immediately() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");

        driver_on_input_start("first()", &server).expect("begin request");
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: "> ".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineResult {
            prompt: "> ".to_string(),
            line: "first()\n".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: "> ".to_string(),
        });

        let completion =
            driver_wait_for_completion(Duration::from_millis(200), server, OutputTextSource::Ipc)
                .expect("expected completion after stable waiting prompt");

        assert_eq!(completion.prompt.as_deref(), Some("> "));
        assert!(completion.protocol_warnings.is_empty());
        assert_eq!(completion.echo_events.len(), 1);
        assert_eq!(completion.echo_events[0].prompt, "> ");
        assert_eq!(completion.echo_events[0].line, "first()\n");
    }

    #[test]
    fn completion_retains_echo_events_when_session_ends_before_prompt_completion() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");
        driver_on_input_start("quit()", &server).expect("begin request");

        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: "> ".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineResult {
            prompt: "> ".to_string(),
            line: "quit()\n".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::SessionEnd {
            reason: None,
            message_b64: None,
            turn_id: None,
        });

        let completion =
            driver_wait_for_completion(Duration::from_millis(200), server, OutputTextSource::Ipc)
                .expect("expected completion after session end");

        assert!(completion.session_end_seen);
        assert_eq!(completion.echo_events.len(), 1);
        assert_eq!(completion.echo_events[0].prompt, "> ");
        assert_eq!(completion.echo_events[0].line, "quit()\n");
    }

    #[test]
    fn completion_reports_session_end_when_prompt_is_also_stable() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");
        driver_on_input_start("quit()", &server).expect("begin request");

        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: "> ".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineResult {
            prompt: "> ".to_string(),
            line: "quit()\n".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: "> ".to_string(),
        });
        thread::sleep(Duration::from_millis(25));
        let _ = worker.send(WorkerToServerIpcMessage::SessionEnd {
            reason: None,
            message_b64: None,
            turn_id: None,
        });
        thread::sleep(Duration::from_millis(25));

        let completion =
            driver_wait_for_completion(Duration::from_millis(200), server, OutputTextSource::Ipc)
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
                WriteStdinOptions::default(),
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
        process.set_exit_status_for_test(failing_test_status());
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
    fn timed_out_prompt_completion_with_exited_worker_reports_session_end_immediately() {
        let _guard = env_test_mutex().lock().expect("env mutex");
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");
        let mut manager = WorkerManager::new(
            Backend::Python,
            SandboxCliPlan::default(),
            crate::oversized_output::OversizedOutputMode::Files,
        )
        .expect("worker manager");
        let mut process = test_worker_process(successful_test_child());
        let status = process.wait_child_for_test().expect("wait test child");
        process.set_exit_status_for_test(status);
        process.set_ipc_for_test(server);
        manager.process = Some(process);
        manager.pending_request = true;
        manager.pending_request_started_at = Some(std::time::Instant::now());
        manager.pending_request_input = Some("quit()\n".to_string());

        let prompt = ">>> ".to_string();
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: prompt.clone(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineResult {
            prompt,
            line: "quit()\n".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: ">>> ".to_string(),
        });
        drop(worker);
        manager.resolve_timeout_marker_with_wait(Duration::from_millis(200));
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
    fn files_prepare_input_context_trims_echo_from_prompt_fallback_when_echo_events_missing() {
        let mut manager = WorkerManager::new(
            Backend::Python,
            SandboxCliPlan::default(),
            crate::oversized_output::OversizedOutputMode::Files,
        )
        .expect("worker manager");
        manager
            .pending_output_tape
            .append_stdout_bytes(b">>> import time; time.sleep(0.2)\nDETACHED_OK\n");
        manager.pending_request_input = Some("import time; time.sleep(0.2)\n".to_string());
        manager.settled_pending_completion = Some(CompletionInfo {
            prompt: Some(">>> ".to_string()),
            stdin_wait_prompt: None,
            prompt_variants: Some(vec![">>> ".to_string()]),
            echo_events: Vec::new(),
            protocol_warnings: Vec::new(),
            session_end_seen: false,
        });

        let context = manager.prepare_input_context_files();
        let text = contents_text(&context.detached_prefix_contents);

        assert!(
            text.contains("DETACHED_OK\n"),
            "expected the settled files-mode output to survive trimming, got: {text:?}"
        );
        assert!(
            !text.contains("import time; time.sleep(0.2)"),
            "did not expect the Python prompt echo to leak into the next files-mode reply, got: {text:?}"
        );
        assert!(
            manager.settled_pending_completion.is_none(),
            "expected settled completion metadata to be consumed with the detached prefix"
        );
    }

    #[test]
    fn interrupt_files_drains_settled_completion_without_leaking_echo() {
        let mut manager = WorkerManager::new(
            Backend::Python,
            SandboxCliPlan::default(),
            crate::oversized_output::OversizedOutputMode::Files,
        )
        .expect("worker manager");
        manager
            .pending_output_tape
            .append_stdout_bytes(b">>> import time; time.sleep(0.07)\nDETACHED_OK\n");
        manager.pending_request_input = Some("import time; time.sleep(0.07)\n".to_string());
        manager.settled_pending_completion = Some(CompletionInfo {
            prompt: Some(">>> ".to_string()),
            stdin_wait_prompt: None,
            prompt_variants: Some(vec![">>> ".to_string()]),
            echo_events: Vec::new(),
            protocol_warnings: Vec::new(),
            session_end_seen: false,
        });

        let WorkerReply::Output { contents, .. } = manager
            .interrupt(Duration::from_millis(10), None, false)
            .expect("interrupt reply");
        let text = contents_text(&contents);

        assert!(
            text.contains("DETACHED_OK\n"),
            "expected the settled completion output to be preserved, got: {text:?}"
        );
        assert!(
            !text.contains("import time; time.sleep(0.07)"),
            "did not expect the settled completion echo to leak through interrupt handling, got: {text:?}"
        );
        assert!(
            text.contains(">>> "),
            "expected the settled completion to keep the prompt on the interrupt reply, got: {text:?}"
        );
        assert!(
            manager.settled_pending_completion.is_none(),
            "expected the settled completion to be consumed by the interrupt follow-up"
        );
    }

    #[test]
    fn files_empty_poll_waits_for_late_stdout_after_settled_completion() {
        let mut manager = WorkerManager::new(
            Backend::R,
            SandboxCliPlan::default(),
            crate::oversized_output::OversizedOutputMode::Files,
        )
        .expect("worker manager");
        manager.process = Some(test_worker_process(sleeping_test_child()));
        manager.settled_pending_completion = Some(CompletionInfo {
            prompt: Some("> ".to_string()),
            stdin_wait_prompt: None,
            prompt_variants: Some(vec!["> ".to_string()]),
            echo_events: Vec::new(),
            protocol_warnings: Vec::new(),
            session_end_seen: false,
        });

        let tape = manager.pending_output_tape.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            tape.append_stdout_bytes(b"[1] 2\n");
        });

        let reply = manager
            .write_stdin_files(
                String::new(),
                Duration::from_millis(500),
                Duration::from_millis(500),
                WriteStdinOptions::default(),
            )
            .expect("empty poll reply");

        writer.join().expect("late stdout writer");
        if let Some(process) = manager.process.take() {
            let _ = process.kill();
        }

        let WorkerReply::Output { contents, .. } = reply;
        let text = contents_text(&contents);
        assert!(
            text.contains("[1] 2\n"),
            "expected the empty poll to wait for late settled stdout, got: {text:?}"
        );
        assert!(
            !text.contains("<<repl status: idle>>"),
            "did not expect an idle marker before late settled stdout, got: {text:?}"
        );
    }

    #[test]
    fn files_reset_preserving_detached_output_keeps_pending_request_input_for_trim() {
        let mut manager = WorkerManager::new(
            Backend::Python,
            SandboxCliPlan::default(),
            crate::oversized_output::OversizedOutputMode::Files,
        )
        .expect("worker manager");
        manager
            .pending_output_tape
            .append_stdout_bytes(b">>> import time; time.sleep(0.2)\nDETACHED_OK\n");
        manager.pending_request_input = Some("import time; time.sleep(0.2)\n".to_string());
        manager.settled_pending_completion = Some(CompletionInfo {
            prompt: Some(">>> ".to_string()),
            stdin_wait_prompt: None,
            prompt_variants: Some(vec![">>> ".to_string()]),
            echo_events: Vec::new(),
            protocol_warnings: Vec::new(),
            session_end_seen: false,
        });

        manager.reset_output_state_files_preserving_detached_output();

        let context = manager.prepare_input_context_files();
        let text = contents_text(&context.detached_prefix_contents);

        assert!(
            text.contains("DETACHED_OK\n"),
            "expected detached files-mode output to survive the preserved reset, got: {text:?}"
        );
        assert!(
            !text.contains("import time; time.sleep(0.2)"),
            "did not expect the preserved reset to leak the original Python input echo, got: {text:?}"
        );
        assert!(
            manager.pending_request_input.is_none(),
            "expected preserved pending input to be consumed once the detached prefix is prepared"
        );
    }

    #[test]
    fn files_respawned_pending_request_trims_echo_without_settled_completion() {
        let mut manager = WorkerManager::new(
            Backend::Python,
            SandboxCliPlan::default(),
            crate::oversized_output::OversizedOutputMode::Files,
        )
        .expect("worker manager");
        manager.pending_request = true;
        manager.last_prompt = Some(">>> ".to_string());
        manager
            .pending_output_tape
            .append_stdout_bytes(b">>> import time; time.sleep(0.2)\nDETACHED_OK\n");
        manager.pending_request_input = Some("import time; time.sleep(0.2)\n".to_string());

        manager.reset_output_state_files_preserving_detached_output();

        let context = manager.prepare_input_context_files();
        let text = contents_text(&context.detached_prefix_contents);

        assert!(
            text.contains("DETACHED_OK\n"),
            "expected aborted pending output to survive the respawned reset, got: {text:?}"
        );
        assert!(
            !text.contains("import time; time.sleep(0.2)"),
            "did not expect the aborted request echo to leak across the respawn boundary, got: {text:?}"
        );
        assert!(
            manager.pending_request_input.is_none(),
            "expected the aborted request input fallback to be consumed once the detached prefix is prepared"
        );
    }

    #[test]
    fn pager_respawned_pending_request_trims_echo_without_echo_events() {
        let _guard = output_ring_test_guard();
        let _output_ring = ensure_output_ring(OUTPUT_RING_CAPACITY_BYTES);
        reset_output_ring();
        reset_last_reply_marker_offset();

        let mut manager = WorkerManager::new(
            Backend::Python,
            SandboxCliPlan::default(),
            crate::oversized_output::OversizedOutputMode::Pager,
        )
        .expect("worker manager");
        manager.pending_request = true;
        manager.last_prompt = Some(">>> ".to_string());
        manager.pending_request_input = Some("import time; time.sleep(0.2)\n".to_string());
        manager.output.start_capture();
        manager.output_timeline.append_ipc_text_with_continuation(
            b">>> import time; time.sleep(0.2)\nDETACHED_OK\n",
            false,
            ContentOrigin::Worker,
            false,
        );

        manager.reset_output_state_pager_preserving_detached_output(false);

        let context = manager.prepare_input_context_pager("1+1", false);
        let text = contents_text(&context.detached_prefix_contents);

        assert!(
            text.contains("DETACHED_OK\n"),
            "expected aborted pager output to survive the respawned reset, got: {text:?}"
        );
        assert!(
            !text.contains("import time; time.sleep(0.2)"),
            "did not expect the aborted pager echo to leak across the respawn boundary, got: {text:?}"
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
            contents_text(&first.detached_prefix_contents),
            "\\xC3",
            "expected an accepted request to seal the detached utf-8 lead byte into the prefix"
        );

        manager
            .pending_output_tape
            .append_stdout_bytes(&[0xA9, b'\n']);
        let second = manager.prepare_input_context_files();

        assert_eq!(
            contents_text(&second.detached_prefix_contents),
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
            .append_stdout_ipc_bytes(b"> Sys.sleep(5)\n");
        manager
            .pending_output_tape
            .append_sideband(PendingSidebandKind::ReadlineResult {
                prompt: "> ".to_string(),
                line: "Sys.sleep(5)\n".to_string(),
                echo_source: PendingTextSource::Ipc,
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
            .append_stdout_ipc_bytes(b"> Sys.sleep(5)\n");
        manager
            .pending_output_tape
            .append_sideband(PendingSidebandKind::ReadlineResult {
                prompt: "> ".to_string(),
                line: "Sys.sleep(5)\n".to_string(),
                echo_source: PendingTextSource::Ipc,
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
            .append_stdout_ipc_bytes(b"> Sys.sleep(5)\n");
        manager
            .pending_output_tape
            .append_sideband(PendingSidebandKind::ReadlineResult {
                prompt: "> ".to_string(),
                line: "Sys.sleep(5)\n".to_string(),
                echo_source: PendingTextSource::Ipc,
            });

        let context = manager.prepare_input_context_files();

        assert_eq!(
            context.detached_prefix_contents,
            vec![WorkerContent::stdout("> Sys.sleep(5)\n")],
            "expected a sealed files-mode prefix without settled completion metadata to keep echoed input"
        );
    }

    #[test]
    fn files_preserved_detached_prefix_stays_separate_from_new_session_startup_output() {
        let mut manager = WorkerManager::new(
            Backend::Python,
            SandboxCliPlan::default(),
            crate::oversized_output::OversizedOutputMode::Files,
        )
        .expect("worker manager");
        manager
            .pending_output_tape
            .append_stdout_bytes(b"OLD_TAIL\n");

        manager.reset_output_state_files_preserving_detached_output();
        manager.next_live_prefix_belongs_to_reply = true;
        manager
            .pending_output_tape
            .append_stdout_bytes(b"NEW_SESSION_STARTUP\n");

        let context = manager.prepare_input_context_files();

        assert_eq!(
            contents_text(&context.detached_prefix_contents),
            "OLD_TAIL\n",
            "expected preserved detached output to stay isolated from the replacement session"
        );
        assert_eq!(
            contents_text(&context.reply_prefix_contents),
            "NEW_SESSION_STARTUP\n",
            "expected fresh-session startup output to stay with the new reply prefix"
        );
    }

    #[test]
    fn busy_guardrail_event_survives_sandbox_restart_notice() {
        let mut manager = WorkerManager::new(
            Backend::R,
            SandboxCliPlan::default(),
            crate::oversized_output::OversizedOutputMode::Files,
        )
        .expect("worker manager");
        manager.exe_path = PathBuf::from("definitely-missing-worker-exe");
        manager.stage_sandbox_change_restart_notice(true);
        manager.guardrail.busy.store(true, Ordering::Relaxed);
        {
            let mut slot = manager
                .guardrail
                .event
                .lock()
                .expect("guardrail event mutex poisoned");
            *slot = Some(GuardrailEvent {
                message: "[repl] worker killed by memory guardrail\n".to_string(),
                was_busy: true,
                is_error: true,
            });
        }

        let reply = manager
            .write_stdin_files(
                "1+1".to_string(),
                Duration::from_millis(10),
                Duration::from_millis(10),
                WriteStdinOptions::default(),
            )
            .expect("guardrail reply");
        let WorkerReply::Output { contents, .. } = reply;
        let text = contents_text(&contents);

        assert!(
            text.contains("sandbox policy changed; new session started"),
            "expected the queued restart notice to stay visible, got: {text:?}"
        );
        assert!(
            text.contains("worker error: [repl] worker killed by memory guardrail"),
            "expected the busy guardrail error to remain authoritative, got: {text:?}"
        );
        assert!(
            !manager.guardrail_busy_event_pending(),
            "expected the busy guardrail slot to be consumed by the local retry reply"
        );
        assert!(
            manager.pending_server_notice.is_none(),
            "expected the restart notice to be emitted instead of lingering"
        );
        if let Some(process) = manager.process.take() {
            let _ = process.kill();
        }
    }

    #[test]
    fn bare_restart_flushes_queued_sandbox_change_notice() {
        let mut manager = WorkerManager::new(
            Backend::R,
            SandboxCliPlan::default(),
            crate::oversized_output::OversizedOutputMode::Files,
        )
        .expect("worker manager");
        manager.stage_sandbox_change_restart_notice(true);

        let reply = manager
            .write_stdin_files(
                "\u{4}".to_string(),
                Duration::from_millis(10),
                Duration::from_millis(10),
                WriteStdinOptions::default(),
            )
            .expect("restart reply");
        let WorkerReply::Output { contents, .. } = reply;
        let text = contents_text(&contents);

        assert!(
            text.contains("sandbox policy changed; new session started"),
            "expected bare restart to flush the queued sandbox notice, got: {text:?}"
        );
        assert!(
            text.contains("[repl] new session started"),
            "expected the explicit restart notice to remain visible, got: {text:?}"
        );
        assert!(
            manager.pending_server_notice.is_none(),
            "expected the queued sandbox notice to be consumed by the restart reply"
        );
    }

    #[test]
    fn pager_collapsed_settled_completion_trims_echo_and_keeps_output() {
        let range = OutputRange {
            start_offset: 0,
            end_offset: 25,
            bytes: b"> Sys.sleep(0.2); 1+1\n[1] 2\n".to_vec(),
            events: Vec::new(),
            text_spans: vec![OutputTextSpan {
                start_byte: 0,
                end_byte: 25,
                is_stderr: false,
                origin: ContentOrigin::Worker,
                source: crate::output_capture::OutputTextSource::Ipc,
            }],
        };

        let collapsed = collapse_echo_with_attribution(
            range,
            &[echo_event("> ", "Sys.sleep(0.2); 1+1\n")],
            0,
            &["> ".to_string()],
            EchoCollapseMode::CollapseForFinalReply,
        );
        let contents = pager::contents_from_collapsed_output(
            collapsed.bytes,
            collapsed.events,
            collapsed.text_spans,
            25,
        );
        let text = contents_text(&contents);

        assert!(
            text.contains("[1] 2\n"),
            "expected settled pager output to be preserved, got: {text:?}"
        );
        assert!(
            !text.contains("Sys.sleep(0.2); 1+1"),
            "did not expect settled pager echo to leak into the next input context, got: {text:?}"
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

        manager.pager.activate(
            pager_buffer_from_worker_text("line0001\nline0002\nline0003\nline0004\n"),
            false,
        );

        manager.output.start_capture();
        if let Some(end_offset) = manager.output.end_offset() {
            manager.output.advance_offset_to(end_offset);
        }
        manager
            .output_timeline
            .append_text(b"detached\n", false, ContentOrigin::Worker);

        let reply = manager
            .write_stdin_pager(
                String::new(),
                Duration::from_millis(0),
                Duration::from_millis(0),
                WriteStdinOptions {
                    page_bytes_override: Some(16),
                    echo_input: true,
                    ..WriteStdinOptions::default()
                },
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
        let mut process = test_worker_process(successful_test_child());
        let status = process.wait_child_for_test().expect("wait test child");
        process.set_exit_status_for_test(status);
        manager.process = Some(process);
        manager.exe_path = PathBuf::from("definitely-missing-worker-exe");

        let output = (1..=24).map(|n| format!("L{n:04}\n")).collect::<String>();
        manager
            .pager
            .activate(static_pager_buffer_from_worker_text(&output), false);
        manager.output.start_capture();
        if let Some(end_offset) = manager.output.end_offset() {
            manager.output.advance_offset_to(end_offset);
        }
        let reply = manager
            .write_stdin_pager(
                String::new(),
                Duration::from_millis(0),
                Duration::from_millis(0),
                WriteStdinOptions {
                    page_bytes_override: Some(16),
                    echo_input: true,
                    ..WriteStdinOptions::default()
                },
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
    fn bare_restart_clears_preserved_detached_prefixes() {
        let mut manager = WorkerManager::new(
            Backend::R,
            SandboxCliPlan::default(),
            crate::oversized_output::OversizedOutputMode::Files,
        )
        .expect("worker manager");
        manager.preserved_detached_prefix = PrefixCapture {
            contents: vec![WorkerContent::worker_stdout("OLD_DETACHED\n")],
            is_error: false,
            bytes: "OLD_DETACHED\n".len() as u64,
        };
        manager.reply_owned_prefix = PrefixCapture {
            contents: vec![WorkerContent::worker_stdout("OLD_REPLY\n")],
            is_error: false,
            bytes: "OLD_REPLY\n".len() as u64,
        };
        manager.next_live_prefix_belongs_to_reply = true;

        let reply = manager
            .write_stdin_files(
                "\u{4}".to_string(),
                Duration::from_millis(10),
                Duration::from_millis(10),
                WriteStdinOptions::default(),
            )
            .expect("restart reply");
        let WorkerReply::Output { contents, .. } = reply;
        let reply_text = contents_text(&contents);
        assert!(
            !reply_text.contains("OLD_DETACHED") && !reply_text.contains("OLD_REPLY"),
            "did not expect preserved detached prefixes in restart reply, got: {reply_text:?}"
        );

        let context = manager.prepare_input_context_files();
        assert!(
            context.detached_prefix_contents.is_empty() && context.reply_prefix_contents.is_empty(),
            "did not expect explicit restart to leak old prefixes into the next input"
        );
    }

    #[test]
    fn pager_empty_input_preserves_idle_guardrail_notice() {
        let _guard = output_ring_test_guard();
        let _output_ring = ensure_output_ring(OUTPUT_RING_CAPACITY_BYTES);

        let mut last_text = String::new();
        for _ in 0..16 {
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
                    is_error: false,
                });
            }

            let reply = manager
                .write_stdin_pager(
                    String::new(),
                    Duration::from_millis(0),
                    Duration::from_millis(0),
                    WriteStdinOptions {
                        page_bytes_override: Some(OUTPUT_RING_CAPACITY_BYTES as u64),
                        echo_input: true,
                        ..WriteStdinOptions::default()
                    },
                )
                .expect("empty poll reply");
            let WorkerReply::Output { contents, .. } = reply;
            last_text = contents_text(&contents);

            if let Some(process) = manager.process.take() {
                let _ = process.kill();
            }

            if last_text.contains("[repl] worker was idle; new session started") {
                return;
            }

            thread::sleep(Duration::from_millis(5));
        }

        assert!(
            last_text.contains("[repl] worker was idle; new session started"),
            "expected empty pager polls to preserve idle guardrail restart notices, got: {last_text:?}"
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
        capture.append_output_text(b"pager output\n", TextStream::Stdout, false);
        capture.append_image(IpcPlotImage {
            id: "img-1".to_string(),
            data: "AA==".to_string(),
            mime_type: "image/png".to_string(),
            is_new: true,
            updates_previous_image: false,
            readline_results_seen: 0,
        });
        capture.append_sideband(PendingSidebandKind::RequestBoundary);

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
    fn files_output_capture_anchors_update_notice_before_late_echo() {
        let output_ring = Arc::new(OutputRing::with_capacity(OUTPUT_RING_CAPACITY_BYTES));
        let tape = PendingOutputTape::new();
        let capture = LiveOutputCapture::new(
            OversizedOutputMode::Files,
            tape.clone(),
            OutputTimeline::new(output_ring),
        );

        capture.append_sideband(PendingSidebandKind::ReadlineResult {
            prompt: "> ".to_string(),
            line: "lines(4:8, 4:8)\n".to_string(),
            echo_source: PendingTextSource::Ipc,
        });
        capture.append_image(IpcPlotImage {
            id: "img-1".to_string(),
            data: "AA==".to_string(),
            mime_type: "image/png".to_string(),
            is_new: true,
            updates_previous_image: true,
            readline_results_seen: 1,
        });
        capture.append_output_text(b"> lines(4:8, 4:8)\n", TextStream::Stdout, false);

        let contents = tape
            .drain_final_snapshot()
            .format_contents_for_reply()
            .contents;

        assert_eq!(
            contents,
            vec![
                WorkerContent::server_stdout(PREVIOUS_IMAGE_UPDATE_NOTICE),
                WorkerContent::ContentImage {
                    data: "AA==".to_string(),
                    mime_type: "image/png".to_string(),
                    id: "img-1".to_string(),
                    is_new: true,
                },
            ]
        );
    }

    #[test]
    fn files_ipc_output_text_appends_to_tape_and_timeline_in_ipc_order() {
        let output_ring = Arc::new(OutputRing::with_capacity(OUTPUT_RING_CAPACITY_BYTES));
        let tape = PendingOutputTape::new();
        let capture = LiveOutputCapture::new(
            OversizedOutputMode::Files,
            tape.clone(),
            OutputTimeline::new(output_ring.clone()),
        );
        let (done_tx, done_rx) = mpsc::channel();

        let output_capture = capture.clone();
        let start_capture = capture.clone();
        let result_capture = capture.clone();
        let image_capture = capture.clone();
        let session_capture = capture.clone();
        let (_server, worker) = crate::ipc::test_connection_pair_with_handlers(IpcHandlers {
            on_output_text: Some(Arc::new(move |text| {
                output_capture.append_output_text(&text.bytes, text.stream, text.is_continuation);
            })),
            on_readline_start: Some(Arc::new(move |prompt| {
                start_capture.append_sideband(PendingSidebandKind::ReadlineStart { prompt });
            })),
            on_readline_result: Some(Arc::new(move |event| {
                result_capture.append_sideband(PendingSidebandKind::ReadlineResult {
                    prompt: event.prompt,
                    line: event.line,
                    echo_source: PendingTextSource::Ipc,
                });
            })),
            on_plot_image: Some(Arc::new(move |image| {
                image_capture.append_image(image);
            })),
            on_session_end: Some(Arc::new(move || {
                session_capture.append_sideband(PendingSidebandKind::SessionEnd);
                done_tx.send(()).expect("send session end marker");
            })),
            ..IpcHandlers::default()
        })
        .expect("ipc pair");

        worker
            .send(WorkerToServerIpcMessage::ReadlineStart {
                prompt: "> ".to_string(),
            })
            .expect("send readline_start");
        worker
            .send(WorkerToServerIpcMessage::OutputText {
                stream: TextStream::Stdout,
                data_b64: base64::engine::general_purpose::STANDARD.encode(b"before\n"),
                is_continuation: false,
            })
            .expect("send stdout output_text");
        worker
            .send(WorkerToServerIpcMessage::ReadlineResult {
                prompt: "> ".to_string(),
                line: "plot(1)\n".to_string(),
            })
            .expect("send readline_result");
        worker
            .send(WorkerToServerIpcMessage::PlotImage {
                mime_type: "image/png".to_string(),
                data: "AA==".to_string(),
                is_update: false,
                source: None,
            })
            .expect("send plot_image");
        worker
            .send(WorkerToServerIpcMessage::OutputText {
                stream: TextStream::Stderr,
                data_b64: base64::engine::general_purpose::STANDARD.encode(b"err\n"),
                is_continuation: false,
            })
            .expect("send stderr output_text");
        worker
            .send(WorkerToServerIpcMessage::SessionEnd {
                reason: None,
                message_b64: None,
                turn_id: None,
            })
            .expect("send session_end");

        done_rx
            .recv_timeout(Duration::from_millis(200))
            .expect("server IPC consumed session_end");

        let snapshot = tape.drain_final_snapshot();
        assert_eq!(snapshot.events.len(), 6);
        assert!(matches!(
            &snapshot.events[0],
            PendingOutputEvent::Sideband {
                kind: PendingSidebandKind::ReadlineStart { prompt },
                ..
            } if prompt == "> "
        ));
        assert!(matches!(
            &snapshot.events[1],
            PendingOutputEvent::TextFragment {
                stream: TextStream::Stdout,
                origin: ContentOrigin::Worker,
                bytes,
                ..
            } if bytes == b"before\n"
        ));
        assert!(matches!(
            &snapshot.events[2],
            PendingOutputEvent::Sideband {
                kind: PendingSidebandKind::ReadlineResult { prompt, line, .. },
                ..
            } if prompt == "> " && line == "plot(1)\n"
        ));
        assert!(matches!(
            &snapshot.events[3],
            PendingOutputEvent::Image {
                id,
                mime_type,
                readline_results_seen: 1,
                ..
            } if id.starts_with("image-") && mime_type == "image/png"
        ));
        assert!(matches!(
            &snapshot.events[4],
            PendingOutputEvent::TextFragment {
                stream: TextStream::Stderr,
                origin: ContentOrigin::Worker,
                bytes,
                ..
            } if bytes == b"err\n"
        ));
        assert!(matches!(
            &snapshot.events[5],
            PendingOutputEvent::Sideband {
                kind: PendingSidebandKind::SessionEnd,
                ..
            }
        ));

        let end = output_ring.end_offset();
        let range = output_ring.read_range(0, end);
        assert_eq!(range.bytes, b"before\n\nstderr: err\n");
        let image_event = range
            .events
            .iter()
            .find_map(|event| match &event.kind {
                OutputEventKind::Image {
                    id,
                    mime_type,
                    readline_results_seen,
                    ..
                } => Some((event.offset, id, mime_type, readline_results_seen)),
                _ => None,
            })
            .expect("timeline image event");
        assert_eq!(image_event.0, b"before\n".len() as u64);
        assert!(image_event.1.starts_with("image-"));
        assert_eq!(image_event.2, "image/png");
        assert_eq!(*image_event.3, 1);
    }

    #[test]
    fn pager_output_capture_anchors_update_notice_before_late_echo() {
        let output_ring = Arc::new(OutputRing::with_capacity(OUTPUT_RING_CAPACITY_BYTES));
        let capture = LiveOutputCapture::new(
            OversizedOutputMode::Pager,
            PendingOutputTape::new(),
            OutputTimeline::new(output_ring.clone()),
        );

        capture.append_image(IpcPlotImage {
            id: "img-1".to_string(),
            data: "AA==".to_string(),
            mime_type: "image/png".to_string(),
            is_new: true,
            updates_previous_image: true,
            readline_results_seen: 1,
        });
        capture.append_output_text(b"> lines(4:8, 4:8)\n", TextStream::Stdout, false);

        let end = output_ring.end_offset();
        let collapsed = collapse_echo_with_attribution(
            output_ring.read_range(0, end),
            &[echo_event("> ", "lines(4:8, 4:8)\n")],
            0,
            &["> ".to_string()],
            EchoCollapseMode::CollapseForFinalReply,
        );
        let contents = pager::contents_from_collapsed_output(
            collapsed.bytes,
            collapsed.events,
            collapsed.text_spans,
            end,
        );

        assert_eq!(
            contents,
            vec![
                WorkerContent::server_stdout(PREVIOUS_IMAGE_UPDATE_NOTICE),
                WorkerContent::ContentImage {
                    data: "AA==".to_string(),
                    mime_type: "image/png".to_string(),
                    id: "img-1".to_string(),
                    is_new: true,
                },
            ]
        );
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
    fn linux_bwrap_startup_retry_matches_worker_ready_failures() {
        for message in [
            "ipc disconnected while waiting for worker_ready",
            "worker session ended before worker_ready",
        ] {
            assert!(
                linux_sandbox_startup_retryable(&WorkerError::Protocol(message.to_string())),
                "expected worker_ready startup failure to be retryable: {message}"
            );
        }
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

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_bwrap_startup_retry_stays_disabled_after_followup_codex_meta_update() {
        let plan = SandboxCliPlan {
            operations: vec![crate::sandbox_cli::SandboxCliOperation::SetMode(
                crate::sandbox_cli::SandboxModeArg::Inherit,
            )],
        };
        let mut manager = WorkerManager::new(
            Backend::Python,
            plan,
            crate::oversized_output::OversizedOutputMode::Files,
        )
        .expect("worker manager");
        let mut inherited_state = manager.sandbox_defaults.clone();
        inherited_state.apply_update(SandboxStateUpdate {
            sandbox_policy: SandboxPolicy::WorkspaceWrite {
                writable_roots: Vec::new(),
                network_access: false,
                exclude_tmpdir_env_var: false,
                exclude_slash_tmp: false,
            },
            sandbox_cwd: Some(std::env::temp_dir()),
            use_linux_sandbox_bwrap: Some(true),
            use_legacy_landlock: None,
        });
        manager.inherited_sandbox_state = Some(inherited_state.clone());
        manager.sandbox_state = resolve_effective_sandbox_state_with_defaults(
            &manager.sandbox_plan,
            Some(&inherited_state),
            &manager.sandbox_defaults,
        )
        .expect("resolved initial sandbox state");
        assert!(
            manager.sandbox_state.use_linux_sandbox_bwrap,
            "test setup should start with bwrap enabled"
        );

        let retry = manager.maybe_retry_spawn_without_linux_bwrap(
            &WorkerError::Protocol("ipc disconnected while waiting for backend info".to_string()),
            false,
        );
        assert!(retry, "expected startup failure to disable bwrap");

        let update = sandbox_state_update_from_codex_meta(&json!({
            "sandboxPolicy": {
                "type": "workspace-write",
                "writable_roots": [],
                "network_access": false,
                "exclude_tmpdir_env_var": false,
                "exclude_slash_tmp": false
            },
            "sandboxCwd": std::env::temp_dir(),
            "useLegacyLandlock": false,
            "codexLinuxSandboxExe": "/tmp/codex-linux-sandbox"
        }))
        .expect("Codex sandbox metadata");
        manager
            .update_sandbox_state(update, Duration::from_millis(1))
            .expect("follow-up sandbox state");

        assert!(
            !manager.sandbox_state.use_linux_sandbox_bwrap,
            "follow-up Codex metadata should preserve the local no-bwrap fallback"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_bwrap_startup_retry_stays_disabled_after_followup_plan_bwrap_override() {
        let plan = SandboxCliPlan {
            operations: vec![
                crate::sandbox_cli::SandboxCliOperation::SetMode(
                    crate::sandbox_cli::SandboxModeArg::Inherit,
                ),
                crate::sandbox_cli::SandboxCliOperation::Config(
                    crate::sandbox_cli::SandboxConfigOperation::SetUseLinuxSandboxBwrap(true),
                ),
            ],
        };
        let mut manager = WorkerManager::new(
            Backend::Python,
            plan,
            crate::oversized_output::OversizedOutputMode::Files,
        )
        .expect("worker manager");
        let mut inherited_state = manager.sandbox_defaults.clone();
        inherited_state.apply_update(SandboxStateUpdate {
            sandbox_policy: SandboxPolicy::WorkspaceWrite {
                writable_roots: Vec::new(),
                network_access: false,
                exclude_tmpdir_env_var: false,
                exclude_slash_tmp: false,
            },
            sandbox_cwd: Some(std::env::temp_dir()),
            use_linux_sandbox_bwrap: None,
            use_legacy_landlock: None,
        });
        manager.inherited_sandbox_state = Some(inherited_state.clone());
        manager.sandbox_state = resolve_effective_sandbox_state_with_defaults(
            &manager.sandbox_plan,
            Some(&inherited_state),
            &manager.sandbox_defaults,
        )
        .expect("resolved initial sandbox state");
        assert!(
            manager.sandbox_state.use_linux_sandbox_bwrap,
            "test setup should start with the plan-level bwrap override enabled"
        );

        let retry = manager.maybe_retry_spawn_without_linux_bwrap(
            &WorkerError::Protocol("ipc disconnected while waiting for backend info".to_string()),
            false,
        );
        assert!(retry, "expected startup failure to disable bwrap");

        let update = sandbox_state_update_from_codex_meta(&json!({
            "sandboxPolicy": {
                "type": "workspace-write",
                "writable_roots": [],
                "network_access": false,
                "exclude_tmpdir_env_var": false,
                "exclude_slash_tmp": false
            },
            "sandboxCwd": std::env::temp_dir(),
            "useLegacyLandlock": false,
            "codexLinuxSandboxExe": "/tmp/codex-linux-sandbox"
        }))
        .expect("Codex sandbox metadata");
        manager
            .update_sandbox_state(update, Duration::from_millis(1))
            .expect("follow-up sandbox state");

        assert!(
            !manager.sandbox_state.use_linux_sandbox_bwrap,
            "plan-level bwrap overrides should not re-enable bwrap after the local fallback"
        );
    }

    #[test]
    fn inherit_ending_invalid_plan_fails_during_startup_validation() {
        let plan = SandboxCliPlan {
            operations: vec![
                crate::sandbox_cli::SandboxCliOperation::SetMode(
                    crate::sandbox_cli::SandboxModeArg::ReadOnly,
                ),
                crate::sandbox_cli::SandboxCliOperation::AddWritableRoot(std::env::temp_dir()),
                crate::sandbox_cli::SandboxCliOperation::SetMode(
                    crate::sandbox_cli::SandboxModeArg::Inherit,
                ),
            ],
        };

        let err = match WorkerManager::new(
            Backend::Python,
            plan,
            crate::oversized_output::OversizedOutputMode::Files,
        ) {
            Ok(_) => panic!("invalid inherit-ending plan should fail during startup"),
            Err(err) => err,
        };

        assert!(
            matches!(err, WorkerError::Sandbox(ref message) if message.contains("--add-writable-root can only be used while sandbox mode is workspace-write")),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn inherit_workspace_write_refinements_wait_for_client_state() {
        let writable_root = std::env::temp_dir();
        let plan = SandboxCliPlan {
            operations: vec![
                crate::sandbox_cli::SandboxCliOperation::SetMode(
                    crate::sandbox_cli::SandboxModeArg::Inherit,
                ),
                crate::sandbox_cli::SandboxCliOperation::AddWritableRoot(writable_root.clone()),
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

        manager
            .stage_sandbox_state_update(SandboxStateUpdate {
                sandbox_policy: SandboxPolicy::WorkspaceWrite {
                    writable_roots: Vec::new(),
                    network_access: false,
                    exclude_tmpdir_env_var: false,
                    exclude_slash_tmp: false,
                },
                sandbox_cwd: Some(writable_root.clone()),
                use_linux_sandbox_bwrap: None,
                use_legacy_landlock: None,
            })
            .expect("workspace-write Codex metadata should satisfy deferred refinements");

        let SandboxPolicy::WorkspaceWrite {
            writable_roots,
            network_access,
            ..
        } = &manager.sandbox_state.sandbox_policy
        else {
            panic!(
                "expected staged inherit refinements to resolve to workspace-write, got {:?}",
                manager.sandbox_state.sandbox_policy
            );
        };
        assert!(
            *network_access,
            "expected deferred workspace network setting to apply after client metadata"
        );
        assert!(
            writable_roots.iter().any(|path| path == &writable_root),
            "expected deferred writable root to apply after client metadata"
        );
    }

    #[test]
    fn failed_sandbox_update_does_not_commit_inherited_state() {
        let _guard = cwd_test_mutex().lock().expect("cwd mutex");
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
        let mut inherited_before = manager.sandbox_defaults.clone();
        inherited_before.apply_update(SandboxStateUpdate {
            sandbox_policy: SandboxPolicy::WorkspaceWrite {
                writable_roots: Vec::new(),
                network_access: false,
                exclude_tmpdir_env_var: false,
                exclude_slash_tmp: false,
            },
            sandbox_cwd: None,
            use_linux_sandbox_bwrap: None,
            use_legacy_landlock: None,
        });
        manager.inherited_sandbox_state = Some(inherited_before.clone());
        manager.sandbox_state = resolve_effective_sandbox_state_with_defaults(
            &manager.sandbox_plan,
            Some(&inherited_before),
            &manager.sandbox_defaults,
        )
        .expect("resolved initial inherited sandbox state");

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
    }

    #[test]
    fn exact_interrupt_remains_local_when_worker_would_respawn() {
        let plan = SandboxCliPlan {
            operations: vec![crate::sandbox_cli::SandboxCliOperation::SetMode(
                crate::sandbox_cli::SandboxModeArg::Inherit,
            )],
        };
        let mut manager = WorkerManager::new(
            Backend::Python,
            plan,
            crate::oversized_output::OversizedOutputMode::Files,
        )
        .expect("worker manager");

        assert!(
            manager
                .nonexecuting_follow_up_uses_existing_state("\u{3}")
                .expect("interrupt follow-up classification"),
            "a bare Ctrl-C should stay a local follow-up even when it would otherwise respawn"
        );
    }

    #[test]
    fn interrupt_pager_tail_requires_current_sandbox_when_worker_would_respawn() {
        let plan = SandboxCliPlan {
            operations: vec![crate::sandbox_cli::SandboxCliOperation::SetMode(
                crate::sandbox_cli::SandboxModeArg::Inherit,
            )],
        };
        let mut manager = WorkerManager::new(
            Backend::R,
            plan,
            crate::oversized_output::OversizedOutputMode::Pager,
        )
        .expect("worker manager");
        let mut process = test_worker_process(successful_test_child());
        process
            .wait_child_for_test()
            .expect("wait for the stub worker process to exit");
        manager.process = Some(process);

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

        assert!(
            !manager
                .nonexecuting_follow_up_uses_existing_state("\u{3}:q")
                .expect("interrupt follow-up classification"),
            "a pager ctrl-c tail should require current per-call sandbox metadata when it would respawn"
        );
    }

    #[test]
    fn empty_input_with_busy_guardrail_uses_existing_state() {
        let plan = SandboxCliPlan {
            operations: vec![crate::sandbox_cli::SandboxCliOperation::SetMode(
                crate::sandbox_cli::SandboxModeArg::Inherit,
            )],
        };
        let mut manager = WorkerManager::new(
            Backend::R,
            plan,
            crate::oversized_output::OversizedOutputMode::Files,
        )
        .expect("worker manager");
        {
            let mut slot = manager
                .guardrail
                .event
                .lock()
                .expect("guardrail event mutex poisoned");
            *slot = Some(GuardrailEvent {
                message: "[repl] previous request aborted; retry your last input\n".to_string(),
                was_busy: true,
                is_error: true,
            });
        }

        assert!(
            !manager
                .empty_input_requires_spawn()
                .expect("empty-input classification"),
            "empty polls should keep pending busy-guardrail recovery local"
        );
    }

    #[test]
    fn nonempty_input_with_busy_guardrail_requires_current_state() {
        let plan = SandboxCliPlan {
            operations: vec![crate::sandbox_cli::SandboxCliOperation::SetMode(
                crate::sandbox_cli::SandboxModeArg::Inherit,
            )],
        };
        let mut manager = WorkerManager::new(
            Backend::R,
            plan,
            crate::oversized_output::OversizedOutputMode::Files,
        )
        .expect("worker manager");
        {
            let mut slot = manager
                .guardrail
                .event
                .lock()
                .expect("guardrail event mutex poisoned");
            *slot = Some(GuardrailEvent {
                message: "[repl] previous request aborted; retry your last input\n".to_string(),
                was_busy: true,
                is_error: true,
            });
        }

        assert!(
            !manager
                .nonexecuting_follow_up_uses_existing_state("1+1")
                .expect("follow-up classification"),
            "busy-guardrail retries should require current per-call sandbox metadata"
        );
    }

    #[test]
    fn empty_input_with_idle_guardrail_requires_spawn() {
        let plan = SandboxCliPlan {
            operations: vec![crate::sandbox_cli::SandboxCliOperation::SetMode(
                crate::sandbox_cli::SandboxModeArg::Inherit,
            )],
        };
        let mut manager = WorkerManager::new(
            Backend::R,
            plan,
            crate::oversized_output::OversizedOutputMode::Files,
        )
        .expect("worker manager");
        {
            let mut slot = manager
                .guardrail
                .event
                .lock()
                .expect("guardrail event mutex poisoned");
            *slot = Some(GuardrailEvent {
                message: "[repl] worker was idle; new session started\n".to_string(),
                was_busy: false,
                is_error: false,
            });
        }

        assert!(
            manager
                .empty_input_requires_spawn()
                .expect("empty-input classification"),
            "idle guardrail notices should still require current per-call sandbox metadata when a poll would respawn"
        );
    }

    #[test]
    fn prechecked_empty_input_requires_current_sandbox_when_worker_exited() {
        let plan = SandboxCliPlan {
            operations: vec![crate::sandbox_cli::SandboxCliOperation::SetMode(
                crate::sandbox_cli::SandboxModeArg::Inherit,
            )],
        };
        let mut manager = WorkerManager::new(
            Backend::R,
            plan,
            crate::oversized_output::OversizedOutputMode::Files,
        )
        .expect("worker manager");
        manager
            .stage_sandbox_state_update(SandboxStateUpdate {
                sandbox_policy: SandboxPolicy::ReadOnly,
                sandbox_cwd: None,
                use_linux_sandbox_bwrap: None,
                use_legacy_landlock: None,
            })
            .expect("initial inherited state");
        let mut process = test_worker_process(successful_test_child());
        process
            .wait_child_for_test()
            .expect("wait for the stub worker process to exit");
        manager.process = Some(process);

        let result = manager.write_stdin(
            String::new(),
            Duration::from_secs(1),
            Duration::from_secs(1),
            WriteStdinOptions {
                pending_state_prechecked: true,
                ..WriteStdinOptions::default()
            },
        );

        assert!(
            matches!(result, Err(ref err) if is_prechecked_follow_up_requires_meta(err)),
            "expected prechecked empty input to require current sandbox metadata once the worker has exited, got: {result:?}"
        );
    }

    #[test]
    fn prechecked_bare_interrupt_requires_current_sandbox_when_worker_exited() {
        let plan = SandboxCliPlan {
            operations: vec![crate::sandbox_cli::SandboxCliOperation::SetMode(
                crate::sandbox_cli::SandboxModeArg::Inherit,
            )],
        };
        let mut manager = WorkerManager::new(
            Backend::R,
            plan,
            crate::oversized_output::OversizedOutputMode::Files,
        )
        .expect("worker manager");
        manager
            .stage_sandbox_state_update(SandboxStateUpdate {
                sandbox_policy: SandboxPolicy::ReadOnly,
                sandbox_cwd: None,
                use_linux_sandbox_bwrap: None,
                use_legacy_landlock: None,
            })
            .expect("initial inherited state");
        let mut process = test_worker_process(successful_test_child());
        process
            .wait_child_for_test()
            .expect("wait for the stub worker process to exit");
        manager.process = Some(process);

        let result = manager.write_stdin(
            "\u{3}".to_string(),
            Duration::from_secs(1),
            Duration::from_secs(1),
            WriteStdinOptions {
                pending_state_prechecked: true,
                ..WriteStdinOptions::default()
            },
        );

        assert!(
            matches!(result, Err(ref err) if is_prechecked_follow_up_requires_meta(err)),
            "expected prechecked bare ctrl-c to require current sandbox metadata once the worker has exited, got: {result:?}"
        );
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn interrupt_tail_uses_current_sandbox_for_the_respawn() {
        let _guard = cwd_test_mutex().lock().expect("cwd mutex");
        let temp = tempfile::Builder::new()
            .prefix(".tmp-interrupt-tail-current-sandbox-")
            .tempdir_in(worker_process_test_temp_parent("worker-process"))
            .expect("tempdir");
        let sandbox_cwd = temp.path().to_path_buf();
        let plan = SandboxCliPlan {
            operations: vec![crate::sandbox_cli::SandboxCliOperation::SetMode(
                crate::sandbox_cli::SandboxModeArg::Inherit,
            )],
        };
        let mut manager = WorkerManager::new(
            Backend::R,
            plan,
            crate::oversized_output::OversizedOutputMode::Files,
        )
        .expect("worker manager");
        manager
            .stage_sandbox_state_update(SandboxStateUpdate {
                sandbox_policy: SandboxPolicy::ReadOnly,
                sandbox_cwd: Some(sandbox_cwd.clone()),
                use_linux_sandbox_bwrap: None,
                use_legacy_landlock: None,
            })
            .expect("initial inherited read-only state");
        let mut process = test_worker_process(successful_test_child());
        process
            .wait_child_for_test()
            .expect("wait for the stub worker process to exit");
        manager.process = Some(process);
        manager.exe_path = PathBuf::from("definitely-missing-worker-exe");

        let result = manager.write_stdin(
            "\u{3}1+1".to_string(),
            Duration::from_secs(10),
            Duration::from_secs(10),
            WriteStdinOptions {
                deferred_sandbox_state_update: Some(SandboxStateUpdate {
                    sandbox_policy: SandboxPolicy::WorkspaceWrite {
                        writable_roots: Vec::new(),
                        network_access: false,
                        exclude_tmpdir_env_var: false,
                        exclude_slash_tmp: false,
                    },
                    sandbox_cwd: Some(sandbox_cwd.clone()),
                    use_linux_sandbox_bwrap: None,
                    use_legacy_landlock: None,
                }),
                ..WriteStdinOptions::default()
            },
        );
        match result {
            Ok(WorkerReply::Output {
                contents, is_error, ..
            }) => {
                let text = contents_text(&contents);
                assert!(
                    is_error,
                    "expected the failed interrupt-tail respawn attempt to surface as an error reply"
                );
                assert!(
                    text.contains("worker error:"),
                    "expected the failed interrupt-tail respawn attempt to report a worker error, got: {text:?}"
                );
            }
            Err(WorkerError::Protocol(message)) => {
                assert!(
                    message.contains("backend info") || message.contains("ipc disconnected"),
                    "expected the failed interrupt-tail respawn attempt to fail during worker startup, got: {message:?}"
                );
            }
            Err(err) => panic!("unexpected interrupt-tail respawn error: {err}"),
        }
        assert!(
            matches!(
                manager.sandbox_state.sandbox_policy,
                SandboxPolicy::WorkspaceWrite { .. }
            ),
            "expected deferred metadata to stage before interrupt attempts the respawn"
        );
        assert_eq!(
            manager.sandbox_state.sandbox_cwd, sandbox_cwd,
            "expected deferred metadata to update the effective sandbox cwd before the respawn path"
        );
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
            WINDOWS_IPC_CONNECT_MAX_WAIT <= Duration::from_secs(10),
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
    fn windows_python_sandbox_prepare_access_denied_fails_fast() {
        let _guard = crate::windows_sandbox::prepare_sandbox_launch_test_mutex()
            .lock()
            .expect("windows sandbox test mutex");
        crate::windows_sandbox::set_prepare_sandbox_launch_test_error(Some(
            "failed to prepare writable ACL on 'C:\\workspace': SetNamedSecurityInfoW failed: 5"
                .to_string(),
        ));

        let mut manager = WorkerManager::new(
            Backend::Python,
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
            "Python access-denied prepare failures should abort launch preparation, got: {result:?}"
        );
        assert!(
            manager.windows_sandbox_launch.is_none(),
            "failed Python launch preparation should not cache a prepared launch"
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
