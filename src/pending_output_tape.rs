use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use crate::worker_protocol::{TextStream, WorkerContent};

#[derive(Clone, Default)]
pub(crate) struct PendingOutputTape {
    inner: Arc<Mutex<PendingOutputTapeInner>>,
}

#[derive(Default)]
struct PendingOutputTapeInner {
    next_seq: u64,
    events: VecDeque<PendingOutputEvent>,
    stdout_tail: Vec<u8>,
    stderr_tail: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PendingOutputEvent {
    TextFragment {
        seq: u64,
        stream: TextStream,
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
        self.append_bytes(bytes, TextStream::Stdout);
    }

    pub(crate) fn append_stderr_bytes(&self, bytes: &[u8]) {
        self.append_bytes(bytes, TextStream::Stderr);
    }

    pub(crate) fn append_image(&self, id: String, mime_type: String, data: String, is_new: bool) {
        let mut guard = self
            .inner
            .lock()
            .expect("pending output tape mutex poisoned");
        flush_tail(&mut guard, TextStream::Stdout);
        flush_tail(&mut guard, TextStream::Stderr);
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
        flush_tail(&mut guard, TextStream::Stdout);
        flush_tail(&mut guard, TextStream::Stderr);
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
        !guard.events.is_empty() || !guard.stdout_tail.is_empty() || !guard.stderr_tail.is_empty()
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
        guard.next_seq
    }

    pub(crate) fn drain_snapshot(&self) -> PendingOutputSnapshot {
        let mut guard = self
            .inner
            .lock()
            .expect("pending output tape mutex poisoned");
        flush_tail(&mut guard, TextStream::Stdout);
        flush_tail(&mut guard, TextStream::Stderr);
        PendingOutputSnapshot {
            events: guard.events.drain(..).collect(),
        }
    }

    fn append_bytes(&self, bytes: &[u8], stream: TextStream) {
        if bytes.is_empty() {
            return;
        }
        let mut guard = self
            .inner
            .lock()
            .expect("pending output tape mutex poisoned");
        flush_tail(&mut guard, other_stream(stream));
        tail_mut(&mut guard, stream).extend_from_slice(bytes);
        loop {
            let line = {
                let tail = tail_mut(&mut guard, stream);
                let Some(newline_idx) = tail.iter().position(|byte| *byte == b'\n') else {
                    break;
                };
                tail.drain(..=newline_idx).collect::<Vec<u8>>()
            };
            let seq = next_seq(&mut guard);
            guard.events.push_back(PendingOutputEvent::TextFragment {
                seq,
                stream,
                bytes: line,
                terminated: true,
            });
        }
    }
}

impl PendingOutputSnapshot {
    pub(crate) fn format_contents(&self) -> FormattedPendingOutput {
        let mut formatted = FormattedPendingOutput::default();
        for event in &self.events {
            match event {
                PendingOutputEvent::TextFragment { stream, bytes, .. } => {
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
                        format!("stderr: {rendered}")
                    } else {
                        rendered
                    };
                    push_text(&mut formatted.contents, *stream, text);
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

fn push_text(contents: &mut Vec<WorkerContent>, stream: TextStream, text: String) {
    if text.is_empty() {
        return;
    }
    if let Some(WorkerContent::ContentText {
        text: existing,
        stream: existing_stream,
    }) = contents.last_mut()
        && *existing_stream == stream
    {
        existing.push_str(&text);
        return;
    }
    contents.push(WorkerContent::ContentText { text, stream });
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

fn other_stream(stream: TextStream) -> TextStream {
    match stream {
        TextStream::Stdout => TextStream::Stderr,
        TextStream::Stderr => TextStream::Stdout,
    }
}

fn tail_mut(inner: &mut PendingOutputTapeInner, stream: TextStream) -> &mut Vec<u8> {
    match stream {
        TextStream::Stdout => &mut inner.stdout_tail,
        TextStream::Stderr => &mut inner.stderr_tail,
    }
}

fn flush_tail(inner: &mut PendingOutputTapeInner, stream: TextStream) {
    let bytes = {
        let tail = tail_mut(inner, stream);
        if tail.is_empty() {
            return;
        }
        std::mem::take(tail)
    };
    let seq = next_seq(inner);
    inner.events.push_back(PendingOutputEvent::TextFragment {
        seq,
        stream,
        bytes,
        terminated: false,
    });
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
                    bytes: b"abc".to_vec(),
                    terminated: false,
                },
                PendingOutputEvent::TextFragment {
                    seq: 1,
                    stream: TextStream::Stderr,
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
}
