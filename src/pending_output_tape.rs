use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use crate::ipc::IpcEchoEvent;
use crate::output_capture::{OutputEvent, OutputEventKind, OutputRange, OutputTextSpan};
use crate::output_timeline::collapse_echo_with_attribution;
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
    pending_echo_prefix: String,
    last_rendered_text: Option<RenderedTextState>,
}

#[derive(Default)]
struct PendingTextTail {
    bytes: Vec<u8>,
    origin: Option<ContentOrigin>,
    start_seq: Option<u64>,
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
        readline_results_seen: usize,
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
            | Self::Sideband { seq, .. } => *seq,
        }
    }
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
    leading_echo_prefix: Option<String>,
    prior_rendered_text: Option<RenderedTextState>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct FormattedPendingOutput {
    pub contents: Vec<WorkerContent>,
    pub saw_stderr: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RenderedTextState {
    stream: TextStream,
    origin: ContentOrigin,
    terminated: bool,
}

struct RenderedPendingOutput {
    range: OutputRange,
    echo_events: Vec<IpcEchoEvent>,
    prompt_variants: Vec<String>,
    saw_stderr: bool,
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
            &status_line,
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
        append_readline_results_to_echo_prefix(&mut guard.pending_echo_prefix, &events);
        let leading_echo_prefix =
            (!guard.pending_echo_prefix.is_empty()).then(|| guard.pending_echo_prefix.clone());
        if let Some(echo_prefix) = leading_echo_prefix.as_deref() {
            let (matched_bytes, keep_remaining_suffix) =
                leading_echo_match_progress(&events, echo_prefix);
            if keep_remaining_suffix
                && !(snapshot_has_no_visible_text(&events)
                    && snapshot_crossed_request_boundary(&events))
            {
                guard.pending_echo_prefix = echo_prefix[matched_bytes..].to_string();
            } else {
                guard.pending_echo_prefix.clear();
            }
        }
        guard.last_rendered_text = rendered_text_state_after(events.iter(), prior_rendered_text);
        PendingOutputSnapshot {
            events,
            leading_echo_prefix,
            prior_rendered_text,
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
        tail.bytes.extend_from_slice(bytes);
        commit_complete_lines(&mut guard, stream);
    }
}

impl PendingOutputSnapshot {
    #[cfg(test)]
    pub(crate) fn format_contents(&self) -> FormattedPendingOutput {
        let mut formatted = FormattedPendingOutput::default();
        let mut last_rendered_text = self.prior_rendered_text;
        for event in &self.events {
            match event {
                PendingOutputEvent::TextFragment {
                    stream,
                    origin,
                    bytes,
                    terminated,
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
                        render_stderr_text(last_rendered_text, *origin, rendered)
                    } else {
                        rendered
                    };
                    push_text(&mut formatted.contents, *stream, *origin, text);
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
                    formatted.contents.push(WorkerContent::ContentImage {
                        data: data.clone(),
                        mime_type: mime_type.clone(),
                        id: id.clone(),
                        is_new: *is_new,
                    });
                    last_rendered_text = None;
                }
                PendingOutputEvent::Sideband { .. } => {}
            }
        }
        maybe_trim_leading_echo_prefix(
            self.leading_echo_prefix.as_deref(),
            &mut formatted.contents,
        );
        formatted
    }

    pub(crate) fn format_contents_for_reply(&self) -> FormattedPendingOutput {
        let RenderedPendingOutput {
            range,
            echo_events,
            prompt_variants,
            saw_stderr,
        } = self.rendered_output_for_reply();
        let source_end = range.end_offset;
        let collapsed = collapse_echo_with_attribution(range, &echo_events, &prompt_variants);
        let mut contents = pager::contents_from_collapsed_output(
            collapsed.bytes,
            collapsed.events,
            collapsed.text_spans,
            source_end,
        );
        maybe_trim_leading_echo_prefix(self.leading_echo_prefix.as_deref(), &mut contents);
        FormattedPendingOutput {
            contents,
            saw_stderr,
        }
    }

    fn rendered_output_for_reply(&self) -> RenderedPendingOutput {
        let mut bytes = Vec::new();
        let mut text_spans = Vec::new();
        let mut events = Vec::new();
        let mut echo_events = Vec::new();
        let mut prompt_variants = Vec::new();
        let mut saw_stderr = false;
        let mut last_rendered_text = self.prior_rendered_text;

        for event in &self.events {
            match event {
                PendingOutputEvent::TextFragment {
                    stream,
                    origin,
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
                    readline_results_seen,
                    ..
                } => {
                    events.push(OutputEvent {
                        offset: bytes.len() as u64,
                        kind: OutputEventKind::Image {
                            data: data.clone(),
                            mime_type: mime_type.clone(),
                            id: id.clone(),
                            is_new: *is_new,
                            readline_results_seen: *readline_results_seen,
                        },
                    });
                    last_rendered_text = None;
                }
                PendingOutputEvent::Sideband { kind, .. } => match kind {
                    PendingSidebandKind::ReadlineStart { prompt } => {
                        push_prompt_variant(&mut prompt_variants, prompt);
                    }
                    PendingSidebandKind::ReadlineResult { prompt, line } => {
                        push_prompt_variant(&mut prompt_variants, prompt);
                        echo_events.push(IpcEchoEvent {
                            prompt: prompt.clone(),
                            line: line.clone(),
                        });
                    }
                    PendingSidebandKind::RequestEnd | PendingSidebandKind::SessionEnd => {}
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
            echo_events,
            prompt_variants,
            saw_stderr,
        }
    }
}

fn maybe_trim_leading_echo_prefix(echo_prefix: Option<&str>, contents: &mut Vec<WorkerContent>) {
    let Some(echo_prefix) = echo_prefix else {
        return;
    };
    trim_matching_echo_prefix_from_contents(contents, echo_prefix);
}

fn append_readline_results_to_echo_prefix(echo_prefix: &mut String, events: &[PendingOutputEvent]) {
    for event in events {
        if let PendingOutputEvent::Sideband {
            kind: PendingSidebandKind::ReadlineResult { prompt, line },
            ..
        } = event
            && is_trim_eligible_readline_prompt(prompt)
        {
            echo_prefix.push_str(prompt);
            echo_prefix.push_str(line);
        }
    }
}

fn snapshot_has_no_visible_text(events: &[PendingOutputEvent]) -> bool {
    events
        .iter()
        .all(|event| !matches!(event, PendingOutputEvent::TextFragment { bytes, .. } if !render_bytes(bytes).is_empty()))
}

fn snapshot_crossed_request_boundary(events: &[PendingOutputEvent]) -> bool {
    events.iter().any(|event| {
        matches!(
            event,
            PendingOutputEvent::Sideband {
                kind: PendingSidebandKind::RequestEnd | PendingSidebandKind::SessionEnd,
                ..
            }
        )
    })
}

fn is_trim_eligible_readline_prompt(prompt: &str) -> bool {
    matches!(
        prompt.trim_end_matches(|ch: char| ch.is_whitespace()),
        ">" | "+" | ">>>" | "..."
    )
}

fn leading_echo_match_progress(events: &[PendingOutputEvent], echo_prefix: &str) -> (usize, bool) {
    if echo_prefix.is_empty() {
        return (0, false);
    }

    let mut remaining = echo_prefix;
    let mut matched_bytes = 0usize;
    let mut saw_visible_content = false;

    for event in events {
        let PendingOutputEvent::TextFragment {
            stream,
            origin,
            bytes,
            ..
        } = event
        else {
            if matches!(event, PendingOutputEvent::Sideband { .. }) {
                continue;
            }
            return (matched_bytes, false);
        };

        if !matches!(stream, TextStream::Stdout) || !matches!(origin, ContentOrigin::Worker) {
            return (matched_bytes, false);
        }

        let rendered = render_bytes(bytes);
        if rendered.is_empty() {
            continue;
        }

        saw_visible_content = true;
        if remaining.is_empty() {
            return (matched_bytes, false);
        }

        let common = common_prefix_len(remaining, &rendered);
        matched_bytes = matched_bytes.saturating_add(common);
        remaining = &remaining[common..];

        if common < rendered.len() {
            return (matched_bytes, false);
        }
    }

    if !saw_visible_content {
        return (matched_bytes, true);
    }

    (matched_bytes, !remaining.is_empty())
}

fn trim_matching_echo_prefix_from_contents(contents: &mut Vec<WorkerContent>, echo_prefix: &str) {
    if echo_prefix.is_empty() {
        return;
    }

    let mut remaining = echo_prefix;
    let mut matched_bytes = 0usize;
    for content in contents.iter() {
        let WorkerContent::ContentText {
            text,
            stream,
            origin,
        } = content
        else {
            break;
        };
        if !matches!(stream, TextStream::Stdout) || !matches!(origin, ContentOrigin::Worker) {
            break;
        }
        let common = common_prefix_len(remaining, text);
        matched_bytes = matched_bytes.saturating_add(common);
        remaining = &remaining[common..];
        if common < text.len() || remaining.is_empty() {
            break;
        }
    }

    if matched_bytes == 0 {
        return;
    }

    let mut remaining = &echo_prefix[..matched_bytes];
    let mut idx = 0usize;
    while idx < contents.len() && !remaining.is_empty() {
        let remove_current = match &mut contents[idx] {
            WorkerContent::ContentText { text, .. } => {
                if remaining.len() >= text.len() {
                    remaining = &remaining[text.len()..];
                    text.clear();
                    true
                } else {
                    let updated = text[remaining.len()..].to_string();
                    *text = updated;
                    remaining = "";
                    false
                }
            }
            _ => return,
        };

        if remove_current {
            contents.remove(idx);
            continue;
        }
        idx = idx.saturating_add(1);
    }
}

fn common_prefix_len(left: &str, right: &str) -> usize {
    let mut matched = 0usize;
    for (lch, rch) in left.chars().zip(right.chars()) {
        if lch != rch {
            break;
        }
        matched = matched.saturating_add(lch.len_utf8());
    }
    matched
}

#[cfg(test)]
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

fn append_rendered_text(
    bytes: &mut Vec<u8>,
    text_spans: &mut Vec<OutputTextSpan>,
    text: &[u8],
    stream: TextStream,
    origin: ContentOrigin,
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
        && last.end_byte == start_byte
    {
        last.end_byte = end_byte;
    } else {
        text_spans.push(OutputTextSpan {
            start_byte,
            end_byte,
            is_stderr,
            origin,
        });
    }
}

fn push_prompt_variant(prompt_variants: &mut Vec<String>, prompt: &str) {
    if prompt_variants.iter().any(|existing| existing == prompt) {
        return;
    }
    prompt_variants.push(prompt.to_string());
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
            bytes: bytes.to_vec(),
            terminated: bytes.ends_with(b"\n"),
        },
    );
}

