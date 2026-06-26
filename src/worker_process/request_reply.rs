use std::sync::atomic::Ordering;

use crate::completion_reply::{
    CompletionReplyMode, InputContext, ReplyWithOffset, build_completed_reply, build_timeout_reply,
    timeout_status_content,
};
use crate::output_snapshot::{
    SnapshotWithImages, snapshot_after_completion, snapshot_page_with_images,
};
use crate::pager;
use crate::worker_protocol::{WorkerContent, WorkerErrorCode, WorkerReply};

use super::request_lifecycle::RequestState;
use super::{WorkerError, WorkerManager};

impl WorkerManager {
    pub(super) fn build_reply_from_worker_error_files(
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
        }
    }

    pub(super) fn build_reply_from_worker_error_pager(
        &mut self,
        err: &WorkerError,
        context: InputContext,
        page_bytes: u64,
    ) -> ReplyWithOffset {
        self.last_detached_prefix_item_count = context.detached_prefix_contents.len();
        self.output_timeline.flush_utf8_tails();
        let end_offset = self.output.end_offset().unwrap_or(context.start_offset);
        let first_page_budget = page_bytes.saturating_sub(context.prefix_bytes);
        let mut contents = context.detached_prefix_contents;
        contents.extend(context.reply_prefix_contents);
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
        }
    }

    pub(super) fn build_reply_from_request_files(
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
                let formatted = self.drain_completed_formatted_output(session_end);
                let is_error = context.prefix_is_error || formatted.saw_stderr;
                contents.extend(formatted.contents);
                let built = build_completed_reply(
                    contents,
                    is_error,
                    &completion,
                    session_end,
                    CompletionReplyMode::Files {
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

                Ok(build_timeout_reply(contents, is_error))
            }
            Err(err) => {
                let reply = self.build_reply_from_worker_error_files(&err, context);
                let _ = self.reset_preserving_detached_prefix_item_count();
                Ok(reply)
            }
        }
    }

    pub(super) fn build_reply_from_request_pager(
        &mut self,
        request: RequestState,
        context: InputContext,
        page_bytes: u64,
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
                self.output_timeline.flush_utf8_tails();
                let end_offset = self.output.end_offset().unwrap_or(context.start_offset);
                let first_page_budget = page_bytes.saturating_sub(context.prefix_bytes);
                let mut contents = context.detached_prefix_contents;
                contents.extend(context.reply_prefix_contents);
                let completion_snapshot = snapshot_after_completion(
                    &self.output,
                    context.start_offset,
                    end_offset,
                    first_page_budget,
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
                    &completion,
                    session_end,
                    CompletionReplyMode::Pager {
                        pager_active: self.pager.is_active(),
                    },
                    self.backend,
                );
                self.remember_prompt(built.prompt_to_remember.clone());
                if let Some(pager_prompt) = built.pager_prompt {
                    self.pager_prompt = Some(pager_prompt);
                }
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

                self.pending_request = true;
                self.pending_request_started_at = Some(request.started_at);
                self.output_timeline.flush_ready_utf8_tails();
                let end_offset = self.output.end_offset().unwrap_or(0);
                let first_page_budget = page_bytes.saturating_sub(context.prefix_bytes);
                let mut contents = context.detached_prefix_contents;
                contents.extend(context.reply_prefix_contents);
                let SnapshotWithImages {
                    contents: mut page_contents,
                    pages_left,
                    buffer,
                    last_range,
                } = snapshot_page_with_images(&self.output, end_offset, first_page_budget);
                contents.append(&mut page_contents);

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

                Ok(build_timeout_reply(contents, is_error))
            }
            Err(err) => {
                let reply = self.build_reply_from_worker_error_pager(&err, context, page_bytes);
                let preserve_pager = self.pager.is_active();
                let _ = self.reset_with_pager_preserving_detached_prefix_item_count(preserve_pager);
                Ok(reply)
            }
        }
    }
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
