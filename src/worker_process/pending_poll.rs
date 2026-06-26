use std::time::{Duration, Instant};

use super::{WorkerError, WorkerManager};
use crate::completion_reply::{
    CompletionInfo, CompletionReplyMode, ReplyWithOffset, build_completed_reply,
    build_timeout_reply, timeout_status_content,
};
use crate::output_snapshot::{
    SnapshotWithImages, snapshot_after_completion, snapshot_page_with_images,
    snapshot_pending_timeout_page_with_images,
};
use crate::pager;
use crate::pending_output_tape::FormattedPendingOutput;

enum PendingPollState {
    TimedOut {
        elapsed: Duration,
    },
    Completed {
        completion: CompletionInfo,
        session_end: bool,
        consumed_completion: bool,
    },
    NoCompletion,
}

impl WorkerManager {
    /// Serves empty-input polls and busy follow-up replies for a timed-out request.
    /// Each poll only returns newly available output, but the server may keep appending it to one transcript file.
    pub(super) fn poll_pending_output_files(
        &mut self,
        timeout: Duration,
    ) -> Result<ReplyWithOffset, WorkerError> {
        let poll_start = Instant::now();

        if let Some(err) = self.settled_pending_error.take() {
            let _ = self.reset_preserving_detached_prefix_item_count();
            return Err(err);
        }

        let state = self.observe_pending_poll_state(timeout, poll_start)?;
        let (completion, session_end, consumed_completion, timed_out_elapsed) = match state {
            PendingPollState::TimedOut { elapsed } => {
                (CompletionInfo::empty(), false, false, Some(elapsed))
            }
            PendingPollState::Completed {
                completion,
                session_end,
                consumed_completion,
            } => (completion, session_end, consumed_completion, None),
            PendingPollState::NoCompletion => (CompletionInfo::empty(), false, false, None),
        };

        if timed_out_elapsed.is_none() && consumed_completion {
            self.wait_for_late_files_output_after_settled_completion(timeout);
        }

        let FormattedPendingOutput {
            mut contents,
            saw_stderr,
        } = if timed_out_elapsed.is_some() {
            self.drain_formatted_output()
        } else {
            self.drain_completed_formatted_output(session_end)
        };
        let is_error = saw_stderr;

        if let Some(elapsed) = timed_out_elapsed {
            contents.push(timeout_status_content(elapsed));
            return Ok(build_timeout_reply(contents, is_error));
        }

        let built = build_completed_reply(
            contents,
            is_error,
            &completion,
            session_end,
            CompletionReplyMode::Files {
                idle_status_if_empty: true,
            },
            self.backend,
        );
        self.remember_prompt(built.prompt_to_remember.clone());
        Ok(built.reply)
    }

    pub(super) fn poll_pending_output_pager(
        &mut self,
        timeout: Duration,
        page_bytes: u64,
    ) -> Result<ReplyWithOffset, WorkerError> {
        let poll_start = Instant::now();
        let start_offset = self.output.current_offset().unwrap_or(0);
        let mut end_offset = self.output.end_offset().unwrap_or(start_offset);

        if let Some(err) = self.settled_pending_error.take() {
            let preserve_pager = self.pager.is_active();
            let _ = self.reset_with_pager_preserving_detached_prefix_item_count(preserve_pager);
            return Err(err);
        }

        let state = self.observe_pending_poll_state(timeout, poll_start)?;
        let observed_completion = !matches!(state, PendingPollState::NoCompletion);
        let (completion, session_end, timed_out_elapsed) = match state {
            PendingPollState::TimedOut { elapsed } => {
                (CompletionInfo::empty(), false, Some(elapsed))
            }
            PendingPollState::Completed {
                completion,
                session_end,
                ..
            } => (completion, session_end, None),
            PendingPollState::NoCompletion => (CompletionInfo::empty(), false, None),
        };
        if timed_out_elapsed.is_some() {
            self.output_timeline
                .seal_utf8_tails_blocking_visible_output();
        } else if observed_completion {
            self.output_timeline.seal_utf8_tails();
        }
        if observed_completion {
            end_offset = self.output.end_offset().unwrap_or(end_offset);
        }
        if end_offset < start_offset {
            end_offset = start_offset;
        }

        let (saw_stderr, snapshot) = if observed_completion && timed_out_elapsed.is_none() {
            let completed =
                snapshot_after_completion(&self.output, start_offset, end_offset, page_bytes);
            (completed.saw_stderr, completed.snapshot)
        } else if timed_out_elapsed.is_some() {
            let saw_stderr = self
                .output
                .saw_stderr_in_range(start_offset.min(end_offset), end_offset);
            let snapshot =
                snapshot_pending_timeout_page_with_images(&self.output, end_offset, page_bytes);
            (saw_stderr, snapshot)
        } else {
            let saw_stderr = self
                .output
                .saw_stderr_in_range(start_offset.min(end_offset), end_offset);
            let snapshot = snapshot_page_with_images(&self.output, end_offset, page_bytes);
            (saw_stderr, snapshot)
        };
        let is_error = saw_stderr;
        let page_is_error = saw_stderr;
        let SnapshotWithImages {
            mut contents,
            pages_left,
            buffer,
            last_range,
        } = snapshot;

        if let Some(elapsed) = timed_out_elapsed {
            contents.push(timeout_status_content(elapsed));
        }
        pager::maybe_activate_and_append_footer(
            &mut self.pager,
            &mut contents,
            pages_left,
            page_is_error,
            buffer,
            last_range,
        );

        if timed_out_elapsed.is_some() {
            return Ok(build_timeout_reply(contents, is_error));
        }

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
        Ok(built.reply)
    }