fn commit_complete_lines(inner: &mut PendingOutputTapeInner, stream: TextStream) {
    loop {
        let (seq, origin, line, tail_empty) = {
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
            let line = tail.bytes.drain(..=newline_idx).collect::<Vec<u8>>();
            let tail_empty = tail.bytes.is_empty();
            if tail_empty {
                tail.origin = None;
                tail.start_seq = None;
            }
            (seq, origin, line, tail_empty)
        };
        append_event(
            inner,
            PendingOutputEvent::TextFragment {
                seq,
                stream,
                origin,
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
    let (seq, origin, bytes, tail_empty) = {
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
        let bytes = tail.bytes.drain(..flush_len).collect::<Vec<u8>>();
        let tail_empty = tail.bytes.is_empty();
        if tail_empty {
            tail.origin = None;
            tail.start_seq = None;
        }
        (seq, origin, bytes, tail_empty)
    };
    append_event(
        inner,
        PendingOutputEvent::TextFragment {
            seq,
            stream,
            origin,
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
        Some(PendingOutputEvent::Image { .. } | PendingOutputEvent::Sideband { .. }) | None => None,
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
            PendingOutputEvent::Image { .. } => state = None,
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
    fn split_utf8_sequence_is_preserved_across_sideband_events() {
        let tape = PendingOutputTape::new();

        tape.append_stdout_bytes(&[0xC3]);
        tape.append_sideband(PendingSidebandKind::RequestEnd);
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
        tape.append_sideband(PendingSidebandKind::RequestEnd);
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
    fn readline_result_prefix_carries_across_snapshot_drains_until_echo_arrives() {
        let tape = PendingOutputTape::new();

        tape.append_sideband(PendingSidebandKind::ReadlineResult {
            prompt: "> ".to_string(),
            line: "1+\n".to_string(),
        });
        let first = tape.drain_snapshot();
        assert!(
            first.format_contents().contents.is_empty(),
            "sideband-only snapshot should not render visible content"
        );

        tape.append_stdout_bytes(b"> 1");
        let second = tape.drain_snapshot();
        assert!(
            second.format_contents().contents.is_empty(),
            "partial echoed prefix should stay hidden until the remainder arrives"
        );

        tape.append_stdout_bytes(b"+\n[1] 2\n");
        let third = tape.drain_snapshot();
        assert_eq!(
            third.format_contents().contents,
            vec![WorkerContent::stdout("[1] 2\n")]
        );
    }

    #[test]
    fn request_end_clears_pending_echo_prefix_after_sideband_only_snapshot() {
        let tape = PendingOutputTape::new();

        tape.append_sideband(PendingSidebandKind::ReadlineResult {
            prompt: "> ".to_string(),
            line: "x <- 1\n".to_string(),
        });
        tape.append_sideband(PendingSidebandKind::RequestEnd);

        let first = tape.drain_snapshot();
        assert!(
            first.format_contents().contents.is_empty(),
            "sideband-only snapshot should not render visible content"
        );

        let guard = tape
            .inner
            .lock()
            .expect("pending output tape mutex poisoned");
        assert!(
            guard.pending_echo_prefix.is_empty(),
            "request boundary should clear unmatched carried echo"
        );
    }

    #[test]
    fn interleaved_output_drops_unmatched_echo_suffix_from_later_drains() {
        let tape = PendingOutputTape::new();

        tape.append_sideband(PendingSidebandKind::ReadlineResult {
            prompt: "> ".to_string(),
            line: "x <- 1\n".to_string(),
        });
        tape.append_sideband(PendingSidebandKind::ReadlineResult {
            prompt: "> ".to_string(),
            line: "y <- 2\n".to_string(),
        });
        let first = tape.drain_snapshot();
        assert!(
            first.format_contents().contents.is_empty(),
            "sideband-only snapshot should not render visible content"
        );

        tape.append_stdout_bytes(b"> x <- 1\nok\n");
        let second = tape.drain_snapshot();
        assert_eq!(
            second.format_contents().contents,
            vec![WorkerContent::stdout("ok\n")]
        );

        tape.append_stdout_bytes(b"> y <- 2\n");
        let third = tape.drain_snapshot();
        assert_eq!(
            third.format_contents().contents,
            vec![WorkerContent::stdout("> y <- 2\n")]
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
                    bytes: vec![0xC3],
                    terminated: false,
                },
                PendingOutputEvent::TextFragment {
                    seq: 1,
                    stream: TextStream::Stdout,
                    origin: ContentOrigin::Server,
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
                    bytes: vec![0xC3],
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
    fn reply_format_anchors_image_before_later_echoed_input_and_stdout() {
        let tape = PendingOutputTape::new();

        tape.append_stdout_bytes(b"> plot(1:10)\n");
        tape.append_sideband(PendingSidebandKind::ReadlineResult {
            prompt: "> ".to_string(),
            line: "plot(1:10)\n".to_string(),
        });
        tape.append_stdout_bytes(b"> cat('done\\n')\n");
        tape.append_stdout_bytes(b"done\n");
        tape.append_image(
            "img-1".to_string(),
            "image/png".to_string(),
            "AA==".to_string(),
            true,
            1,
        );
        tape.append_sideband(PendingSidebandKind::ReadlineResult {
            prompt: "> ".to_string(),
            line: "cat('done\\n')\n".to_string(),
        });

        let snapshot = tape.drain_snapshot();
        assert_eq!(
            snapshot.format_contents_for_reply().contents,
            vec![
                WorkerContent::ContentImage {
                    data: "AA==".to_string(),
                    mime_type: "image/png".to_string(),
                    id: "img-1".to_string(),
                    is_new: true,
                },
                WorkerContent::stdout("done\n"),
            ]
        );
    }
}
