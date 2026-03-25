use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use crate::worker_protocol::{ContentOrigin, TextStream, WorkerContent};

#[derive(Clone, Default)]
pub(crate) struct PendingOutputTape {
    inner: Arc<Mutex<PendingOutputTapeInner>>,
}

#[derive(Default)]
struct PendingOutputTapeInner {
    next_seq: u64,
    progress_seq: u64,
    events: VecDeque<PendingOutputEvent>,
    stdout_tail: PendingTextTail,
    stderr_tail: PendingTextTail,
}

#[derive(Default)]
struct PendingTextTail {
    bytes: Vec<u8>,
    origin: Option<ContentOrigin>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PendingOutputEvent {
    TextFragment {
        seq: u64,
        stream: TextStream,
        origin: ContentOrigin,
        bytes: Vec<u8>,
        terminated: bool,
    },
    Image {
        seq: u64,
        data: String,
        mime_type: String,
        id: String,
        is_new: bool,
    },
    Sideband {
        seq: u64,
        kind: PendingSidebandKind,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PendingSidebandKind {
    ReadlineStart { prompt: String },
    ReadlineResult { prompt: String, line: String },
    RequestEnd,
    SessionEnd,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PendingOutputSnapshot {
    pub events: Vec<PendingOutputEvent>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct FormattedPendingOutput {
    pub contents: Vec<WorkerContent>,
    pub saw_stderr: bool,
}

impl PendingOutputTape {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn append_stdout_bytes(&self, bytes: &[u8]) {
        self.append_bytes(bytes, TextStream::Stdout, ContentOrigin::Worker);
    }

    pub(crate) fn append_stderr_bytes(&self, bytes: &[u8]) {
        self.append_bytes(bytes, TextStream::Stderr, ContentOrigin::Worker);
    }

    pub(crate) fn append_server_stderr_bytes(&self, bytes: &[u8]) {
        self.append_bytes(bytes, TextStream::Stderr, ContentOrigin::Server);
    }

    pub(crate) fn append_stdout_status_line(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let mut guard = self
            .inner
            .lock()
            .expect("pending output tape mutex poisoned");
        note_progress(&mut guard);
        flush_tail(&mut guard, TextStream::Stdout, false);
        flush_tail(&mut guard, TextStream::Stderr, false);
        let needs_separator = last_text_fragment_bytes(&guard.events)
            .is_some_and(|last| !last.ends_with(b"\n"))
            && !bytes.starts_with(b"\n");
        let mut status_line = Vec::with_capacity(bytes.len() + usize::from(needs_separator));
        if needs_separator {
            status_line.push(b'\n');
        }
        status_line.extend_from_slice(bytes);
        append_complete_bytes(
            &mut guard,
            TextStream::Stdout,
            ContentOrigin::Server,
            &status_line,
        );
    }

    pub(crate) fn append_image(&self, id: String, mime_type: String, data: String, is_new: bool) {
        let mut guard = self
            .inner
            .lock()
            .expect("pending output tape mutex poisoned");
        note_progress(&mut guard);
        flush_tail(&mut guard, TextStream::Stdout, false);
        flush_tail(&mut guard, TextStream::Stderr, false);
        let seq = next_seq(&mut guard);
        guard.events.push_back(PendingOutputEvent::Image {
            seq,
            data,
            mime_type,
            id,
            is_new,
        });
    }

    pub(crate) fn append_sideband(&self, kind: PendingSidebandKind) {
        let mut guard = self
            .inner
            .lock()
            .expect("pending output tape mutex poisoned");
        note_progress(&mut guard);
        flush_tail(&mut guard, TextStream::Stdout, false);
        flush_tail(&mut guard, TextStream::Stderr, false);
        let seq = next_seq(&mut guard);
        guard
            .events
            .push_back(PendingOutputEvent::Sideband { seq, kind });
    }

    pub(crate) fn has_pending(&self) -> bool {
        let guard = self
            .inner
            .lock()
            .expect("pending output tape mutex poisoned");
        guard.events.iter().any(|event| {
            matches!(
                event,
                PendingOutputEvent::TextFragment { .. } | PendingOutputEvent::Image { .. }
            )
        }) || tail_has_flushable_bytes(&guard.stdout_tail)
            || tail_has_flushable_bytes(&guard.stderr_tail)
    }

    pub(crate) fn clear(&self) {
        let mut guard = self
            .inner
            .lock()
            .expect("pending output tape mutex poisoned");
        *guard = PendingOutputTapeInner::default();
    }

    pub(crate) fn current_seq(&self) -> u64 {
        let guard = self
            .inner
            .lock()
            .expect("pending output tape mutex poisoned");
        guard.progress_seq
    }

    pub(crate) fn drain_snapshot(&self) -> PendingOutputSnapshot {
        self.drain_snapshot_with_policy(false)
    }

    pub(crate) fn drain_final_snapshot(&self) -> PendingOutputSnapshot {
        self.drain_snapshot_with_policy(true)
    }

    fn drain_snapshot_with_policy(&self, flush_incomplete: bool) -> PendingOutputSnapshot {
        let mut guard = self
            .inner
            .lock()
            .expect("pending output tape mutex poisoned");
        flush_tail(&mut guard, TextStream::Stdout, flush_incomplete);
        flush_tail(&mut guard, TextStream::Stderr, flush_incomplete);
        PendingOutputSnapshot {
            events: guard.events.drain(..).collect(),
        }
    }

    fn append_bytes(&self, bytes: &[u8], stream: TextStream, origin: ContentOrigin) {
        if bytes.is_empty() {
            return;
        }
        let mut guard = self
            .inner
            .lock()
            .expect("pending output tape mutex poisoned");
        note_progress(&mut guard);
        flush_tail(&mut guard, other_stream(stream), false);
        if tail_mut(&mut guard, stream)
            .origin
            .is_some_and(|tail_origin| tail_origin != origin)
        {
            flush_tail(&mut guard, stream, false);
        }
        let tail = tail_mut(&mut guard, stream);
        if tail.origin.is_none() {
            tail.origin = Some(origin);
        }
        tail.bytes.extend_from_slice(bytes);
        commit_complete_lines(&mut guard, stream);
    }
}

impl PendingOutputSnapshot {
    pub(crate) fn format_contents(&self) -> FormattedPendingOutput {
        let mut formatted = FormattedPendingOutput::default();
        for event in &self.events {
            match event {
                PendingOutputEvent::TextFragment {
                    stream,
                    origin,
                    bytes,
                    ..
                } => {
                    if bytes.is_empty() {
                        continue;
                    }
                    if matches!(stream, TextStream::Stderr) {
                        formatted.saw_stderr = true;
                    }
                    let rendered = render_bytes(bytes);
                    if rendered.is_empty() {
                        continue;
                    }
                    let text = if matches!(stream, TextStream::Stderr) {
                        render_stderr_text(&formatted.contents, rendered)
                    } else {
                        rendered
                    };
                    push_text(&mut formatted.contents, *stream, *origin, text);
                }
                PendingOutputEvent::Image {
                    data,
                    mime_type,
                    id,
                    is_new,
                    ..
                } => formatted.contents.push(WorkerContent::ContentImage {
                    data: data.clone(),
                    mime_type: mime_type.clone(),
                    id: id.clone(),
                    is_new: *is_new,
                }),
                PendingOutputEvent::Sideband { .. } => {}
            }
        }
        formatted
    }
}

fn push_text(
    contents: &mut Vec<WorkerContent>,
    stream: TextStream,
    origin: ContentOrigin,
    text: String,
) {
    if text.is_empty() {
        return;
    }
    if let Some(WorkerContent::ContentText {
        text: existing,
        stream: existing_stream,
        origin: existing_origin,
    }) = contents.last_mut()
        && *existing_stream == stream
        && *existing_origin == origin
    {
        existing.push_str(&text);
        return;
    }
    contents.push(WorkerContent::ContentText {
        text,
        stream,
        origin,
    });
}

fn render_bytes(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut remaining = bytes;
    while !remaining.is_empty() {
        match std::str::from_utf8(remaining) {
            Ok(valid) => {
                out.push_str(valid);
                break;
            }
            Err(err) => {
                let valid_up_to = err.valid_up_to();
                if valid_up_to > 0 {
                    out.push_str(
                        std::str::from_utf8(&remaining[..valid_up_to]).expect("valid utf-8 prefix"),
                    );
                }
                let invalid_start = valid_up_to;
                let invalid_end = match err.error_len() {
                    Some(len) => invalid_start.saturating_add(len),
                    None => remaining.len(),
                };
                for byte in &remaining[invalid_start..invalid_end] {
                    let _ = write!(&mut out, "\\x{byte:02X}");
                }
                remaining = &remaining[invalid_end..];
            }
        }
    }
    out
}

fn next_seq(inner: &mut PendingOutputTapeInner) -> u64 {
    let seq = inner.next_seq;
    inner.next_seq = inner.next_seq.saturating_add(1);
    seq
}

fn note_progress(inner: &mut PendingOutputTapeInner) {
    inner.progress_seq = inner.progress_seq.saturating_add(1);
}

fn render_stderr_text(existing_contents: &[WorkerContent], rendered: String) -> String {
    let needs_separator = existing_contents
        .last()
        .and_then(last_text_content)
        .is_some_and(|text| !text.ends_with('\n'))
        && !rendered.starts_with('\n');
    if needs_separator {
        format!("\nstderr: {rendered}")
    } else {
        format!("stderr: {rendered}")
    }
}

fn last_text_content(content: &WorkerContent) -> Option<&str> {
    match content {
        WorkerContent::ContentText { text, .. } => Some(text.as_str()),
        WorkerContent::ContentImage { .. } => None,
    }
}

fn other_stream(stream: TextStream) -> TextStream {
    match stream {
        TextStream::Stdout => TextStream::Stderr,
        TextStream::Stderr => TextStream::Stdout,
    }
}

fn tail_mut(inner: &mut PendingOutputTapeInner, stream: TextStream) -> &mut PendingTextTail {
    match stream {
        TextStream::Stdout => &mut inner.stdout_tail,
        TextStream::Stderr => &mut inner.stderr_tail,
    }
}

fn append_complete_bytes(
    inner: &mut PendingOutputTapeInner,
    stream: TextStream,
    origin: ContentOrigin,
    bytes: &[u8],
) {
    if bytes.is_empty() {
        return;
    }
    let seq = next_seq(inner);
    inner.events.push_back(PendingOutputEvent::TextFragment {
        seq,
        stream,
        origin,
        bytes: bytes.to_vec(),
        terminated: bytes.ends_with(b"\n"),
    });
}

fn commit_complete_lines(inner: &mut PendingOutputTapeInner, stream: TextStream) {
    loop {
        let (origin, line, tail_empty) = {
            let tail = tail_mut(inner, stream);
            let Some(newline_idx) = tail.bytes.iter().position(|byte| *byte == b'\n') else {
                break;
            };
            let origin = tail
                .origin
                .expect("text tail should record origin while bytes are buffered");
            let line = tail.bytes.drain(..=newline_idx).collect::<Vec<u8>>();
            let tail_empty = tail.bytes.is_empty();
            if tail_empty {
                tail.origin = None;
            }
            (origin, line, tail_empty)
        };
        let seq = next_seq(inner);
        inner.events.push_back(PendingOutputEvent::TextFragment {
            seq,
            stream,
            origin,
            bytes: line,
            terminated: true,
        });
        if tail_empty {
            break;
        }
    }
}

fn flush_tail(inner: &mut PendingOutputTapeInner, stream: TextStream, flush_incomplete: bool) {
    let (origin, bytes) = {
        let tail = tail_mut(inner, stream);
        if tail.bytes.is_empty() {
            return;
        }
        let mut flush_len = flushable_prefix_len(&tail.bytes);
        if flush_incomplete && flush_len == 0 {
            flush_len = tail.bytes.len();
        }
        if flush_len == 0 {
            return;
        }
        let origin = tail
            .origin
            .expect("text tail should record origin while bytes are buffered");
        let bytes = tail.bytes.drain(..flush_len).collect::<Vec<u8>>();
        if tail.bytes.is_empty() {
            tail.origin = None;
        }
        (origin, bytes)
    };
    let seq = next_seq(inner);
    inner.events.push_back(PendingOutputEvent::TextFragment {
        seq,
        stream,
        origin,
        bytes,
        terminated: false,
    });
}

fn last_text_fragment_bytes(events: &VecDeque<PendingOutputEvent>) -> Option<&[u8]> {
    match events.back() {
        Some(PendingOutputEvent::TextFragment { bytes, .. }) => Some(bytes.as_slice()),
        Some(PendingOutputEvent::Image { .. } | PendingOutputEvent::Sideband { .. }) | None => None,
    }
}

fn tail_has_flushable_bytes(tail: &PendingTextTail) -> bool {
    flushable_prefix_len(&tail.bytes) > 0
}

fn flushable_prefix_len(bytes: &[u8]) -> usize {
    let mut offset: usize = 0;
    let mut remaining = bytes;
    while !remaining.is_empty() {
        match std::str::from_utf8(remaining) {
            Ok(_) => return bytes.len(),
            Err(err) => {
                let valid_up_to = err.valid_up_to();
                if let Some(error_len) = err.error_len() {
                    let invalid_end = valid_up_to.saturating_add(error_len);
                    offset = offset.saturating_add(invalid_end);
                    remaining = &remaining[invalid_end..];
                } else {
                    return offset.saturating_add(valid_up_to);
                }
            }
        }
    }
    bytes.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interleaved_streams_flush_partial_fragments() {
        let tape = PendingOutputTape::new();
        tape.append_stdout_bytes(b"abc");
        tape.append_stderr_bytes(b"boom\n");

        let snapshot = tape.drain_snapshot();
        assert_eq!(
            snapshot.events,
            vec![
                PendingOutputEvent::TextFragment {
                    seq: 0,
                    stream: TextStream::Stdout,
                    origin: ContentOrigin::Worker,
                    bytes: b"abc".to_vec(),
                    terminated: false,
                },
                PendingOutputEvent::TextFragment {
                    seq: 1,
                    stream: TextStream::Stderr,
                    origin: ContentOrigin::Worker,
                    bytes: b"boom\n".to_vec(),
                    terminated: true,
                },
            ]
        );
    }

    #[test]
    fn sideband_events_preserve_order_with_text() {
        let tape = PendingOutputTape::new();
        tape.append_stdout_bytes(b"> 1+\n");
        tape.append_sideband(PendingSidebandKind::ReadlineResult {
            prompt: "> ".to_string(),
            line: "1+\n".to_string(),
        });
        tape.append_stdout_bytes(b"[1] 2\n");

        let snapshot = tape.drain_snapshot();
        assert!(matches!(
            snapshot.events[1],
            PendingOutputEvent::Sideband {
                kind: PendingSidebandKind::ReadlineResult { .. },
                ..
            }
        ));
    }

    #[test]
    fn invalid_utf8_bytes_render_as_hex_escapes() {
        let tape = PendingOutputTape::new();
        tape.append_stdout_bytes(b"ok \xFF\xFE done\n");

        let snapshot = tape.drain_snapshot();
        let formatted = snapshot.format_contents();
        assert_eq!(
            formatted.contents,
            vec![WorkerContent::stdout("ok \\xFF\\xFE done\n")]
        );
    }

    #[test]
    fn progress_seq_tracks_partial_line_appends() {
        let tape = PendingOutputTape::new();
        tape.append_stdout_bytes(b"abc");
        let first = tape.current_seq();
        tape.append_stdout_bytes(b"def");
        let second = tape.current_seq();

        assert!(
            second > first,
            "progress counter should advance on tail-only appends"
        );
    }

    #[test]
    fn stderr_after_partial_stdout_starts_on_new_line() {
        let tape = PendingOutputTape::new();
        tape.append_stdout_bytes(b"x");
        tape.append_stderr_bytes(b"boom\n");

        let snapshot = tape.drain_snapshot();
        let formatted = snapshot.format_contents();
        assert_eq!(
            formatted.contents,
            vec![
                WorkerContent::stdout("x"),
                WorkerContent::stderr("\nstderr: boom\n")
            ]
        );
    }

    #[test]
    fn clean_session_end_notice_starts_after_partial_stdout() {
        let tape = PendingOutputTape::new();
        tape.append_stdout_bytes(b"x");
        tape.append_stdout_status_line(b"[repl] session ended\n");

        let snapshot = tape.drain_snapshot();
        let formatted = snapshot.format_contents();
        assert_eq!(
            formatted.contents,
            vec![
                WorkerContent::ContentText {
                    text: "x".to_string(),
                    stream: TextStream::Stdout,
                    origin: ContentOrigin::Worker,
                },
                WorkerContent::ContentText {
                    text: "\n[repl] session ended\n".to_string(),
                    stream: TextStream::Stdout,
                    origin: ContentOrigin::Server,
                },
            ]
        );
    }

    #[test]
    fn server_stderr_notice_preserves_server_origin() {
        let tape = PendingOutputTape::new();
        tape.append_server_stderr_bytes(b"[repl] guardrail\n");

        let snapshot = tape.drain_snapshot();
        let formatted = snapshot.format_contents();
        assert_eq!(
            formatted.contents,
            vec![WorkerContent::ContentText {
                text: "stderr: [repl] guardrail\n".to_string(),
                stream: TextStream::Stderr,
                origin: ContentOrigin::Server,
            }]
        );
    }

    #[test]
    fn buffered_server_stderr_tail_preserves_server_origin_when_flushed() {
        let tape = PendingOutputTape::new();
        tape.append_server_stderr_bytes(b"[repl] guardrail");
        tape.append_stdout_bytes(b"ok\n");

        let snapshot = tape.drain_snapshot();
        assert_eq!(
            snapshot.events,
            vec![
                PendingOutputEvent::TextFragment {
                    seq: 0,
                    stream: TextStream::Stderr,
                    origin: ContentOrigin::Server,
                    bytes: b"[repl] guardrail".to_vec(),
                    terminated: false,
                },
                PendingOutputEvent::TextFragment {
                    seq: 1,
                    stream: TextStream::Stdout,
                    origin: ContentOrigin::Worker,
                    bytes: b"ok\n".to_vec(),
                    terminated: true,
                },
            ]
        );
    }

    #[test]
    fn split_utf8_sequence_is_preserved_across_snapshot_drains() {
        let tape = PendingOutputTape::new();

        tape.append_stdout_bytes(&[0xC3]);
        let first = tape.drain_snapshot();
        assert!(
            first.format_contents().contents.is_empty(),
            "incomplete utf-8 prefix should stay buffered across drain boundaries"
        );

        tape.append_stdout_bytes(&[0xA9, b'\n']);
        let second = tape.drain_snapshot();
        assert_eq!(
            second.format_contents().contents,
            vec![WorkerContent::stdout("é\n")]
        );
    }

    #[test]
    fn final_snapshot_flushes_incomplete_utf8_as_hex_escape() {
        let tape = PendingOutputTape::new();
        tape.append_stdout_bytes(&[0xC3]);

        let snapshot = tape.drain_final_snapshot();
        assert_eq!(
            snapshot.format_contents().contents,
            vec![WorkerContent::stdout("\\xC3")]
        );
    }
}
