use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::completion_reply::ReplyWithOffset;
use crate::worker_protocol::WorkerReply;

use super::reply_state::mark_busy_follow_up_reply;
use super::write_flow::{WriteStdinMode, WriteStdinOptions};
use super::{WorkerError, WorkerManager, prechecked_follow_up_requires_meta_error};

pub(super) struct WritePreflightInput<'a> {
    pub(super) mode: WriteStdinMode,
    pub(super) text: &'a str,
    pub(super) worker_timeout: Duration,
    pub(super) page_bytes: u64,
    pub(super) options: &'a WriteStdinOptions,
}

pub(super) enum WritePreflightOutcome {
    Continue,
    Reply(WorkerReply),
}

impl WorkerManager {
    pub(super) fn write_preflight(
        &mut self,
        input: WritePreflightInput<'_>,
    ) -> Result<WritePreflightOutcome, WorkerError> {
        if let Some(outcome) = self.guardrail_busy_abort(&input)? {
            return Ok(outcome);
        }

        if let Some(outcome) = self.handle_nonempty_pager_command(&input)? {
            return Ok(outcome);
        }

        match input.mode {
            WriteStdinMode::Files => self.write_preflight_files(input),
            WriteStdinMode::Pager => self.write_preflight_pager(input),
        }
    }

    fn write_preflight_files(
        &mut self,
        input: WritePreflightInput<'_>,
    ) -> Result<WritePreflightOutcome, WorkerError> {
        self.emit_guardrail_notice_and_resolve_timeout_marker(&input);

        if input.text.is_empty() {
            if let Some(outcome) = self.empty_input_pending_poll(&input)? {
                return Ok(outcome);
            }
            self.reject_prechecked_empty_follow_up_without_current_state(&input)?;
            if let Some(outcome) = self.ensure_process_or_error_reply(&input)? {
                return Ok(outcome);
            }
            let reply = self.build_idle_reply_for_mode(input.mode);
            return self.finish_default_preflight_reply(reply);
        }

        if let Some(outcome) = self.settle_or_reply_to_pending_request_follow_up(&input)? {
            return Ok(outcome);
        }

        Ok(WritePreflightOutcome::Continue)
    }

    fn write_preflight_pager(
        &mut self,
        input: WritePreflightInput<'_>,
    ) -> Result<WritePreflightOutcome, WorkerError> {
        if input.text.is_empty() {
            self.output.start_capture();
            self.emit_guardrail_notice_and_resolve_timeout_marker(&input);
            if let Some(outcome) = self.empty_input_pending_poll(&input)? {
                return Ok(outcome);
            }
            if let Some(outcome) = self.empty_input_pager_command(&input)? {
                return Ok(outcome);
            }
            self.reject_prechecked_empty_follow_up_without_current_state(&input)?;
            if let Some(outcome) = self.ensure_process_or_error_reply(&input)? {
                return Ok(outcome);
            }
            let reply = self.build_idle_reply_for_mode(input.mode);
            return self.finish_default_preflight_reply(reply);
        }

        if let Some(outcome) = self.ensure_process_or_error_reply(&input)? {
            return Ok(outcome);
        }
        self.output.start_capture();
        self.emit_guardrail_notice_and_resolve_timeout_marker(&input);
        if let Some(outcome) = self.settle_or_reply_to_pending_request_follow_up(&input)? {
            return Ok(outcome);
        }

        Ok(WritePreflightOutcome::Continue)
    }

    fn guardrail_busy_abort(
        &mut self,
        input: &WritePreflightInput<'_>,
    ) -> Result<Option<WritePreflightOutcome>, WorkerError> {
        if !self.guardrail_busy_event_pending() {
            return Ok(None);
        }

        self.maybe_emit_guardrail_notice();
        let event = self
            .guardrail
            .event
            .lock()
            .expect("guardrail event mutex poisoned")
            .take()
            .expect("guardrail event should be present");
        self.guardrail.busy.store(false, Ordering::Relaxed);
        let err = WorkerError::Guardrail(event.message);
        let reply = self.build_worker_error_reply_for_mode(&err, input);
        self.reset_after_worker_error(input.mode);
        self.finish_default_preflight_reply(reply).map(Some)
    }

