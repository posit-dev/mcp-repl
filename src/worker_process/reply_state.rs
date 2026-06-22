use std::sync::atomic::Ordering;

use crate::completion_reply::{ReplyWithOffset, idle_status_content};
use crate::oversized_output::OversizedOutputMode;
use crate::pending_output_tape::FormattedPendingOutput;
use crate::reply_presentation::{append_prompt_if_missing, normalize_prompt};
use crate::worker_protocol::{ContentOrigin, WorkerContent, WorkerErrorCode, WorkerReply};
use crate::worker_supervisor::GuardrailEvent;

use super::WorkerManager;

impl WorkerManager {
    pub(super) fn handle_pager_command(&mut self, text: &str) -> Option<ReplyWithOffset> {
        if !self.pager.is_active() {
            return None;
        }
        self.pager.refresh_from_output(&self.output);
        let mut reply = self.pager.handle_command(text);
        let pager_active = self.pager.is_active();
        let WorkerReply::Output {
            contents, prompt, ..
        } = &mut reply;
        let resolved_prompt = if pager_active {
            None
        } else {
            match self.pager_prompt.take() {
                Some(prompt) => prompt.into_prompt(),
                None => {
                    contents.push(WorkerContent::server_stderr(
                        "[repl] protocol error: missing prompt after pager dismiss",
                    ));
                    None
                }
            }
        };
        if pager_active {
            *prompt = None;
        } else {
            self.remember_prompt(resolved_prompt.clone());
            append_prompt_if_missing(contents, resolved_prompt.clone());
            *prompt = resolved_prompt;
        }
        let end_offset = self.output.end_offset().unwrap_or(0);
        Some(ReplyWithOffset { reply, end_offset })
    }

    pub(super) fn guardrail_event_pending(&self) -> bool {
        self.guardrail
            .event
            .lock()
            .expect("guardrail event mutex poisoned")
            .is_some()
    }

    pub(super) fn guardrail_busy_event_pending(&self) -> bool {
        self.guardrail
            .event
            .lock()
            .expect("guardrail event mutex poisoned")
            .as_ref()
            .is_some_and(|event| event.was_busy)
    }

    pub(super) fn clear_guardrail_busy_event(&mut self) {
        let mut slot = self
            .guardrail
            .event
            .lock()
            .expect("guardrail event mutex poisoned");
        if slot.as_ref().is_some_and(|event| event.was_busy) {
            *slot = None;
            self.guardrail.busy.store(false, Ordering::Relaxed);
        }
    }

    pub(super) fn maybe_emit_guardrail_notice(&mut self) {
        self.maybe_emit_pending_server_notice();
        let event = {
            let mut slot = self
                .guardrail
                .event
                .lock()
                .expect("guardrail event mutex poisoned");
            if slot.as_ref().is_some_and(|event| event.was_busy) {
                return;
            }
            slot.take()
        };
        let Some(event) = event else {
            return;
        };
        self.append_server_notice(event);
    }

    pub(super) fn maybe_emit_pending_server_notice(&mut self) {
        let Some(event) = self.pending_server_notice.take() else {
            return;
        };
        self.append_server_notice(event);
    }

    fn append_server_notice(&mut self, event: GuardrailEvent) {
        match self.oversized_output {
            OversizedOutputMode::Files => {
                if event.is_error {
                    self.pending_output_tape
                        .append_server_stderr_bytes(event.message.as_bytes());
                } else {
                    self.pending_output_tape
                        .append_stdout_status_line(event.message.as_bytes());
                }
            }
            OversizedOutputMode::Pager => {
                self.output_timeline.append_text(
                    event.message.as_bytes(),
                    event.is_error,
                    ContentOrigin::Server,
                );
            }
        }
    }

    pub(super) fn finalize_reply(&self, reply: ReplyWithOffset) -> WorkerReply {
        let _ = reply.end_offset;
        reply.reply
    }

    pub(super) fn remember_prompt(&mut self, prompt: Option<String>) {
        self.last_prompt = normalize_prompt(prompt);
    }

    pub(super) fn current_prompt_hint(&self) -> Option<String> {
        let prompt = self
            .process
            .as_ref()
            .and_then(|process| process.ipc_connection())
            .and_then(|ipc| ipc.try_take_prompt())
            .and_then(|prompt| normalize_prompt(Some(prompt)));
        prompt.or_else(|| self.last_prompt.clone())
    }

