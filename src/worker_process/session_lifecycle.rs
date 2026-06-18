use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::oversized_output::OversizedOutputMode;
use crate::sandbox::SandboxStateUpdate;
use crate::worker_protocol::ContentOrigin;

use super::{WORKER_SHUTDOWN_TIMEOUT, WorkerError, WorkerManager};

impl WorkerManager {
    pub(super) fn reset_preserving_detached_prefix_item_count(
        &mut self,
    ) -> Result<(), WorkerError> {
        let detached_prefix_item_count = self.last_detached_prefix_item_count;
        let result = self.reset();
        self.last_detached_prefix_item_count = detached_prefix_item_count;
        result
    }

    pub(super) fn reset_with_pager_preserving_detached_prefix_item_count(
        &mut self,
        preserve_pager: bool,
    ) -> Result<(), WorkerError> {
        let detached_prefix_item_count = self.last_detached_prefix_item_count;
        let result = self.reset_with_pager(preserve_pager);
        self.last_detached_prefix_item_count = detached_prefix_item_count;
        result
    }

    pub(super) fn note_session_end(&mut self, include_notice: bool) {
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

    pub(super) fn maybe_reset_after_session_end(&mut self) {
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

    pub(super) fn maybe_reset_after_session_end_with_options(
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

    pub fn shutdown(&mut self) {
        crate::event_log::log("worker_shutdown", serde_json::json!({}));
        if let Some(process) = self.process.take() {
            let _ = process.shutdown_graceful(WORKER_SHUTDOWN_TIMEOUT);
        }
        self.guardrail.busy.store(false, Ordering::Relaxed);
    }

    pub(super) fn ensure_process(&mut self) -> Result<(), WorkerError> {
        self.require_inherited_sandbox_state()?;
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

    pub(super) fn reset(&mut self) -> Result<(), WorkerError> {
        crate::event_log::log("worker_reset_begin", serde_json::json!({}));
        if let Some(process) = self.process.take() {
            let _ = process.kill();
        }
        self.require_inherited_sandbox_state()?;
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

    pub(super) fn reset_with_pager(&mut self, preserve_pager: bool) -> Result<(), WorkerError> {
        crate::event_log::log(
            "worker_reset_with_pager_begin",
            serde_json::json!({
                "preserve_pager": preserve_pager,
            }),
        );
        if let Some(process) = self.process.take() {
            let _ = process.kill();
        }
        self.require_inherited_sandbox_state()?;
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

    pub(super) fn spawn_worker_after_initial_sandbox_state(&mut self) -> Result<(), WorkerError> {
        match self.oversized_output {
            OversizedOutputMode::Files => self.reset_output_state_files(true),
            OversizedOutputMode::Pager => self.reset_output_state_pager(true, false),
        }
        self.process = Some(match self.oversized_output {
            OversizedOutputMode::Files => self.spawn_process_files()?,
            OversizedOutputMode::Pager => self.spawn_process_with_pager(false)?,
        });
        self.note_respawn_during_write();
        Ok(())
    }

    pub(super) fn restart_worker_after_sandbox_state_change(
        &mut self,
        timeout: Duration,
    ) -> Result<(), WorkerError> {
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
        self.note_respawn_during_write();
        Ok(())
    }
}