    fn observe_pending_poll_state(
        &mut self,
        timeout: Duration,
        poll_start: Instant,
    ) -> Result<PendingPollState, WorkerError> {
        if self.pending_request {
            return self.observe_active_pending_request(timeout, poll_start);
        }

        if let Some(info) = self.settled_pending_completion.take() {
            let session_end = info.session_end_seen;
            if session_end {
                self.note_session_end(false);
            }
            return Ok(PendingPollState::Completed {
                completion: info,
                session_end,
                consumed_completion: true,
            });
        }

        Ok(PendingPollState::NoCompletion)
    }

    fn observe_active_pending_request(
        &mut self,
        timeout: Duration,
        poll_start: Instant,
    ) -> Result<PendingPollState, WorkerError> {
        match self.wait_for_request_completion(timeout) {
            Ok(info) => {
                let session_end = info.session_end_seen;
                self.clear_pending_request_state();
                if session_end {
                    self.note_session_end(true);
                }
                Ok(PendingPollState::Completed {
                    completion: info,
                    session_end,
                    consumed_completion: true,
                })
            }
            Err(WorkerError::Timeout(_)) => {
                if self.pending_worker_exited()? {
                    self.note_session_end(true);
                    self.clear_pending_request_state();
                    let mut completion = CompletionInfo::empty();
                    completion.session_end_seen = true;
                    return Ok(PendingPollState::Completed {
                        completion,
                        session_end: true,
                        consumed_completion: true,
                    });
                }

                let elapsed = self
                    .pending_request_started_at
                    .map(|start| start.elapsed())
                    .unwrap_or_else(|| poll_start.elapsed());
                Ok(PendingPollState::TimedOut { elapsed })
            }
            Err(err) => Err(err),
        }
    }

    fn pending_worker_exited(&mut self) -> Result<bool, WorkerError> {
        match self.process.as_mut() {
            Some(process) => process.is_running().map(|running| !running),
            None => Ok(true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Backend;
    use crate::oversized_output::OversizedOutputMode;
    use crate::sandbox_cli::SandboxCliPlan;
    use crate::worker_process::WriteStdinOptions;
    use crate::worker_process::test_support::{
        contents_text, sleeping_test_child, test_worker_process,
    };
    use crate::worker_protocol::WorkerReply;

    #[test]
    fn files_empty_poll_waits_for_late_stdout_after_settled_completion() {
        let mut manager = WorkerManager::new(
            Backend::R,
            SandboxCliPlan::default(),
            OversizedOutputMode::Files,
        )
        .expect("worker manager");
        manager.process = Some(test_worker_process(sleeping_test_child()));
        manager.settled_pending_completion = Some(CompletionInfo {
            prompt: Some("> ".to_string()),
            prompt_variants: Some(vec!["> ".to_string()]),
            protocol_warnings: Vec::new(),
            session_end_seen: false,
        });

        let tape = manager.pending_output_tape.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            tape.append_stdout_bytes(b"[1] 2\n");
        });

        let reply = manager
            .write_stdin_files(
                String::new(),
                Duration::from_millis(500),
                Duration::from_millis(500),
                WriteStdinOptions::default(),
            )
            .expect("empty poll reply");

        writer.join().expect("late stdout writer");
        if let Some(process) = manager.process.take() {
            let _ = process.kill();
        }

        let WorkerReply::Output { contents, .. } = reply;
        let text = contents_text(&contents);
        assert!(
            text.contains("[1] 2\n"),
            "expected the empty poll to wait for late settled stdout, got: {text:?}"
        );
        assert!(
            !text.contains("<<repl status: idle>>"),
            "did not expect an idle marker before late settled stdout, got: {text:?}"
        );
    }
}
