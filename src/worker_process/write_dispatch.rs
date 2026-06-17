use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::completion_reply::{InputContext, ReplyWithOffset};
use crate::sandbox::SandboxStateUpdate;
use crate::worker_protocol::WorkerReply;

use super::{WorkerError, WorkerManager, WriteStdinMode};

pub(super) struct WriteDispatchInput {
    pub(super) mode: WriteStdinMode,
    pub(super) text: String,
    pub(super) worker_timeout: Duration,
    pub(super) server_timeout: Duration,
    pub(super) deferred_sandbox_state_update: Option<SandboxStateUpdate>,
    pub(super) page_bytes: u64,
    pub(super) echo_input: bool,
    pub(super) process_prechecked: bool,
}

impl WorkerManager {
    pub(super) fn dispatch_write_request(
        &mut self,
        input: WriteDispatchInput,
    ) -> Result<WorkerReply, WorkerError> {
        self.apply_deferred_sandbox_state_update(input.deferred_sandbox_state_update.clone())?;
        if !input.process_prechecked
            && let Err(err) = self.ensure_process()
        {
            let reply = self.build_write_dispatch_worker_error_reply(&err, &input);
            return self.finish_write_dispatch_reply(reply);
        }

        let mode = input.mode;
        let page_bytes = input.page_bytes;
        let input_context = self.prepare_write_dispatch_input_context(&input);
        let request = match self.send_worker_request(
            input.text,
            input.worker_timeout,
            input.server_timeout,
        ) {
            Ok(request) => request,
            Err(err) => {
                self.guardrail.busy.store(false, Ordering::Relaxed);
                let reply = self.build_write_dispatch_worker_error_reply_from_context(
                    &err,
                    input_context,
                    mode,
                    page_bytes,
                );
                self.reset_after_write_dispatch_send_error(mode);
                return Ok(self.finalize_reply(reply));
            }
        };

        let reply =
            self.build_write_dispatch_request_reply(request, input_context, mode, page_bytes)?;
        let reply = self.finalize_reply(reply);
        self.maybe_reset_after_session_end_with_options(None, false, false)?;
        Ok(reply)
    }

    fn prepare_write_dispatch_input_context(&mut self, input: &WriteDispatchInput) -> InputContext {
        match input.mode {
            WriteStdinMode::Files => self.prepare_input_context_files(),
            WriteStdinMode::Pager => {
                self.prepare_input_context_pager(&input.text, input.echo_input)
            }
        }
    }

    fn build_write_dispatch_worker_error_reply(
        &mut self,
        err: &WorkerError,
        input: &WriteDispatchInput,
    ) -> ReplyWithOffset {
        let input_context = self.prepare_write_dispatch_input_context(input);
        self.build_write_dispatch_worker_error_reply_from_context(
            err,
            input_context,
            input.mode,
            input.page_bytes,
        )
    }

    fn build_write_dispatch_worker_error_reply_from_context(
        &mut self,
        err: &WorkerError,
        input_context: InputContext,
        mode: WriteStdinMode,
        page_bytes: u64,
    ) -> ReplyWithOffset {
        match mode {
            WriteStdinMode::Files => self.build_reply_from_worker_error_files(err, input_context),
            WriteStdinMode::Pager => {
                self.build_reply_from_worker_error_pager(err, input_context, page_bytes)
            }
        }
    }

    fn reset_after_write_dispatch_send_error(&mut self, mode: WriteStdinMode) {
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

    fn build_write_dispatch_request_reply(
        &mut self,
        request: super::RequestState,
        input_context: InputContext,
        mode: WriteStdinMode,
        page_bytes: u64,
    ) -> Result<ReplyWithOffset, WorkerError> {
        match mode {
            WriteStdinMode::Files => self.build_reply_from_request_files(request, input_context),
            WriteStdinMode::Pager => {
                self.build_reply_from_request_pager(request, input_context, page_bytes)
            }
        }
    }

    fn finish_write_dispatch_reply(
        &mut self,
        reply: ReplyWithOffset,
    ) -> Result<WorkerReply, WorkerError> {
        let reply = self.finalize_reply(reply);
        self.maybe_reset_after_session_end_with_options(None, false, false)?;
        Ok(reply)
    }
}
