use std::sync::atomic::Ordering;

use super::{WorkerManager, output_echo_source_for_backend};
use crate::completion_reply::{CompletionInfo, InputContext};
use crate::ipc::IpcEchoEvent;
use crate::output_capture::{OutputBuffer, reset_last_reply_marker_offset, reset_output_ring};
use crate::output_snapshot::take_range_from_ring_after_completion;
use crate::oversized_output::OversizedOutputMode;
use crate::pager::{self, Pager};
use crate::pending_output_tape::FormattedPendingOutput;
use crate::reply_presentation::{
    build_input_transcript, echo_transcript_from_events, fallback_prompt_variants,
    should_drop_echo_only_contents, should_trim_echo_prefix,
    trim_echo_then_append_protocol_warnings, trim_leading_input_echo_from_contents,
    trim_matching_echo_event_suffix_from_contents,
};
use crate::worker_protocol::{ContentOrigin, WorkerContent};

#[derive(Default)]
pub(super) struct PrefixCapture {
    pub(super) contents: Vec<WorkerContent>,
    pub(super) is_error: bool,
    pub(super) bytes: u64,
}

impl WorkerManager {
    /// Drains detached output that arrived before the next accepted request so it can be prefixed
    /// into that request's visible reply.
    pub(super) fn prepare_input_context_files(&mut self) -> InputContext {
        let reply_prefix = self.take_current_prefix_files();
        let (detached_prefix, reply_prefix) = self.take_prefixes_for_next_request(reply_prefix);
        InputContext {
            detached_prefix_contents: detached_prefix.contents,
            reply_prefix_contents: reply_prefix.contents,
            prefix_is_error: detached_prefix.is_error || reply_prefix.is_error,
            start_offset: 0,
            prefix_bytes: 0,
            input_echo: None,
            input_transcript: None,
        }
    }

    pub(super) fn prepare_input_context_pager(
        &mut self,
        text: &str,
        echo_input: bool,
    ) -> InputContext {
        self.output.start_capture();

        let had_pending_output = self.output.has_pending_output();
        let saw_background_output = self.output.pending_output_since_last_reply();
        let prompt_hint = self.current_prompt_hint();
        self.remember_prompt(prompt_hint.clone());

        let mut input_echo = echo_input
            .then(|| text.to_string())
            .and_then(|value| pager::build_input_echo(&value));
        let input_transcript = build_input_transcript(prompt_hint.as_deref(), text);
        let reply_prefix = self.take_current_prefix_pager(had_pending_output);
        let (detached_prefix, reply_prefix) = self.take_prefixes_for_next_request(reply_prefix);

        let start_offset = self.output.end_offset().unwrap_or(0);
        if input_echo.is_none() && (echo_input || saw_background_output || had_pending_output) {
            input_echo = pager::build_input_echo(text);
        }

        InputContext {
            detached_prefix_contents: detached_prefix.contents,
            reply_prefix_contents: reply_prefix.contents,
            prefix_is_error: detached_prefix.is_error || reply_prefix.is_error,
            start_offset,
            prefix_bytes: detached_prefix.bytes.saturating_add(reply_prefix.bytes),
            input_echo,
            input_transcript,
        }
    }

    pub(super) fn has_detached_output_to_preserve(&self) -> bool {
        match self.oversized_output {
            OversizedOutputMode::Files => {
                self.pending_output_tape.has_pending() || self.settled_pending_completion.is_some()
            }
            OversizedOutputMode::Pager => {
                self.output.has_pending_output() || self.settled_pending_completion.is_some()
            }
        }
    }

    pub(super) fn reset_output_state_files(&mut self, clear_pending_output: bool) {
        self.reset_output_state_files_inner(clear_pending_output, false);
    }

    pub(super) fn reset_output_state_files_preserving_detached_output(&mut self) {
        self.seed_aborted_files_completion_for_respawn();
        let prefix = self.take_current_prefix_files();
        self.stage_prefix_before_respawn(prefix);
        self.reset_output_state_files_inner(true, false);
    }

    pub(super) fn reset_output_state_pager(
        &mut self,
        clear_pending_output: bool,
        preserve_pager: bool,
    ) {
        self.reset_output_state_pager_inner(clear_pending_output, preserve_pager, false);
    }

    pub(super) fn reset_output_state_pager_preserving_detached_output(
        &mut self,
        preserve_pager: bool,
    ) {
        self.seed_aborted_pager_completion_for_respawn();
        let had_pending_output = self.output.has_pending_output();
        let prefix = self.take_current_prefix_pager(had_pending_output);
        self.stage_prefix_before_respawn(prefix);
        self.reset_output_state_pager_inner(true, preserve_pager, false);
    }