    fn handle_nonempty_pager_command(
        &mut self,
        input: &WritePreflightInput<'_>,
    ) -> Result<Option<WritePreflightOutcome>, WorkerError> {
        if !matches!(input.mode, WriteStdinMode::Pager)
            || input.text.is_empty()
            || !self.pager.is_active()
        {
            return Ok(None);
        }

        let trimmed = input.text.trim();
        if trimmed.is_empty() || trimmed.starts_with(':') {
            if let Some(reply) = self.handle_pager_command(input.text) {
                return self.finish_pager_local_reply(reply).map(Some);
            }
        } else {
            self.pager.dismiss();
            self.pager_prompt = None;
        }
        Ok(None)
    }

    fn empty_input_pager_command(
        &mut self,
        input: &WritePreflightInput<'_>,
    ) -> Result<Option<WritePreflightOutcome>, WorkerError> {
        if matches!(input.mode, WriteStdinMode::Pager)
            && self.pager.is_active()
            && let Some(reply) = self.handle_pager_command(input.text)
        {
            return self.finish_pager_local_reply(reply).map(Some);
        }
        Ok(None)
    }

    fn emit_guardrail_notice_and_resolve_timeout_marker(
        &mut self,
        input: &WritePreflightInput<'_>,
    ) {
        self.maybe_emit_guardrail_notice();
        if !input.options.pending_state_prechecked {
            self.resolve_timeout_marker();
        }
    }

    fn empty_input_pending_poll(
        &mut self,
        input: &WritePreflightInput<'_>,
    ) -> Result<Option<WritePreflightOutcome>, WorkerError> {
        if !self.empty_input_has_pending_output(input.mode) {
            return Ok(None);
        }

        let reply = self.poll_pending_output_for_mode(input, input.worker_timeout)?;
        self.finish_caller_preflight_reply(reply, input).map(Some)
    }

    fn reject_prechecked_empty_follow_up_without_current_state(
        &mut self,
        input: &WritePreflightInput<'_>,
    ) -> Result<(), WorkerError> {
        if input.options.pending_state_prechecked && self.control_only_interrupt_requires_spawn()? {
            return Err(prechecked_follow_up_requires_meta_error());
        }
        Ok(())
    }

    fn ensure_process_or_error_reply(
        &mut self,
        input: &WritePreflightInput<'_>,
    ) -> Result<Option<WritePreflightOutcome>, WorkerError> {
        if let Err(err) = self.ensure_process() {
            let reply = self.build_worker_error_reply_for_mode(&err, input);
            return self.finish_default_preflight_reply(reply).map(Some);
        }
        Ok(None)
    }

    fn settle_or_reply_to_pending_request_follow_up(
        &mut self,
        input: &WritePreflightInput<'_>,
    ) -> Result<Option<WritePreflightOutcome>, WorkerError> {
        if !self.pending_request {
            return Ok(None);
        }

        if !input.options.pending_state_prechecked {
            self.resolve_timeout_marker_with_wait(input.worker_timeout);
            if !self.pending_request {
                return Ok(None);
            }
        }

        self.pending_request_busy_follow_up(input, Duration::ZERO)
    }

    fn pending_request_busy_follow_up(
        &mut self,
        input: &WritePreflightInput<'_>,
        timeout: Duration,
    ) -> Result<Option<WritePreflightOutcome>, WorkerError> {
        if !self.pending_request {
            return Ok(None);
        }

        let mut reply = self.poll_pending_output_for_mode(input, timeout)?;
        let detached_prefix_item_count = match &reply.reply {
            WorkerReply::Output { contents, .. } => contents.len(),
        };
        self.last_detached_prefix_item_count = detached_prefix_item_count;
        mark_busy_follow_up_reply(&mut reply.reply);
        self.finish_caller_preflight_reply(reply, input).map(Some)
    }

    fn empty_input_has_pending_output(&self, mode: WriteStdinMode) -> bool {
        match mode {
            WriteStdinMode::Files => {
                self.pending_request
                    || self.pending_output_tape.has_pending()
                    || self.settled_pending_completion.is_some()
            }
            WriteStdinMode::Pager => {
                self.pending_request
                    || self.output.has_pending_output()
                    || self.settled_pending_completion.is_some()
            }
        }
    }

    fn poll_pending_output_for_mode(
        &mut self,
        input: &WritePreflightInput<'_>,
        timeout: Duration,
    ) -> Result<ReplyWithOffset, WorkerError> {
        match input.mode {
            WriteStdinMode::Files => self.poll_pending_output_files(timeout),
            WriteStdinMode::Pager => self.poll_pending_output_pager(timeout, input.page_bytes),
        }
    }

