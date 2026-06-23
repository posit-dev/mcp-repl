use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex, atomic::AtomicBool};
use std::time::Duration;

use crate::backend::{Backend, WorkerLaunch};
use crate::completion_reply::{CompletionInfo, PagerCompletionPrompt};
use crate::output_capture::{
    OUTPUT_RING_CAPACITY_BYTES, OutputBuffer, OutputTimeline, ensure_output_ring, reset_output_ring,
};
use crate::oversized_output::OversizedOutputMode;
use crate::pager::Pager;
use crate::pending_output_tape::PendingOutputTape;
use crate::sandbox::{SandboxState, SandboxStateUpdate};
use crate::sandbox_cli::SandboxCliPlan;
pub(crate) use crate::stdin_payload::{WriteStdinControlAction, split_write_stdin_control_prefix};
use crate::worker_protocol::WorkerReply;
use crate::worker_supervisor::{GuardrailEvent, GuardrailShared, WorkerProcess};

mod backend_driver;
mod control_prefix;
mod interrupt;
mod output_state;
mod pending_poll;
mod reply_state;
mod request_lifecycle;
mod request_reply;
mod restart;
mod sandbox_state;
mod session_lifecycle;
mod session_reset_reply;
#[cfg(test)]
mod test_support;
mod worker_launch;
mod write_dispatch;
mod write_flow;
mod write_preflight;

use self::backend_driver::{BackendDriver, new_backend_driver};
use self::output_state::PrefixCapture;
use self::request_lifecycle::RequestState;
pub(crate) use self::write_flow::WriteStdinOptions;

#[cfg(target_family = "unix")]
use std::os::unix::process::CommandExt;

const WORKER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const PREVIOUS_IMAGE_UPDATE_NOTICE: &str =
    "[repl] image update from previous request shown as a new image\n";
const PRECHECKED_FOLLOW_UP_REQUIRES_META_MESSAGE: &str =
    "worker follow-up needs current sandbox metadata after precheck";

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