    pub(super) fn clear_preserved_prefixes(&mut self) {
        self.preserved_detached_prefix = PrefixCapture::default();
        self.reply_owned_prefix = PrefixCapture::default();
        self.next_live_prefix_belongs_to_reply = false;
    }

    fn take_current_prefix_files(&mut self) -> PrefixCapture {
        let settled_completion = self.settled_pending_completion.take();
        let fallback_input = settled_completion
            .as_ref()
            .map(|completion| self.take_input_fallback(completion))
            .unwrap_or_default();
        let fallback_input_transcript = fallback_input.transcript.clone();
        // A new accepted request seals the detached prefix. Flush any incomplete UTF-8 tail now
        // so it stays with the detached transcript instead of merging into fresh request output.
        let FormattedPendingOutput {
            mut contents,
            saw_stderr,
        } = self.drain_sealed_formatted_output();
        if let Some(completion) = settled_completion.as_ref() {
            let has_fallback_input_transcript = fallback_input_transcript.is_some();
            let trim_enabled = if completion.echo_events.is_empty() {
                has_fallback_input_transcript
            } else {
                should_trim_echo_prefix(&completion.echo_events)
            };
            let echo_transcript = echo_transcript_from_events(&completion.echo_events)
                .or(fallback_input_transcript.clone());
            trim_echo_then_append_protocol_warnings(
                &mut contents,
                echo_transcript.as_deref(),
                trim_enabled,
                if completion.echo_events.is_empty() {
                    has_fallback_input_transcript
                } else {
                    should_drop_echo_only_contents(&completion.echo_events)
                },
                &completion.protocol_warnings,
            );
            if !trim_enabled {
                let _ = trim_matching_echo_event_suffix_from_contents(
                    &mut contents,
                    &completion.echo_events,
                );
            }
            if completion.echo_events.is_empty() && fallback_input_transcript.is_none() {
                let prompt_variants = fallback_prompt_variants(
                    completion.prompt.as_deref(),
                    completion.prompt_variants.as_deref(),
                );
                let _ = trim_leading_input_echo_from_contents(
                    &mut contents,
                    fallback_input.raw_input.as_deref(),
                    &prompt_variants,
                );
            }
        }
        PrefixCapture {
            contents,
            is_error: saw_stderr,
            bytes: 0,
        }
    }

    fn take_current_prefix_pager(&mut self, had_pending_output: bool) -> PrefixCapture {
        let settled_completion = self.settled_pending_completion.take();

        let mut prefix_contents = Vec::new();
        let mut prefix_bytes: u64 = 0;
        let mut prefix_is_error = false;

        if had_pending_output || settled_completion.is_some() {
            let pending_end = self.output.end_offset().unwrap_or(0);
            let pending_start = self.output.current_offset().unwrap_or(pending_end);
            let pending_bytes = pending_end.saturating_sub(pending_start);

            if let Some(completion) = settled_completion {
                let FormattedPendingOutput {
                    contents,
                    saw_stderr,
                } = take_range_from_ring_after_completion(
                    &self.output,
                    pending_start,
                    pending_end,
                    &completion.echo_events,
                    completion.prompt_variants.as_deref(),
                    &completion.protocol_warnings,
                );
                prefix_is_error = saw_stderr;
                prefix_contents = contents;
            } else {
                prefix_is_error = self
                    .output
                    .saw_stderr_in_range(pending_start.min(pending_end), pending_end);
                prefix_contents = pager::take_range_from_ring(&self.output, pending_end);
            }
            prefix_bytes = pending_bytes;
        }

        PrefixCapture {
            contents: prefix_contents,
            is_error: prefix_is_error,
            bytes: prefix_bytes,
        }
    }

    fn seed_aborted_files_completion_for_respawn(&mut self) {
        if !self.pending_request
            || self.settled_pending_completion.is_some()
            || self.pending_request_input.is_none()
        {
            return;
        }

        let prompt = self.last_prompt.clone();
        self.settled_pending_completion = Some(CompletionInfo {
            prompt: prompt.clone(),
            stdin_wait_prompt: None,
            prompt_variants: prompt.clone().map(|prompt| vec![prompt]),
            echo_events: Vec::new(),
            protocol_warnings: Vec::new(),
            session_end_seen: false,
        });
    }

