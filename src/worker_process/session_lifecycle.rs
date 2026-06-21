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

    pub(super) fn reset_after_session_end_preserving_detached_prefix_item_count(
        &mut self,
    ) -> Result<(), WorkerError> {
        let detached_prefix_item_count = self.last_detached_prefix_item_count;
        let result = self.reset_after_session_end();
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

    pub(super) fn reset_after_session_end_with_pager_preserving_detached_prefix_item_count(
        &mut self,
        preserve_pager: bool,
    ) -> Result<(), WorkerError> {
        let detached_prefix_item_count = self.last_detached_prefix_item_count;
        let result = self.reset_after_session_end_with_pager(preserve_pager);
        self.last_detached_prefix_item_count = detached_prefix_item_count;
        result
    }

    pub(super) fn note_session_end(&mut self, include_notice: bool) {
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

    pub(super) fn maybe_reset_after_session_end(&mut self) {
        if self.session_end_seen {
            let result = match self.oversized_output {
                OversizedOutputMode::Files => {
                    self.reset_after_session_end_preserving_detached_prefix_item_count()
                }
                OversizedOutputMode::Pager => self
                    .reset_after_session_end_with_pager_preserving_detached_prefix_item_count(
                        self.pager.is_active(),
                    ),
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

    pub(super) fn reset_after_session_end(&mut self) -> Result<(), WorkerError> {
        self.reset_after_session_end_for_mode(false)
    }

    pub(super) fn reset_after_session_end_with_pager(
        &mut self,
        preserve_pager: bool,
    ) -> Result<(), WorkerError> {
        self.reset_after_session_end_for_mode(preserve_pager)
    }

    fn reset_after_session_end_for_mode(
        &mut self,
        preserve_pager: bool,
    ) -> Result<(), WorkerError> {
        crate::event_log::log("worker_session_end_reset_begin", serde_json::json!({}));
        if let Some(process) = self.process.take() {
            let _ = process.finish_session_end_for_respawn();
        }
        self.require_inherited_sandbox_state()?;
        match self.oversized_output {
            OversizedOutputMode::Files => self.reset_output_state_files(true),
            OversizedOutputMode::Pager => self.reset_output_state_pager(true, preserve_pager),
        }
        self.process = Some(match self.oversized_output {
            OversizedOutputMode::Files => self.spawn_process_files()?,
            OversizedOutputMode::Pager => self.spawn_process_with_pager(false)?,
        });
        crate::event_log::log(
            "worker_session_end_reset_end",
            serde_json::json!({"status": "ok"}),
        );
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Backend;
    use crate::oversized_output::OversizedOutputMode;
    use crate::sandbox_cli::SandboxCliPlan;

    #[test]
    fn session_end_reset_preserves_detached_prefix_count() {
        let mut manager = WorkerManager::new(
            Backend::R,
            SandboxCliPlan::default(),
            OversizedOutputMode::Files,
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
    fn failing_session_end_notice_flushes_partial_stdout_in_files_mode() {
        use crate::worker_process::test_support::{
            contents_text, env_test_mutex, failing_test_status, sleeping_test_child,
            test_worker_process,
        };

        let _guard = env_test_mutex().lock().expect("env mutex");
        let mut manager = WorkerManager::new(
            Backend::R,
            SandboxCliPlan::default(),
            OversizedOutputMode::Files,
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
        use crate::ipc::WorkerToServerIpcMessage;
        use crate::worker_process::test_support::{
            contents_text, env_test_mutex, successful_test_child, test_worker_process,
        };

        let _guard = env_test_mutex().lock().expect("env mutex");
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");
        let mut manager = WorkerManager::new(
            Backend::Python,
            SandboxCliPlan::default(),
            OversizedOutputMode::Files,
        )
        .expect("worker manager");
        let mut process = test_worker_process(successful_test_child());
        let status = process.wait_child_for_test().expect("wait test child");
        process.set_exit_status_for_test(status);
        let _ = worker.send(WorkerToServerIpcMessage::InputWait {
            prompt: ">>> ".to_string(),
        });
        server
            .wait_for_input_wait(Duration::from_millis(200))
            .expect("server observes initial input_wait");
        server.begin_input().expect("begin input");
        process.set_ipc_for_test(server);
        manager.process = Some(process);
        manager.pending_request = true;
        manager.pending_request_started_at = Some(std::time::Instant::now());
        manager.pending_request_input = Some("quit()\n".to_string());

        let prompt = ">>> ".to_string();
        let _ = worker.send(WorkerToServerIpcMessage::InputLine {
            prompt,
            text: "quit()\n".to_string(),
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
}
