#[cfg(any(test, target_os = "windows"))]
use crate::backend::Backend;
#[cfg(target_os = "linux")]
use crate::oversized_output::OversizedOutputMode;
use crate::reply_presentation::normalize_prompt;
#[cfg(target_os = "linux")]
use crate::worker_protocol::ContentOrigin;
#[cfg(target_os = "linux")]
use crate::worker_supervisor::linux_sandbox_startup_retryable;
use crate::worker_supervisor::{
    InitialWorkerPrompt, SupervisorSpawn, WorkerProcess, WorkerSpawnContext, WorkerSupervisor,
};

#[cfg(target_os = "windows")]
use super::worker_context_event_payload;
use super::{WorkerError, WorkerManager};

#[cfg(target_os = "linux")]
const LINUX_BWRAP_FALLBACK_NOTICE: &str =
    "[repl] Linux bubblewrap sandbox unavailable; continuing without bwrap\n";

#[cfg(any(test, target_os = "windows"))]
pub(super) fn backend_prepares_windows_sandbox_launch(backend: Backend) -> bool {
    matches!(backend, Backend::R | Backend::Python)
}

impl WorkerManager {
    pub(super) fn spawn_process_files(&mut self) -> Result<WorkerProcess, WorkerError> {
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

    pub(super) fn spawn_process_with_pager(
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
    pub(super) fn maybe_retry_spawn_without_linux_bwrap(
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
    pub(super) fn ensure_windows_sandbox_launch(
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
}