    fn seed_aborted_pager_completion_for_respawn(&mut self) {
        if !self.pending_request
            || self.settled_pending_completion.is_some()
            || self.pending_request_input.is_none()
        {
            return;
        }

        let prompt = self.last_prompt.clone();
        let prompt_variants = prompt.clone().map(|prompt| vec![prompt]);
        let echo_events = match (prompt, self.pending_request_input.clone()) {
            (Some(prompt), Some(line)) => vec![IpcEchoEvent {
                prompt,
                line,
                source: output_echo_source_for_backend(self.backend),
            }],
            _ => Vec::new(),
        };
        self.settled_pending_completion = Some(CompletionInfo {
            prompt: self.last_prompt.clone(),
            stdin_wait_prompt: None,
            prompt_variants,
            echo_events,
            protocol_warnings: Vec::new(),
            session_end_seen: false,
        });
    }

    fn reset_output_state_files_inner(
        &mut self,
        clear_pending_output: bool,
        preserve_detached_output: bool,
    ) {
        if clear_pending_output {
            self.pending_output_tape.clear();
        }
        self.pending_request = false;
        self.pending_request_started_at = None;
        if !preserve_detached_output {
            self.pending_request_input = None;
        }
        self.driver.clear_active_turn();
        self.session_end_seen = false;
        if !preserve_detached_output {
            self.settled_pending_completion = None;
            self.settled_pending_error = None;
            self.last_detached_prefix_item_count = 0;
        }
        self.last_prompt = None;
        self.stdin_waiting = false;
        self.guardrail.busy.store(false, Ordering::Relaxed);
    }

    fn reset_output_state_pager_inner(
        &mut self,
        clear_pending_output: bool,
        preserve_pager: bool,
        preserve_detached_output: bool,
    ) {
        if clear_pending_output {
            self.pending_output_tape.clear();
        }
        if !preserve_detached_output {
            reset_output_ring();
            reset_last_reply_marker_offset();
            self.output = OutputBuffer::default();
        }
        if !preserve_pager {
            self.pager = Pager::default();
        }
        self.pending_request = false;
        self.pending_request_started_at = None;
        self.pending_request_input = None;
        self.driver.clear_active_turn();
        self.session_end_seen = false;
        if !preserve_detached_output {
            self.settled_pending_completion = None;
            self.settled_pending_error = None;
            self.last_detached_prefix_item_count = 0;
        }
        self.pager_prompt = None;
        self.last_prompt = None;
        self.stdin_waiting = false;
        self.guardrail.busy.store(false, Ordering::Relaxed);
    }

    fn append_prefix_capture(target: &mut PrefixCapture, mut prefix: PrefixCapture) {
        if prefix.contents.is_empty() {
            prefix.bytes = 0;
        }
        if prefix.contents.is_empty() && !prefix.is_error {
            return;
        }
        target.is_error |= prefix.is_error;
        target.bytes = target
            .bytes
            .saturating_add(prefix_worker_text_bytes(&prefix.contents));
        target.contents.append(&mut prefix.contents);
    }

    fn take_prefixes_for_next_request(
        &mut self,
        current_prefix: PrefixCapture,
    ) -> (PrefixCapture, PrefixCapture) {
        let mut detached_prefix = std::mem::take(&mut self.preserved_detached_prefix);
        let mut reply_prefix = std::mem::take(&mut self.reply_owned_prefix);
        if self.next_live_prefix_belongs_to_reply {
            Self::append_prefix_capture(&mut reply_prefix, current_prefix);
        } else {
            Self::append_prefix_capture(&mut detached_prefix, current_prefix);
        }
        self.next_live_prefix_belongs_to_reply = false;
        (detached_prefix, reply_prefix)
    }

    fn stage_prefix_before_respawn(&mut self, prefix: PrefixCapture) {
        if self.next_live_prefix_belongs_to_reply {
            Self::append_prefix_capture(&mut self.reply_owned_prefix, prefix);
            self.next_live_prefix_belongs_to_reply = false;
        } else {
            Self::append_prefix_capture(&mut self.preserved_detached_prefix, prefix);
        }
    }
}

fn prefix_worker_text_bytes(contents: &[WorkerContent]) -> u64 {
    contents
        .iter()
        .map(|content| match content {
            WorkerContent::ContentText {
                text,
                origin: ContentOrigin::Worker,
                ..
            } => text.len() as u64,
            WorkerContent::ContentText { .. } | WorkerContent::ContentImage { .. } => 0,
        })
        .sum()
}
