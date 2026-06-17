use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::completion_reply::ReplyWithOffset;
use crate::worker_protocol::WorkerReply;

use super::{
    WorkerError, WorkerManager, WriteStdinMode, WriteStdinOptions, mark_busy_follow_up_reply,
    prechecked_follow_up_requires_meta_error,
};

pub(super) struct WritePreflightInput<'a> {
    pub(super) mode: WriteStdinMode,
    pub(super) text: &'a str,
    pub(super) worker_timeout: Duration,
    pub(super) page_bytes: u64,
    pub(super) echo_input: bool,
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

        self.resolve_pending_request_before_busy_reply(&input);
        if let Some(outcome) = self.pending_request_busy_follow_up(&input)? {
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
        self.resolve_pending_request_before_busy_reply(&input);
        if let Some(outcome) = self.pending_request_busy_follow_up(&input)? {
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

        let reply = self.poll_pending_output_for_mode(input)?;
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

    fn resolve_pending_request_before_busy_reply(&mut self, input: &WritePreflightInput<'_>) {
        if !input.options.pending_state_prechecked && self.pending_request {
            self.resolve_timeout_marker_with_wait(Duration::from_millis(25));
        }
    }

    fn pending_request_busy_follow_up(
        &mut self,
        input: &WritePreflightInput<'_>,
    ) -> Result<Option<WritePreflightOutcome>, WorkerError> {
        if !self.pending_request {
            return Ok(None);
        }

        let mut reply = self.poll_pending_output_for_mode(input)?;
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
    ) -> Result<ReplyWithOffset, WorkerError> {
        match input.mode {
            WriteStdinMode::Files => self.poll_pending_output_files(input.worker_timeout),
            WriteStdinMode::Pager => {
                self.poll_pending_output_pager(input.worker_timeout, input.page_bytes)
            }
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
                let input_context = self.prepare_input_context_pager(input.text, input.echo_input);
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
