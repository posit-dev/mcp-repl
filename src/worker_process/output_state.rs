use std::sync::atomic::Ordering;

use super::WorkerManager;
use super::backend_driver::output_echo_source_for_backend;
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
        self.driver.clear_active_input();
        self.session_end_seen = false;
        if !preserve_detached_output {
            self.settled_pending_completion = None;
            self.settled_pending_error = None;
            self.last_detached_prefix_item_count = 0;
        }
        self.last_prompt = None;
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
        self.driver.clear_active_input();
        self.session_end_seen = false;
        if !preserve_detached_output {
            self.settled_pending_completion = None;
            self.settled_pending_error = None;
            self.last_detached_prefix_item_count = 0;
        }
        self.pager_prompt = None;
        self.last_prompt = None;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Backend;
    use crate::output_capture::{
        OUTPUT_RING_CAPACITY_BYTES, ensure_output_ring, reset_last_reply_marker_offset,
        reset_output_ring,
    };
    use crate::pending_output_tape::{PendingSidebandKind, PendingTextSource};
    use crate::sandbox_cli::SandboxCliPlan;
    use crate::worker_process::test_support::{contents_text, output_ring_test_guard};

    #[test]
    fn files_prepare_input_context_trims_echo_from_prompt_fallback_when_echo_events_missing() {
        let mut manager = WorkerManager::new(
            Backend::Python,
            SandboxCliPlan::default(),
            OversizedOutputMode::Files,
        )
        .expect("worker manager");
        manager
            .pending_output_tape
            .append_stdout_bytes(b">>> import time; time.sleep(0.2)\nDETACHED_OK\n");
        manager.pending_request_input = Some("import time; time.sleep(0.2)\n".to_string());
        manager.settled_pending_completion = Some(CompletionInfo {
            prompt: Some(">>> ".to_string()),
            prompt_variants: Some(vec![">>> ".to_string()]),
            echo_events: Vec::new(),
            protocol_warnings: Vec::new(),
            session_end_seen: false,
        });

        let context = manager.prepare_input_context_files();
        let text = contents_text(&context.detached_prefix_contents);

        assert!(
            text.contains("DETACHED_OK\n"),
            "expected the settled files-mode output to survive trimming, got: {text:?}"
        );
        assert!(
            !text.contains("import time; time.sleep(0.2)"),
            "did not expect the Python prompt echo to leak into the next files-mode reply, got: {text:?}"
        );
        assert!(
            manager.settled_pending_completion.is_none(),
            "expected settled completion metadata to be consumed with the detached prefix"
        );
    }

    #[test]
    fn files_reset_preserving_detached_output_keeps_pending_request_input_for_trim() {
        let mut manager = WorkerManager::new(
            Backend::Python,
            SandboxCliPlan::default(),
            OversizedOutputMode::Files,
        )
        .expect("worker manager");
        manager
            .pending_output_tape
            .append_stdout_bytes(b">>> import time; time.sleep(0.2)\nDETACHED_OK\n");
        manager.pending_request_input = Some("import time; time.sleep(0.2)\n".to_string());
        manager.settled_pending_completion = Some(CompletionInfo {
            prompt: Some(">>> ".to_string()),
            prompt_variants: Some(vec![">>> ".to_string()]),
            echo_events: Vec::new(),
            protocol_warnings: Vec::new(),
            session_end_seen: false,
        });

        manager.reset_output_state_files_preserving_detached_output();

        let context = manager.prepare_input_context_files();
        let text = contents_text(&context.detached_prefix_contents);

        assert!(
            text.contains("DETACHED_OK\n"),
            "expected detached files-mode output to survive the preserved reset, got: {text:?}"
        );
        assert!(
            !text.contains("import time; time.sleep(0.2)"),
            "did not expect the preserved reset to leak the original Python input echo, got: {text:?}"
        );
        assert!(
            manager.pending_request_input.is_none(),
            "expected preserved pending input to be consumed once the detached prefix is prepared"
        );
    }

    #[test]
    fn files_respawned_pending_request_trims_echo_without_settled_completion() {
        let mut manager = WorkerManager::new(
            Backend::Python,
            SandboxCliPlan::default(),
            OversizedOutputMode::Files,
        )
        .expect("worker manager");
        manager.pending_request = true;
        manager.last_prompt = Some(">>> ".to_string());
        manager
            .pending_output_tape
            .append_stdout_bytes(b">>> import time; time.sleep(0.2)\nDETACHED_OK\n");
        manager.pending_request_input = Some("import time; time.sleep(0.2)\n".to_string());

        manager.reset_output_state_files_preserving_detached_output();

        let context = manager.prepare_input_context_files();
        let text = contents_text(&context.detached_prefix_contents);

        assert!(
            text.contains("DETACHED_OK\n"),
            "expected aborted pending output to survive the respawned reset, got: {text:?}"
        );
        assert!(
            !text.contains("import time; time.sleep(0.2)"),
            "did not expect the aborted request echo to leak across the respawn boundary, got: {text:?}"
        );
        assert!(
            manager.pending_request_input.is_none(),
            "expected the aborted request input fallback to be consumed once the detached prefix is prepared"
        );
    }

    #[test]
    fn pager_respawned_pending_request_trims_echo_without_echo_events() {
        let mut manager = WorkerManager::new(
            Backend::Python,
            SandboxCliPlan::default(),
            OversizedOutputMode::Pager,
        )
        .expect("worker manager");
        let _guard = output_ring_test_guard();
        let _output_ring = ensure_output_ring(OUTPUT_RING_CAPACITY_BYTES);
        reset_output_ring();
        reset_last_reply_marker_offset();
        manager.pending_request = true;
        manager.last_prompt = Some(">>> ".to_string());
        manager.pending_request_input = Some("import time; time.sleep(0.2)\n".to_string());
        manager.output.start_capture();
        manager.output_timeline.append_ipc_text_with_continuation(
            b">>> import time; time.sleep(0.2)\nDETACHED_OK\n",
            false,
            ContentOrigin::Worker,
            false,
        );

        manager.reset_output_state_pager_preserving_detached_output(false);

        let context = manager.prepare_input_context_pager("1+1", false);
        let text = contents_text(&context.detached_prefix_contents);

        assert!(
            text.contains("DETACHED_OK\n"),
            "expected aborted pager output to survive the respawned reset, got: {text:?}"
        );
        assert!(
            !text.contains("import time; time.sleep(0.2)"),
            "did not expect the aborted pager echo to leak across the respawn boundary, got: {text:?}"
        );
    }

    #[test]
    fn files_prepare_input_context_seals_split_utf8_at_request_boundary() {
        let mut manager = WorkerManager::new(
            Backend::Python,
            SandboxCliPlan::default(),
            OversizedOutputMode::Files,
        )
        .expect("worker manager");
        manager.pending_output_tape.append_stdout_bytes(&[0xC3]);

        let first = manager.prepare_input_context_files();
        assert_eq!(
            contents_text(&first.detached_prefix_contents),
            "\\xC3",
            "expected an accepted request to seal the detached utf-8 lead byte into the prefix"
        );

        manager
            .pending_output_tape
            .append_stdout_bytes(&[0xA9, b'\n']);
        let second = manager.prepare_input_context_files();

        assert_eq!(
            contents_text(&second.detached_prefix_contents),
            "\\xA9\n",
            "expected the next request output to stay split after the detached prefix was sealed"
        );
    }

    #[test]
    fn files_nonfinal_drain_preserves_echo_only_input() {
        let manager = WorkerManager::new(
            Backend::R,
            SandboxCliPlan::default(),
            OversizedOutputMode::Files,
        )
        .expect("worker manager");

        manager
            .pending_output_tape
            .append_stdout_ipc_bytes(b"> Sys.sleep(5)\n");
        manager
            .pending_output_tape
            .append_sideband(PendingSidebandKind::ReadlineResult {
                prompt: "> ".to_string(),
                line: "Sys.sleep(5)\n".to_string(),
                echo_source: PendingTextSource::Ipc,
            });

        let formatted = manager.drain_formatted_output();

        assert_eq!(
            formatted.contents,
            vec![WorkerContent::stdout("> Sys.sleep(5)\n")],
            "expected an in-flight files-mode drain to keep the echoed command visible"
        );
    }

    #[test]
    fn files_nonfinal_drain_drops_leading_repl_echo_after_worker_output() {
        let manager = WorkerManager::new(
            Backend::R,
            SandboxCliPlan::default(),
            OversizedOutputMode::Files,
        )
        .expect("worker manager");

        manager
            .pending_output_tape
            .append_stdout_ipc_bytes(b"> Sys.sleep(5)\n");
        manager
            .pending_output_tape
            .append_sideband(PendingSidebandKind::ReadlineResult {
                prompt: "> ".to_string(),
                line: "Sys.sleep(5)\n".to_string(),
                echo_source: PendingTextSource::Ipc,
            });
        manager.pending_output_tape.append_stdout_bytes(b"start\n");

        let formatted = manager.drain_formatted_output();

        assert_eq!(
            formatted.contents,
            vec![WorkerContent::stdout("start\n")],
            "expected worker output to hide the leading timed-out REPL echo again"
        );
    }

    #[test]
    fn files_prepare_input_context_preserves_unsettled_echo_prefix() {
        let mut manager = WorkerManager::new(
            Backend::R,
            SandboxCliPlan::default(),
            OversizedOutputMode::Files,
        )
        .expect("worker manager");

        manager
            .pending_output_tape
            .append_stdout_ipc_bytes(b"> Sys.sleep(5)\n");
        manager
            .pending_output_tape
            .append_sideband(PendingSidebandKind::ReadlineResult {
                prompt: "> ".to_string(),
                line: "Sys.sleep(5)\n".to_string(),
                echo_source: PendingTextSource::Ipc,
            });

        let context = manager.prepare_input_context_files();

        assert_eq!(
            context.detached_prefix_contents,
            vec![WorkerContent::stdout("> Sys.sleep(5)\n")],
            "expected a sealed files-mode prefix without settled completion metadata to keep echoed input"
        );
    }

    #[test]
    fn files_preserved_detached_prefix_stays_separate_from_new_session_startup_output() {
        let mut manager = WorkerManager::new(
            Backend::Python,
            SandboxCliPlan::default(),
            OversizedOutputMode::Files,
        )
        .expect("worker manager");
        manager
            .pending_output_tape
            .append_stdout_bytes(b"OLD_TAIL\n");

        manager.reset_output_state_files_preserving_detached_output();
        manager.next_live_prefix_belongs_to_reply = true;
        manager
            .pending_output_tape
            .append_stdout_bytes(b"NEW_SESSION_STARTUP\n");

        let context = manager.prepare_input_context_files();

        assert_eq!(
            contents_text(&context.detached_prefix_contents),
            "OLD_TAIL\n",
            "expected preserved detached output to stay isolated from the replacement session"
        );
        assert_eq!(
            contents_text(&context.reply_prefix_contents),
            "NEW_SESSION_STARTUP\n",
            "expected fresh-session startup output to stay with the new reply prefix"
        );
    }
}
