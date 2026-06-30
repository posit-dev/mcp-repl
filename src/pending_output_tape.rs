use crate::output_capture::{OutputBuffer, OutputTimeline, ProjectionMode, TimelineSettleState};
use crate::worker_protocol::ContentOrigin;

pub(crate) use crate::output_capture::FormattedPendingOutput;

#[derive(Clone)]
pub(crate) struct PendingOutputTape {
    output: OutputBuffer,
    timeline: OutputTimeline,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PendingSidebandKind {
    InputWait { prompt: Option<String> },
    ReadlineResult { prompt: String, line: String },
    RequestBoundary,
    SessionEnd,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PendingOutputSettleState {
    pub progress_seq: u64,
    pub readline_results_seen: usize,
    pub has_image: bool,
}

impl PendingOutputTape {
    pub(crate) fn with_timeline(timeline: OutputTimeline) -> Self {
        let output = timeline.buffer();
        output.start_capture();
        Self { output, timeline }
    }

    #[cfg(test)]
    pub(crate) fn append_stdout_bytes(&self, bytes: &[u8]) {
        self.timeline
            .append_text(bytes, false, ContentOrigin::Worker);
    }

    #[cfg(test)]
    pub(crate) fn append_stderr_bytes(&self, bytes: &[u8]) {
        self.timeline
            .append_text(bytes, true, ContentOrigin::Worker);
    }

    #[cfg(test)]
    pub(crate) fn append_stdout_ipc_bytes(&self, bytes: &[u8]) {
        self.timeline
            .append_ipc_text_with_continuation(bytes, false, ContentOrigin::Worker, false);
    }

    pub(crate) fn append_server_stderr_bytes(&self, bytes: &[u8]) {
        self.timeline
            .append_text(bytes, true, ContentOrigin::Server);
    }

    pub(crate) fn append_server_stderr_status_line(&self, bytes: &[u8]) {
        self.append_server_stderr_bytes(bytes);
    }

    pub(crate) fn append_stdout_status_line(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.timeline.seal_utf8_tails();
        if self.timeline.last_text_ends_with_newline() || bytes.starts_with(b"\n") {
            self.timeline
                .append_text(bytes, false, ContentOrigin::Server);
            return;
        }
        let mut status = Vec::with_capacity(bytes.len().saturating_add(1));
        status.push(b'\n');
        status.extend_from_slice(bytes);
        self.timeline
            .append_text(&status, false, ContentOrigin::Server);
    }

    pub(crate) fn append_sideband(&self, kind: PendingSidebandKind) {
        match kind {
            PendingSidebandKind::InputWait { .. } => self.timeline.append_input_wait(),
            PendingSidebandKind::ReadlineResult { prompt, line } => {
                self.timeline.append_input_echo(&prompt, &line);
            }
            PendingSidebandKind::RequestBoundary => self.timeline.append_request_boundary(),
            PendingSidebandKind::SessionEnd => self.timeline.append_session_end(),
        }
    }

    pub(crate) fn has_pending(&self) -> bool {
        self.output.has_pending_output()
    }

    pub(crate) fn clear(&self) {
        self.timeline.clear();
        self.output.clear();
        self.output.start_capture();
    }

    pub(crate) fn current_seq(&self) -> u64 {
        self.output.current_progress_seq()
    }

    pub(crate) fn current_settle_state(&self) -> PendingOutputSettleState {
        let TimelineSettleState {
            progress_seq,
            input_echoes_seen,
            has_image,
        } = self.output.current_settle_state();
        PendingOutputSettleState {
            progress_seq,
            readline_results_seen: input_echoes_seen,
            has_image,
        }
    }

    pub(crate) fn drain_output(&self) -> FormattedPendingOutput {
        self.timeline.seal_utf8_tails_blocking_visible_output();
        self.output.drain_formatted(ProjectionMode::Bundle, false)
    }

    pub(crate) fn drain_final_output(&self) -> FormattedPendingOutput {
        self.timeline.seal_utf8_tails();
        self.output.drain_formatted(ProjectionMode::Bundle, true)
    }

    pub(crate) fn drain_sealed_output(&self) -> FormattedPendingOutput {
        self.timeline.seal_utf8_tails();
        self.output.drain_formatted(ProjectionMode::Bundle, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output_capture::OUTPUT_RING_CAPACITY_BYTES;
    use crate::worker_protocol::WorkerContent;

    fn tape() -> PendingOutputTape {
        PendingOutputTape::with_timeline(OutputTimeline::with_capacity(OUTPUT_RING_CAPACITY_BYTES))
    }

    #[test]
    fn marker_only_sideband_is_not_pending_output() {
        let tape = tape();

        tape.append_sideband(PendingSidebandKind::RequestBoundary);

        assert!(
            !tape.has_pending(),
            "marker-only sideband events should not make files-mode output pending"
        );
    }

    #[test]
    fn stderr_prefix_state_continues_across_drains() {
        let tape = tape();

        tape.append_stderr_bytes(b"abc");
        let first = tape.drain_output();
        assert_eq!(
            first.contents,
            vec![WorkerContent::worker_stderr("stderr: abc")]
        );

        tape.append_stderr_bytes(b"def\n");
        let second = tape.drain_output();
        assert_eq!(second.contents, vec![WorkerContent::worker_stderr("def\n")]);
    }

    #[test]
    fn transcript_only_input_echo_resets_stderr_prefix_state() {
        let tape = tape();

        tape.append_stderr_bytes(b"warning");
        tape.append_sideband(PendingSidebandKind::ReadlineResult {
            prompt: "> ".to_string(),
            line: "input\n".to_string(),
        });
        tape.append_stderr_bytes(b"more\n");

        let formatted = tape.drain_output();
        assert_eq!(
            formatted.contents,
            vec![
                WorkerContent::worker_stderr("stderr: warning"),
                WorkerContent::worker_stdout_transcript_only("> input\n"),
                WorkerContent::worker_stderr("stderr: more\n"),
            ]
        );
    }

    #[test]
    fn stdout_status_line_starts_after_partial_stdout() {
        let tape = tape();

        tape.append_stdout_bytes(b"partial");
        tape.append_stdout_status_line(b"[repl] session ended\n");

        let formatted = tape.drain_output();
        assert_eq!(
            formatted.contents,
            vec![
                WorkerContent::worker_stdout("partial"),
                WorkerContent::server_stdout("\n[repl] session ended\n"),
            ]
        );
    }

    #[test]
    fn stdout_status_line_flushes_incomplete_utf8_tail_before_separator() {
        let tape = tape();

        tape.append_stdout_bytes(&[0xC3]);
        tape.append_stdout_status_line(b"[repl] session ended\n");

        let formatted = tape.drain_final_output();
        assert_eq!(
            formatted.contents,
            vec![
                WorkerContent::worker_stdout("\\xC3"),
                WorkerContent::server_stdout("\n[repl] session ended\n"),
            ]
        );
    }

    #[test]
    fn clear_drops_incomplete_utf8_tail() {
        let tape = tape();

        tape.append_stdout_bytes(&[0xC3]);
        tape.clear();
        tape.append_stdout_bytes(&[0xA9, b'\n']);

        let formatted = tape.drain_final_output();
        assert_eq!(
            formatted.contents,
            vec![WorkerContent::worker_stdout("\\xA9\n")],
            "clear should not let an incomplete UTF-8 prefix merge with later output"
        );
    }
}