    fn build_worker_error_reply_for_mode(
        &mut self,
        err: &WorkerError,
        input: &WritePreflightInput<'_>,
    ) -> ReplyWithOffset {
        match input.mode {
            WriteStdinMode::Files => {
                let input_context = self.prepare_input_context_files();
                self.build_reply_from_worker_error_files(err, input_context)
            }
            WriteStdinMode::Pager => {
                let input_context = self.prepare_input_context_pager();
                self.build_reply_from_worker_error_pager(err, input_context, input.page_bytes)
            }
        }
    }

    fn reset_after_worker_error(&mut self, mode: WriteStdinMode) {
        match mode {
            WriteStdinMode::Files => {
                let _ = self.reset_preserving_detached_prefix_item_count();
            }
            WriteStdinMode::Pager => {
                let preserve_pager = self.pager.is_active();
                let _ = self.reset_with_pager_preserving_detached_prefix_item_count(preserve_pager);
            }
        }
    }

    fn build_idle_reply_for_mode(&mut self, mode: WriteStdinMode) -> ReplyWithOffset {
        match mode {
            WriteStdinMode::Files => self.build_idle_poll_reply_files(),
            WriteStdinMode::Pager => self.build_idle_poll_reply_pager(),
        }
    }

    fn finish_default_preflight_reply(
        &mut self,
        reply: ReplyWithOffset,
    ) -> Result<WritePreflightOutcome, WorkerError> {
        let reply = self.finalize_reply(reply);
        self.maybe_reset_after_session_end_with_options(None, false, false)?;
        Ok(WritePreflightOutcome::Reply(reply))
    }

    fn finish_caller_preflight_reply(
        &mut self,
        reply: ReplyWithOffset,
        input: &WritePreflightInput<'_>,
    ) -> Result<WritePreflightOutcome, WorkerError> {
        let reply = self.finalize_reply(reply);
        self.maybe_reset_after_session_end_with_options(
            input.options.deferred_sandbox_state_update.clone(),
            input.options.suppress_session_end_reset,
            input.options.pending_state_prechecked,
        )?;
        Ok(WritePreflightOutcome::Reply(reply))
    }

    fn finish_pager_local_reply(
        &mut self,
        reply: ReplyWithOffset,
    ) -> Result<WritePreflightOutcome, WorkerError> {
        let reply = self.finalize_reply(reply);
        self.maybe_reset_after_session_end_with_options(None, true, false)?;
        Ok(WritePreflightOutcome::Reply(reply))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Backend;
    use crate::output_capture::OUTPUT_RING_CAPACITY_BYTES;
    use crate::oversized_output::OversizedOutputMode;
    use crate::sandbox_cli::SandboxCliPlan;
    use crate::worker_process::test_support::{
        contents_text, pager_buffer_from_worker_text, sleeping_test_child,
        static_pager_buffer_from_worker_text, successful_test_child, test_worker_process,
    };
    use crate::worker_protocol::{ContentOrigin, WorkerReply};
    use crate::worker_supervisor::GuardrailEvent;
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;

    #[test]
    fn busy_guardrail_event_survives_sandbox_restart_notice() {
        let mut manager = WorkerManager::new(
            Backend::R,
            SandboxCliPlan::default(),
            OversizedOutputMode::Files,
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
    fn pager_empty_input_polls_pending_output_before_pager_commands() {
        let mut manager = WorkerManager::new(
            Backend::R,
            SandboxCliPlan::default(),
            OversizedOutputMode::Pager,
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
        let mut manager = WorkerManager::new(
            Backend::R,
            SandboxCliPlan::default(),
            OversizedOutputMode::Pager,
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
    fn pager_empty_input_preserves_idle_guardrail_notice() {
        let mut last_text = String::new();
        for _ in 0..16 {
            let mut manager = WorkerManager::new(
                Backend::R,
                SandboxCliPlan::default(),
                OversizedOutputMode::Pager,
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

            std::thread::sleep(Duration::from_millis(5));
        }

        assert!(
            last_text.contains("[repl] worker was idle; new session started"),
            "expected empty pager polls to preserve idle guardrail restart notices, got: {last_text:?}"
        );
    }
}
