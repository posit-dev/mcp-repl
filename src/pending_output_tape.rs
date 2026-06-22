use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use crate::output_capture::{
    OutputEvent, OutputEventKind, OutputRange, OutputTextSource, OutputTextSpan,
};
use crate::pager;
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
    drained_readline_results: usize,
    last_rendered_text: Option<RenderedTextState>,
}

#[derive(Default)]
struct PendingTextTail {
    bytes: Vec<u8>,
    origin: Option<ContentOrigin>,
    source: Option<PendingTextSource>,
    start_seq: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PendingOutputEvent {
    TextFragment {
        seq: u64,
        stream: TextStream,
        origin: ContentOrigin,
        source: PendingTextSource,
        bytes: Vec<u8>,
        terminated: bool,
    },
    Image {
        seq: u64,
        data: String,
        mime_type: String,
        id: String,
        is_new: bool,
        readline_results_seen: usize,
    },
    TextEvent {
        seq: u64,
        text: String,
        is_stderr: bool,
        origin: ContentOrigin,
        readline_results_seen: Option<usize>,
    },
    Sideband {
        seq: u64,
        kind: PendingSidebandKind,
    },
}

impl PendingOutputEvent {
    fn seq(&self) -> u64 {
        match self {
            Self::TextFragment { seq, .. }
            | Self::Image { seq, .. }
            | Self::TextEvent { seq, .. }
            | Self::Sideband { seq, .. } => *seq,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PendingSidebandKind {
    InputWait { prompt: String },
    ReadlineResult { prompt: String, line: String },
    RequestBoundary,
    SessionEnd,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PendingOutputSnapshot {
    pub events: Vec<PendingOutputEvent>,
    prior_rendered_text: Option<RenderedTextState>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct FormattedPendingOutput {
    pub contents: Vec<WorkerContent>,
    pub saw_stderr: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PendingOutputSettleState {
    pub progress_seq: u64,
    pub readline_results_seen: usize,
    pub has_image: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RenderedTextState {
    stream: TextStream,
    origin: ContentOrigin,
    terminated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PendingTextSource {
    Raw,
    Ipc,
}

struct RenderedPendingOutput {
    range: OutputRange,
    saw_stderr: bool,
}

impl PendingOutputTape {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn append_stdout_bytes(&self, bytes: &[u8]) {
        self.append_bytes(
            bytes,
            TextStream::Stdout,
            ContentOrigin::Worker,
            PendingTextSource::Raw,
        );
    }

    pub(crate) fn append_stdout_ipc_bytes(&self, bytes: &[u8]) {
        self.append_bytes(
            bytes,
            TextStream::Stdout,
            ContentOrigin::Worker,
            PendingTextSource::Ipc,
        );
    }

    pub(crate) fn append_stderr_bytes(&self, bytes: &[u8]) {
        self.append_bytes(
            bytes,
            TextStream::Stderr,
            ContentOrigin::Worker,
            PendingTextSource::Raw,
        );
    }

    pub(crate) fn append_stderr_ipc_bytes(&self, bytes: &[u8]) {
        self.append_bytes(
            bytes,
            TextStream::Stderr,
            ContentOrigin::Worker,
            PendingTextSource::Ipc,
        );
    }

    pub(crate) fn append_server_stderr_bytes(&self, bytes: &[u8]) {
        self.append_bytes(
            bytes,
            TextStream::Stderr,
            ContentOrigin::Server,
            PendingTextSource::Raw,
        );
    }

    pub(crate) fn append_server_stderr_status_line(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let mut guard = self
            .inner
            .lock()
            .expect("pending output tape mutex poisoned");
        note_progress(&mut guard);
        flush_tail(&mut guard, TextStream::Stdout, true);
        flush_tail(&mut guard, TextStream::Stderr, true);
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
            TextStream::Stderr,
            ContentOrigin::Server,
            PendingTextSource::Raw,
            &status_line,
        );
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
        flush_tail(&mut guard, TextStream::Stdout, true);
        flush_tail(&mut guard, TextStream::Stderr, true);
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
            PendingTextSource::Raw,
            &status_line,
        );
    }

    pub(crate) fn append_stdout_status_event(&self, text: String, readline_results_seen: usize) {
        if text.is_empty() {
            return;
        }
        let mut guard = self
            .inner
            .lock()
            .expect("pending output tape mutex poisoned");
        note_progress(&mut guard);
        flush_tail(&mut guard, TextStream::Stdout, false);
        flush_tail(&mut guard, TextStream::Stderr, false);
        let seq = next_seq(&mut guard);
        append_event(
            &mut guard,
            PendingOutputEvent::TextEvent {
                seq,
                text,
                is_stderr: false,
                origin: ContentOrigin::Server,
                readline_results_seen: Some(readline_results_seen),
            },
        );
    }

    pub(crate) fn append_image(
        &self,
        id: String,
        mime_type: String,
        data: String,
        is_new: bool,
        readline_results_seen: usize,
    ) {
        let mut guard = self
            .inner
            .lock()
            .expect("pending output tape mutex poisoned");
        note_progress(&mut guard);
        flush_tail(&mut guard, TextStream::Stdout, false);
        flush_tail(&mut guard, TextStream::Stderr, false);
        let seq = next_seq(&mut guard);
        append_event(
            &mut guard,
            PendingOutputEvent::Image {
                seq,
                data,
                mime_type,
                id,
                is_new,
                readline_results_seen,
            },
        );
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
        append_event(&mut guard, PendingOutputEvent::Sideband { seq, kind });
    }

    pub(crate) fn has_pending(&self) -> bool {
        let guard = self
            .inner
            .lock()
            .expect("pending output tape mutex poisoned");
        guard.events.iter().any(|event| {
            matches!(
                event,
                PendingOutputEvent::TextFragment { .. }
                    | PendingOutputEvent::Image { .. }
                    | PendingOutputEvent::TextEvent { .. }
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

    pub(crate) fn current_settle_state(&self) -> PendingOutputSettleState {
        let guard = self
            .inner
            .lock()
            .expect("pending output tape mutex poisoned");
        let pending_readline_results = guard
            .events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    PendingOutputEvent::Sideband {
                        kind: PendingSidebandKind::ReadlineResult { .. },
                        ..
                    }
                )
            })
            .count();
        let has_image = guard
            .events
            .iter()
            .any(|event| matches!(event, PendingOutputEvent::Image { .. }));
        PendingOutputSettleState {
            progress_seq: guard.progress_seq,
            readline_results_seen: guard.drained_readline_results + pending_readline_results,
            has_image,
        }
    }

    pub(crate) fn drain_snapshot(&self) -> PendingOutputSnapshot {
        self.drain_snapshot_with_policy(false)
    }

    pub(crate) fn drain_final_snapshot(&self) -> PendingOutputSnapshot {
        self.drain_snapshot_with_policy(false)
    }

    pub(crate) fn drain_sealed_snapshot(&self) -> PendingOutputSnapshot {
        self.drain_snapshot_with_policy(true)
    }

    fn drain_snapshot_with_policy(&self, flush_incomplete: bool) -> PendingOutputSnapshot {
        let mut guard = self
            .inner
            .lock()
            .expect("pending output tape mutex poisoned");
        flush_tail(&mut guard, TextStream::Stdout, flush_incomplete);
        flush_tail(&mut guard, TextStream::Stderr, flush_incomplete);
        let prior_rendered_text = guard.last_rendered_text;
        let events: Vec<_> = guard.events.drain(..).collect();
        let drained_readline_results = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    PendingOutputEvent::Sideband {
                        kind: PendingSidebandKind::ReadlineResult { .. },
                        ..
                    }
                )
            })
            .count();
        guard.drained_readline_results = guard
            .drained_readline_results
            .saturating_add(drained_readline_results);
        if events.iter().any(|event| {
            matches!(
                event,
                PendingOutputEvent::Sideband {
                    kind: PendingSidebandKind::RequestBoundary | PendingSidebandKind::SessionEnd,
                    ..
                }
            )
        }) {
            guard.drained_readline_results = 0;
        }
        guard.last_rendered_text = rendered_text_state_after(events.iter(), prior_rendered_text);
        PendingOutputSnapshot {
            events,
            prior_rendered_text,
        }
    }

    fn append_bytes(
        &self,
        bytes: &[u8],
        stream: TextStream,
        origin: ContentOrigin,
        source: PendingTextSource,
    ) {
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
            || tail_mut(&mut guard, stream)
                .source
                .is_some_and(|tail_source| tail_source != source)
        {
            flush_tail(&mut guard, stream, true);
        }
        if tail_mut(&mut guard, stream).bytes.is_empty() {
            let seq = next_seq(&mut guard);
            let tail = tail_mut(&mut guard, stream);
            tail.start_seq = Some(seq);
        }
        let tail = tail_mut(&mut guard, stream);
        if tail.origin.is_none() {
            tail.origin = Some(origin);
        }
        if tail.source.is_none() {
            tail.source = Some(source);
        }
        tail.bytes.extend_from_slice(bytes);
        commit_complete_lines(&mut guard, stream);
    }
}

impl PendingOutputSnapshot {
    pub(crate) fn format_contents(&self) -> FormattedPendingOutput {
        self.format_contents_preserving_output()
    }

    pub(crate) fn format_contents_for_reply(&self) -> FormattedPendingOutput {
        self.format_contents_preserving_output()
    }

    fn format_contents_preserving_output(&self) -> FormattedPendingOutput {
        let RenderedPendingOutput { range, saw_stderr } = self.rendered_output();
        let contents = pager::contents_from_output_range(range);
        FormattedPendingOutput {
            contents,
            saw_stderr,
        }
    }

    fn rendered_output(&self) -> RenderedPendingOutput {
        let mut bytes = Vec::new();
        let mut text_spans = Vec::new();
        let mut events = Vec::new();
        let mut saw_stderr = false;
        let mut last_rendered_text = self.prior_rendered_text;

        for event in &self.events {
            match event {
                PendingOutputEvent::TextFragment {
                    stream,
                    origin,
                    source,
                    bytes: fragment,
                    terminated,
                    ..
                } => {
                    if fragment.is_empty() {
                        continue;
                    }
                    if matches!(stream, TextStream::Stderr) {
                        saw_stderr = true;
                    }
                    let rendered = render_bytes(fragment);
                    if rendered.is_empty() {
                        continue;
                    }
                    let text = if matches!(stream, TextStream::Stderr) {
                        render_stderr_text(last_rendered_text, *origin, rendered)
                    } else {
                        rendered
                    };
                    append_rendered_text(
                        &mut bytes,
                        &mut text_spans,
                        text.as_bytes(),
                        *stream,
                        *origin,
                        (*source).into(),
                    );
                    last_rendered_text = Some(RenderedTextState {
                        stream: *stream,
                        origin: *origin,
                        terminated: *terminated,
                    });
                }
                PendingOutputEvent::Image {
                    data,
                    mime_type,
                    id,
                    is_new,
                    ..
                } => {
                    events.push(OutputEvent {
                        offset: bytes.len() as u64,
                        kind: OutputEventKind::Image {
                            data: data.clone(),
                            mime_type: mime_type.clone(),
                            id: id.clone(),
                            is_new: *is_new,
                        },
                    });
                    last_rendered_text = None;
                }
                PendingOutputEvent::TextEvent {
                    text,
                    is_stderr,
                    origin,
                    ..
                } => {
                    events.push(OutputEvent {
                        offset: bytes.len() as u64,
                        kind: OutputEventKind::Text {
                            text: text.clone(),
                            is_stderr: *is_stderr,
                            origin: *origin,
                        },
                    });
                    last_rendered_text = None;
                }
                PendingOutputEvent::Sideband { kind, .. } => match kind {
                    PendingSidebandKind::InputWait { .. }
                    | PendingSidebandKind::ReadlineResult { .. }
                    | PendingSidebandKind::RequestBoundary
                    | PendingSidebandKind::SessionEnd => {}
                },
            }
        }

        RenderedPendingOutput {
            range: OutputRange {
                start_offset: 0,
                end_offset: bytes.len() as u64,
                bytes,
                events,
                text_spans,
            },
            saw_stderr,
        }
    }
}

fn append_rendered_text(
    bytes: &mut Vec<u8>,
    text_spans: &mut Vec<OutputTextSpan>,
    text: &[u8],
    stream: TextStream,
    origin: ContentOrigin,
    source: OutputTextSource,
) {
    if text.is_empty() {
        return;
    }
    let is_stderr = matches!(stream, TextStream::Stderr);
    let start_byte = bytes.len();
    bytes.extend_from_slice(text);
    let end_byte = bytes.len();
    if let Some(last) = text_spans.last_mut()
        && last.is_stderr == is_stderr
        && last.origin == origin
        && last.source == source
        && last.end_byte == start_byte
    {
        last.end_byte = end_byte;
    } else {
        text_spans.push(OutputTextSpan {
            start_byte,
            end_byte,
            is_stderr,
            origin,
            source,
        });
    }
}

impl From<PendingTextSource> for OutputTextSource {
    fn from(source: PendingTextSource) -> Self {
        match source {
            PendingTextSource::Raw => OutputTextSource::Raw,
            PendingTextSource::Ipc => OutputTextSource::Ipc,
        }
    }
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

fn render_stderr_text(
    previous_text: Option<RenderedTextState>,
    origin: ContentOrigin,
    rendered: String,
) -> String {
    if previous_text.is_some_and(|state| {
        matches!(state.stream, TextStream::Stderr) && state.origin == origin && !state.terminated
    }) {
        return rendered;
    }
    let needs_separator =
        previous_text.is_some_and(|state| !state.terminated) && !rendered.starts_with('\n');
    if needs_separator {
        format!("\nstderr: {rendered}")
    } else {
        format!("stderr: {rendered}")
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
    source: PendingTextSource,
    bytes: &[u8],
) {
    if bytes.is_empty() {
        return;
    }
    let seq = next_seq(inner);
    append_event(
        inner,
        PendingOutputEvent::TextFragment {
            seq,
            stream,
            origin,
            source,
            bytes: bytes.to_vec(),
            terminated: bytes.ends_with(b"\n"),
        },
    );
}

fn commit_complete_lines(inner: &mut PendingOutputTapeInner, stream: TextStream) {
    loop {
        let (seq, origin, source, line, tail_empty) = {
            let tail = tail_mut(inner, stream);
            let Some(newline_idx) = tail.bytes.iter().position(|byte| *byte == b'\n') else {
                break;
            };
            let seq = tail
                .start_seq
                .expect("text tail should reserve a sequence while bytes are buffered");
            let origin = tail
                .origin
                .expect("text tail should record origin while bytes are buffered");
            let source = tail
                .source
                .expect("text tail should record source while bytes are buffered");
            let line = tail.bytes.drain(..=newline_idx).collect::<Vec<u8>>();
            let tail_empty = tail.bytes.is_empty();
            if tail_empty {
                tail.origin = None;
                tail.source = None;
                tail.start_seq = None;
            }
            (seq, origin, source, line, tail_empty)
        };
        append_event(
            inner,
            PendingOutputEvent::TextFragment {
                seq,
                stream,
                origin,
                source,
                bytes: line,
                terminated: true,
            },
        );
        if !tail_empty {
            let next = next_seq(inner);
            let tail = tail_mut(inner, stream);
            tail.start_seq = Some(next);
        }
    }
}

fn flush_tail(inner: &mut PendingOutputTapeInner, stream: TextStream, flush_incomplete: bool) {
    let (seq, origin, source, bytes, tail_empty) = {
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
        let seq = tail
            .start_seq
            .expect("text tail should reserve a sequence while bytes are buffered");
        let origin = tail
            .origin
            .expect("text tail should record origin while bytes are buffered");
        let source = tail
            .source
            .expect("text tail should record source while bytes are buffered");
        let bytes = tail.bytes.drain(..flush_len).collect::<Vec<u8>>();
        let tail_empty = tail.bytes.is_empty();
        if tail_empty {
            tail.origin = None;
            tail.source = None;
            tail.start_seq = None;
        }
        (seq, origin, source, bytes, tail_empty)
    };
    append_event(
        inner,
        PendingOutputEvent::TextFragment {
            seq,
            stream,
            origin,
            source,
            bytes,
            terminated: false,
        },
    );
    if !tail_empty {
        let next = next_seq(inner);
        let tail = tail_mut(inner, stream);
        tail.start_seq = Some(next);
    }
}

fn append_event(inner: &mut PendingOutputTapeInner, event: PendingOutputEvent) {
    let seq = event.seq();
    if inner.events.back().is_none_or(|last| last.seq() < seq) {
        inner.events.push_back(event);
        return;
    }
    let idx = inner
        .events
        .iter()
        .position(|existing| existing.seq() > seq)
        .unwrap_or(inner.events.len());
    inner.events.insert(idx, event);
}

fn last_text_fragment_bytes(events: &VecDeque<PendingOutputEvent>) -> Option<&[u8]> {
    match events.back() {
        Some(PendingOutputEvent::TextFragment { bytes, .. }) => Some(bytes.as_slice()),
        Some(
            PendingOutputEvent::Image { .. }
            | PendingOutputEvent::TextEvent { .. }
            | PendingOutputEvent::Sideband { .. },
        )
        | None => None,
    }
}

fn rendered_text_state_after<'a>(
    events: impl Iterator<Item = &'a PendingOutputEvent>,
    mut state: Option<RenderedTextState>,
) -> Option<RenderedTextState> {
    for event in events {
        match event {
            PendingOutputEvent::TextFragment {
                stream,
                origin,
                bytes,
                terminated,
                ..
            } => {
                if !bytes.is_empty() {
                    state = Some(RenderedTextState {
                        stream: *stream,
                        origin: *origin,
                        terminated: *terminated,
                    });
                }
            }
            PendingOutputEvent::Image { .. } | PendingOutputEvent::TextEvent { .. } => state = None,
            PendingOutputEvent::Sideband { .. } => {}
        }
    }
    state
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
                    source: PendingTextSource::Raw,
                    bytes: b"abc".to_vec(),
                    terminated: false,
                },
                PendingOutputEvent::TextFragment {
                    seq: 1,
                    stream: TextStream::Stderr,
                    origin: ContentOrigin::Worker,
                    source: PendingTextSource::Raw,
                    bytes: b"boom\n".to_vec(),
                    terminated: true,
                },
            ]
        );
    }

    #[test]
    fn sideband_events_preserve_order_with_text() {
        let tape = PendingOutputTape::new();
        tape.append_stdout_ipc_bytes(b"> 1+\n");
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
                    source: PendingTextSource::Raw,
                    bytes: b"[repl] guardrail".to_vec(),
                    terminated: false,
                },
                PendingOutputEvent::TextFragment {
                    seq: 1,
                    stream: TextStream::Stdout,
                    origin: ContentOrigin::Worker,
                    source: PendingTextSource::Raw,
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
    fn split_utf8_sequence_is_preserved_across_sideband_events() {
        let tape = PendingOutputTape::new();

        tape.append_stdout_bytes(&[0xC3]);
        tape.append_sideband(PendingSidebandKind::RequestBoundary);
        let first = tape.drain_snapshot();
        assert!(
            first.format_contents().contents.is_empty(),
            "incomplete utf-8 prefix should stay buffered across invisible sideband events"
        );

        tape.append_stdout_bytes(&[0xA9, b'\n']);
        let second = tape.drain_snapshot();
        assert_eq!(
            second.format_contents().contents,
            vec![WorkerContent::stdout("é\n")]
        );
    }

    #[test]
    fn split_utf8_sequence_is_preserved_across_final_snapshot_drains() {
        let tape = PendingOutputTape::new();

        tape.append_stdout_bytes(&[0xC3]);
        tape.append_sideband(PendingSidebandKind::RequestBoundary);
        let first = tape.drain_final_snapshot();
        assert!(
            first.format_contents().contents.is_empty(),
            "final request drains should keep incomplete utf-8 buffered for late bytes"
        );

        tape.append_stdout_bytes(&[0xA9, b'\n']);
        let second = tape.drain_snapshot();
        assert_eq!(
            second.format_contents().contents,
            vec![WorkerContent::stdout("é\n")]
        );
    }

    #[test]
    fn split_utf8_stdout_keeps_order_when_stderr_arrives_before_completion() {
        let tape = PendingOutputTape::new();

        tape.append_stdout_bytes(&[0xC3]);
        tape.append_stderr_bytes(b"boom\n");
        tape.append_stdout_bytes(&[0xA9, b'\n']);

        let snapshot = tape.drain_snapshot();
        assert_eq!(
            snapshot.format_contents().contents,
            vec![
                WorkerContent::stdout("é\n"),
                WorkerContent::stderr("stderr: boom\n"),
            ]
        );
    }

    #[test]
    fn split_utf8_prefix_survives_image_event_without_escape_corruption() {
        let tape = PendingOutputTape::new();

        tape.append_stdout_bytes(&[0xC3]);
        tape.append_image(
            "img-1".to_string(),
            "image/png".to_string(),
            "AA==".to_string(),
            true,
            1,
        );
        tape.append_stdout_bytes(&[0xA9, b'\n']);

        let snapshot = tape.drain_snapshot();
        let formatted = snapshot.format_contents();

        assert_eq!(
            formatted.contents,
            vec![
                WorkerContent::stdout("é\n"),
                WorkerContent::ContentImage {
                    data: "AA==".to_string(),
                    mime_type: "image/png".to_string(),
                    id: "img-1".to_string(),
                    is_new: true,
                },
            ]
        );
    }

    #[test]
    fn stderr_continues_partial_line_across_snapshot_drains() {
        let tape = PendingOutputTape::new();

        tape.append_stderr_bytes(b"abc");
        let first = tape.drain_snapshot();
        assert_eq!(
            first.format_contents().contents,
            vec![WorkerContent::stderr("stderr: abc")]
        );

        tape.append_stderr_bytes(b"def\n");
        let second = tape.drain_snapshot();
        assert_eq!(
            second.format_contents().contents,
            vec![WorkerContent::stderr("def\n")]
        );
    }

    #[test]
    fn server_stderr_notice_reprefixes_after_partial_worker_stderr() {
        let tape = PendingOutputTape::new();

        tape.append_stderr_bytes(b"partial");
        tape.append_server_stderr_bytes(b"[repl] session ended\n");

        let snapshot = tape.drain_snapshot();
        assert_eq!(
            snapshot.format_contents().contents,
            vec![
                WorkerContent::worker_stderr("stderr: partial"),
                WorkerContent::server_stderr("\nstderr: [repl] session ended\n"),
            ]
        );
    }

    #[test]
    fn sealed_snapshot_flushes_incomplete_utf8_as_hex_escape() {
        let tape = PendingOutputTape::new();
        tape.append_stdout_bytes(&[0xC3]);

        let snapshot = tape.drain_sealed_snapshot();
        assert_eq!(
            snapshot.format_contents().contents,
            vec![WorkerContent::stdout("\\xC3")]
        );
    }

    #[test]
    fn status_line_flushes_incomplete_utf8_tail_before_notice() {
        let tape = PendingOutputTape::new();
        tape.append_stdout_bytes(&[0xC3]);
        tape.append_stdout_status_line(b"[repl] session ended\n");

        let snapshot = tape.drain_final_snapshot();
        assert_eq!(
            snapshot.events,
            vec![
                PendingOutputEvent::TextFragment {
                    seq: 0,
                    stream: TextStream::Stdout,
                    origin: ContentOrigin::Worker,
                    source: PendingTextSource::Raw,
                    bytes: vec![0xC3],
                    terminated: false,
                },
                PendingOutputEvent::TextFragment {
                    seq: 1,
                    stream: TextStream::Stdout,
                    origin: ContentOrigin::Server,
                    source: PendingTextSource::Raw,
                    bytes: b"\n[repl] session ended\n".to_vec(),
                    terminated: true,
                },
            ]
        );
    }

    #[test]
    fn origin_change_flushes_incomplete_tail_before_appending_new_bytes() {
        let tape = PendingOutputTape::new();
        tape.append_server_stderr_bytes(&[0xC3]);
        tape.append_stderr_bytes(b"boom\n");

        let snapshot = tape.drain_snapshot();
        assert_eq!(
            snapshot.events,
            vec![
                PendingOutputEvent::TextFragment {
                    seq: 0,
                    stream: TextStream::Stderr,
                    origin: ContentOrigin::Server,
                    source: PendingTextSource::Raw,
                    bytes: vec![0xC3],
                    terminated: false,
                },
                PendingOutputEvent::TextFragment {
                    seq: 1,
                    stream: TextStream::Stderr,
                    origin: ContentOrigin::Worker,
                    source: PendingTextSource::Raw,
                    bytes: b"boom\n".to_vec(),
                    terminated: true,
                },
            ]
        );
    }
}