impl From<std::io::Error> for WorkerError {
    fn from(err: std::io::Error) -> Self {
        WorkerError::Io(err)
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

fn configured_python_executable_hint(worker_launch: &WorkerLaunch) -> Option<PathBuf> {
    if worker_launch.builtin_backend() != Some(Backend::Python) {
        return None;
    }
    worker_launch
        .python_executable()
        .map(Path::to_path_buf)
        .or_else(|| {
            std::env::var_os(crate::python_runtime::PYTHON_EXECUTABLE_ENV).map(PathBuf::from)
        })
}

pub struct WorkerManager {
    exe_path: PathBuf,
    worker_launch: WorkerLaunch,
    active_python_executable_hint: Option<PathBuf>,
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
    user_state_may_exist: bool,
    session_end_seen: bool,
    settled_pending_completion: Option<CompletionInfo>,
    settled_pending_error: Option<WorkerError>,
    preserved_detached_prefix: PrefixCapture,
    reply_owned_prefix: PrefixCapture,
    next_live_prefix_belongs_to_reply: bool,
    last_detached_prefix_item_count: usize,
    pager_prompt: Option<PagerCompletionPrompt>,
    last_prompt: Option<String>,
    last_spawn: Option<std::time::Instant>,
    spawn_count: u64,
    guardrail: GuardrailShared,
    pending_server_notice: Option<GuardrailEvent>,
    write_in_progress: bool,
    last_write_respawned: bool,
    #[cfg(target_os = "linux")]
    linux_bwrap_fallback_disabled: bool,
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
        let active_python_executable_hint = configured_python_executable_hint(&worker_launch);
        let sandbox_defaults = crate::sandbox::sandbox_state_defaults_with_environment();
        let initial_sandbox =
            sandbox_state::prepare_initial_sandbox_state(&sandbox_plan, &sandbox_defaults)?;
        let sandbox_state = initial_sandbox.state;
        if initial_sandbox.awaiting_inherited_state {
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
        #[cfg(test)]
        let _output_ring_guard = crate::output_capture::output_ring_test_mutex()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let output_timeline = {
            let output_ring = ensure_output_ring(OUTPUT_RING_CAPACITY_BYTES);
            reset_output_ring();
            OutputTimeline::new(output_ring)
        };
        Ok(Self {
            exe_path,
            worker_launch: worker_launch.clone(),
            active_python_executable_hint,
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
            driver: new_backend_driver(&worker_launch),
            pending_request: false,
            pending_request_started_at: None,
            pending_request_input: None,
            user_state_may_exist: false,
            session_end_seen: false,
            settled_pending_completion: None,
            settled_pending_error: None,
            preserved_detached_prefix: PrefixCapture::default(),
            reply_owned_prefix: PrefixCapture::default(),
            next_live_prefix_belongs_to_reply: false,
            last_detached_prefix_item_count: 0,
            pager_prompt: None,
            last_prompt: None,
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
        let Some(config) =
            sandbox_state::managed_network_proxy_config_for_state(&self.sandbox_state)?
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

    /// Exposes whether a timed-out logical request still owns future empty-input polls.
    pub fn pending_request(&self) -> bool {
        self.pending_request
    }

    pub fn user_state_may_exist(&self) -> bool {
        self.user_state_may_exist
    }

    pub fn active_python_executable_hint(&self) -> Option<std::path::PathBuf> {
        if self.backend != Backend::Python {
            return None;
        }
        self.active_python_executable_hint.clone()
    }

    pub fn python_executable_hint_matches(&self, target: &std::path::Path) -> bool {
        self.active_python_executable_hint()
            .as_deref()
            .is_some_and(|current| crate::python_prepare::same_python_executable(current, target))
    }

    pub fn prepare_command_sandbox_state(
        &self,
        update: Option<SandboxStateUpdate>,
    ) -> Result<SandboxState, WorkerError> {
        let Some(update) = update else {
            self.require_inherited_sandbox_state()?;
            return Ok(self.sandbox_state.clone());
        };

        let mut inherited_state = self.sandbox_defaults.clone();
        inherited_state.apply_update(update);
        #[cfg(target_os = "linux")]
        self.apply_linux_bwrap_fallback_override(&mut inherited_state);
        let resolved_state = crate::sandbox_cli::resolve_effective_sandbox_state_with_defaults(
            &self.sandbox_plan,
            Some(&inherited_state),
            &self.sandbox_defaults,
        )
        .map_err(WorkerError::Sandbox)?;
        #[cfg(target_os = "linux")]
        {
            let mut resolved_state = resolved_state;
            self.apply_linux_bwrap_fallback_override(&mut resolved_state);
            Ok(resolved_state)
        }
        #[cfg(not(target_os = "linux"))]
        {
            Ok(resolved_state)
        }
    }

    pub fn run_sandboxed_prepare_command(
        &mut self,
        sandbox_state: &SandboxState,
        program: &Path,
        args: &[String],
    ) -> Result<Output, WorkerError> {
        let managed_network_proxy =
            match sandbox_state::managed_network_proxy_config_for_state(sandbox_state)? {
                Some(config) => Some(
                    crate::managed_network::ManagedNetworkProxy::start(config)
                        .map_err(|err| WorkerError::Sandbox(err.to_string()))?,
                ),
                None => None,
            };
        let prepared = crate::sandbox::prepare_worker_command_with_managed_network(
            program,
            args.to_vec(),
            sandbox_state,
            managed_network_proxy.as_ref(),
        )
        .map_err(|err| WorkerError::Sandbox(err.to_string()))?;
        #[cfg(target_os = "windows")]
        let mut prepared = prepared;

        #[cfg(target_os = "windows")]
        let prepared_windows_launch = if sandbox_state.sandbox_policy.requires_sandbox() {
            Some(
                crate::windows_sandbox::prepare_sandbox_launch(
                    &sandbox_state.sandbox_policy,
                    &sandbox_state.sandbox_cwd,
                    &sandbox_state.session_temp_dir,
                )
                .map_err(WorkerError::Sandbox)?,
            )
        } else {
            None
        };
        #[cfg(target_os = "windows")]
        if let Some(prepared_windows_launch) = prepared_windows_launch.as_ref() {
            crate::sandbox::append_windows_prepared_capability_sid(
                &mut prepared.args,
                prepared_windows_launch.capability_sid(),
            )
            .map_err(WorkerError::Sandbox)?;
        }

        let mut command = Command::new(&prepared.program);
        #[cfg(target_family = "unix")]
        if let Some(arg0) = &prepared.arg0 {
            command.arg0(arg0);
        }
        command.args(&prepared.args);
        command.envs(prepared.env.iter());
        command.stdin(Stdio::null());
        command.output().map_err(WorkerError::Io)
    }

    pub fn refresh_timeout_marker_with_wait(&mut self, wait: Duration) {
        self.resolve_timeout_marker_with_wait(wait);
    }

    pub fn missing_inherited_state_without_worker(&self) -> bool {
        self.missing_inherited_sandbox_state() && self.process.is_none()
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
}

#[cfg(test)]
mod tests {
    #[cfg(target_family = "unix")]
    use super::test_support::{cwd_test_mutex, env_test_mutex};
    use super::*;

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

    #[cfg(target_family = "unix")]
    #[test]
    fn worker_manager_new_does_not_panic_for_non_utf8_tmpdir_env() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        use std::path::PathBuf;

        let _guard = env_test_mutex().lock().expect("env mutex");
        let _guard = cwd_test_mutex().lock().expect("cwd mutex");
        let original_tmpdir = std::env::var_os("TMPDIR");
        let non_utf8_tmpdir = PathBuf::from(OsString::from_vec(b"/tmp/non-utf8-\xFF-tmp".to_vec()));
        #[cfg(target_os = "linux")]
        std::fs::create_dir_all(&non_utf8_tmpdir).expect("create non-UTF-8 TMPDIR parent");

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
        #[cfg(target_os = "linux")]
        let _ = std::fs::remove_dir(&non_utf8_tmpdir);

        assert!(result.is_ok(), "WorkerManager::new should not panic");
    }
}
