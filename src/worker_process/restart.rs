use std::sync::atomic::Ordering;
use std::time::Duration;

use super::{WorkerError, WorkerManager};
use crate::completion_reply::{PagerCompletionPrompt, ReplyWithOffset};
#[cfg(any(debug_assertions, test))]
use crate::oversized_output::OversizedOutputMode;
use crate::pager;
use crate::pending_output_tape::FormattedPendingOutput;
use crate::worker_protocol::WorkerReply;

#[derive(Clone, Copy)]
enum RestartMode {
    Files,
    Pager,
}

enum RestartShutdownSnapshot {
    Files {
        output: Option<FormattedPendingOutput>,
    },
    Pager {
        end_offset: Option<u64>,
    },
}

impl WorkerManager {
    #[cfg(debug_assertions)]
    pub fn restart(&mut self, timeout: Duration) -> Result<WorkerReply, WorkerError> {
        match self.oversized_output {
            OversizedOutputMode::Files => self.restart_files(timeout),
            OversizedOutputMode::Pager => self.restart_pager(timeout),
        }
    }

    pub(super) fn restart_files(&mut self, timeout: Duration) -> Result<WorkerReply, WorkerError> {
        self.restart_for_mode(RestartMode::Files, timeout)
    }

    pub(super) fn restart_pager(&mut self, timeout: Duration) -> Result<WorkerReply, WorkerError> {
        self.restart_for_mode(RestartMode::Pager, timeout)
    }

    fn restart_for_mode(
        &mut self,
        mode: RestartMode,
        timeout: Duration,
    ) -> Result<WorkerReply, WorkerError> {
        Self::begin_restart(timeout);
        self.require_inherited_sandbox_state()?;
        self.maybe_emit_pending_server_notice();
        let snapshot = self.shutdown_existing_worker_for_restart(mode, timeout);
        self.clear_restart_busy_guardrail();
        let reply = self.build_restart_reply_for_mode(snapshot);
        self.finish_restart_for_mode(mode);
        Self::end_restart_ok();
        Ok(self.finalize_reply(reply))
    }

    fn begin_restart(timeout: Duration) {
        crate::event_log::log(
            "worker_restart_begin",
            serde_json::json!({
                "timeout_ms": timeout.as_millis(),
            }),
        );
    }

    fn end_restart_ok() {
        crate::event_log::log("worker_restart_end", serde_json::json!({"status": "ok"}));
    }

    fn shutdown_existing_worker_for_restart(
        &mut self,
        mode: RestartMode,
        timeout: Duration,
    ) -> RestartShutdownSnapshot {
        match mode {
            RestartMode::Files => self.shutdown_existing_files_worker_for_restart(timeout),
            RestartMode::Pager => self.shutdown_existing_pager_worker_for_restart(timeout),
        }
    }

    fn shutdown_existing_files_worker_for_restart(
        &mut self,
        timeout: Duration,
    ) -> RestartShutdownSnapshot {
        let had_process = self.process.is_some();
        if let Some(process) = self.process.take() {
            let _ = process.shutdown_for_restart(timeout);
        }
        let output = had_process.then(|| self.drain_sealed_formatted_output());
        RestartShutdownSnapshot::Files { output }
    }

    fn shutdown_existing_pager_worker_for_restart(
        &mut self,
        timeout: Duration,
    ) -> RestartShutdownSnapshot {
        self.output_timeline.seal_utf8_tails();
        if let Some(process) = self.process.take() {
            let _ = process.shutdown_for_restart(timeout);
        }
        self.output_timeline.seal_utf8_tails();
        let end_offset = self.output.end_offset();
        RestartShutdownSnapshot::Pager { end_offset }
    }

    fn clear_restart_busy_guardrail(&self) {
        self.guardrail.busy.store(false, Ordering::Relaxed);
    }

    fn build_restart_reply_for_mode(
        &mut self,
        snapshot: RestartShutdownSnapshot,
    ) -> ReplyWithOffset {
        match snapshot {
            RestartShutdownSnapshot::Files { output } => match output {
                Some(output) => self
                    .build_session_reset_reply_files_from_formatted("new session started", output),
                None => self.build_session_reset_reply_files("new session started"),
            },
            RestartShutdownSnapshot::Pager { end_offset } => {
                self.build_restart_reply_pager(end_offset)
            }
        }
    }

    fn build_restart_reply_pager(&mut self, end_offset: Option<u64>) -> ReplyWithOffset {
        let page_bytes = pager::resolve_page_bytes(None);
        match end_offset {
            Some(end_offset) => {
                let reply = self.build_session_reset_reply_pager_to_offset(
                    page_bytes,
                    "new session started",
                    end_offset,
                );
                self.output.advance_offset_to(end_offset);
                reply
            }
            None => self.build_session_reset_reply_pager(page_bytes, "new session started"),
        }
    }

    fn finish_restart_for_mode(&mut self, mode: RestartMode) {
        self.clear_preserved_prefixes();
        match mode {
            RestartMode::Files => self.reset_output_state_files(true),
            RestartMode::Pager => {
                let preserve_pager = self.pager.is_active();
                self.reset_output_state_pager(true, preserve_pager);
                if preserve_pager {
                    self.pager_prompt = Some(PagerCompletionPrompt::PromptFree);
                }
            }
        }
        self.note_respawn_during_write();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Backend;
    use crate::sandbox_cli::SandboxCliPlan;
    use crate::worker_process::WriteStdinOptions;
    use crate::worker_process::output_state::PrefixCapture;
    use crate::worker_process::test_support::contents_text;
    use crate::worker_protocol::WorkerContent;

    #[test]
    fn bare_restart_flushes_queued_sandbox_change_notice() {
        let mut manager = WorkerManager::new(
            Backend::R,
            SandboxCliPlan::default(),
            OversizedOutputMode::Files,
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
    fn bare_restart_clears_preserved_detached_prefixes() {
        let mut manager = WorkerManager::new(
            Backend::R,
            SandboxCliPlan::default(),
            OversizedOutputMode::Files,
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
}