    pub(super) fn drain_formatted_output(&self) -> FormattedPendingOutput {
        self.pending_output_tape.drain_snapshot().format_contents()
    }

    pub(super) fn drain_final_formatted_output(&self) -> FormattedPendingOutput {
        self.pending_output_tape
            .drain_final_snapshot()
            .format_contents_for_reply()
    }

    pub(super) fn drain_sealed_formatted_output(&self) -> FormattedPendingOutput {
        self.pending_output_tape
            .drain_sealed_snapshot()
            .format_contents()
    }

    pub(super) fn build_idle_poll_reply_files(&mut self) -> ReplyWithOffset {
        let prompt = self.current_prompt_hint();
        self.remember_prompt(prompt.clone());
        let mut contents = vec![idle_status_content()];
        append_prompt_if_missing(&mut contents, prompt.clone());
        ReplyWithOffset {
            reply: WorkerReply::Output {
                contents,
                is_error: false,
                error_code: None,
                prompt,
                prompt_variants: None,
            },
            end_offset: 0,
        }
    }

    pub(super) fn build_idle_poll_reply_pager(&mut self) -> ReplyWithOffset {
        let prompt = self.current_prompt_hint();
        self.remember_prompt(prompt.clone());
        let mut contents = vec![idle_status_content()];
        append_prompt_if_missing(&mut contents, prompt.clone());
        ReplyWithOffset {
            reply: WorkerReply::Output {
                contents,
                is_error: false,
                error_code: None,
                prompt,
                prompt_variants: None,
            },
            end_offset: self.output.end_offset().unwrap_or(0),
        }
    }
}

pub(super) fn mark_busy_follow_up_reply(reply: &mut WorkerReply) {
    let WorkerReply::Output {
        contents,
        is_error,
        error_code,
        ..
    } = reply;
    contents.push(WorkerContent::server_stderr(
        "[repl] input discarded while worker busy",
    ));
    *is_error = true;
    if error_code.is_none() {
        *error_code = Some(WorkerErrorCode::Busy);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contents_text(contents: Vec<WorkerContent>) -> String {
        contents
            .into_iter()
            .filter_map(|content| match content {
                WorkerContent::ContentText { text, .. } => Some(text),
                WorkerContent::ContentImage { .. } => None,
            })
            .collect()
    }

    #[test]
    fn busy_follow_up_reply_sets_busy_error_code_when_missing() {
        let mut reply = WorkerReply::Output {
            contents: vec![WorkerContent::worker_stdout("tail\n")],
            is_error: false,
            error_code: None,
            prompt: None,
            prompt_variants: None,
        };

        mark_busy_follow_up_reply(&mut reply);

        let WorkerReply::Output {
            contents,
            is_error,
            error_code,
            ..
        } = reply;
        let text = contents_text(contents);

        assert!(
            is_error,
            "expected busy follow-up replies to be marked as errors"
        );
        assert_eq!(error_code, Some(WorkerErrorCode::Busy));
        assert!(
            text.contains("[repl] input discarded while worker busy"),
            "expected busy follow-up marker, got: {text:?}"
        );
    }

    #[test]
    fn busy_follow_up_reply_preserves_timeout_error_code() {
        let mut reply = WorkerReply::Output {
            contents: vec![WorkerContent::server_stdout("<<repl status: busy>>\n")],
            is_error: false,
            error_code: Some(WorkerErrorCode::Timeout),
            prompt: None,
            prompt_variants: None,
        };

        mark_busy_follow_up_reply(&mut reply);

        let WorkerReply::Output {
            contents,
            is_error,
            error_code,
            ..
        } = reply;
        let text = contents_text(contents);

        assert!(
            is_error,
            "expected timed-out busy follow-up replies to be marked as errors"
        );
        assert_eq!(
            error_code,
            Some(WorkerErrorCode::Timeout),
            "expected timed-out busy follow-up replies to preserve Timeout"
        );
        assert!(
            text.contains("[repl] input discarded while worker busy"),
            "expected busy follow-up marker, got: {text:?}"
        );
    }
}
