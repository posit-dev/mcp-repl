use std::sync::atomic::Ordering;
use std::time::Duration;

use super::{WorkerError, WorkerManager};
use crate::completion_reply::ReplyWithOffset;
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
        pre_shutdown_end_offset: Option<u64>,
        post_shutdown_end_offset: Option<u64>,
    },
}

impl WorkerManager {
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
        let output = self
            .process
            .is_some()
            .then(|| self.drain_sealed_formatted_output());
        if let Some(process) = self.process.take() {
            let _ = process.shutdown_graceful(timeout);
            self.pending_output_tape.clear();
        }
        RestartShutdownSnapshot::Files { output }
    }

    fn shutdown_existing_pager_worker_for_restart(
        &mut self,
        timeout: Duration,
    ) -> RestartShutdownSnapshot {
        let pre_shutdown_end_offset = self
            .process
            .is_some()
            .then(|| self.output.end_offset().unwrap_or(0));
        if let Some(process) = self.process.take() {
            let _ = process.shutdown_graceful(timeout);
        }
        let post_shutdown_end_offset = self.output.end_offset();
        RestartShutdownSnapshot::Pager {
            pre_shutdown_end_offset,
            post_shutdown_end_offset,
        }
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
            RestartShutdownSnapshot::Pager {
                pre_shutdown_end_offset,
                post_shutdown_end_offset,
            } => self.build_restart_reply_pager(pre_shutdown_end_offset, post_shutdown_end_offset),
        }
    }

    fn build_restart_reply_pager(
        &mut self,
        pre_shutdown_end_offset: Option<u64>,
        post_shutdown_end_offset: Option<u64>,
    ) -> ReplyWithOffset {
        let page_bytes = pager::resolve_page_bytes(None);
        match pre_shutdown_end_offset {
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
        }
    }

    fn finish_restart_for_mode(&mut self, mode: RestartMode) {
        self.clear_preserved_prefixes();
        match mode {
            RestartMode::Files => self.reset_output_state_files(true),
            RestartMode::Pager => self.reset_output_state_pager(true, false),
        }
        self.note_respawn_during_write();
    }
}
