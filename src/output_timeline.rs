use std::cell::Cell;
use std::collections::BTreeMap;

use crate::ipc::IpcEchoEvent;
use crate::output_capture::{OutputEventKind, OutputRange, OutputTextSpan};
use crate::worker_protocol::ContentOrigin;

pub(crate) struct CollapsedOutput {
    pub bytes: Vec<u8>,
    pub events: Vec<(u64, OutputEventKind)>,
    pub text_spans: Vec<OutputTextSpan>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EchoCollapseMode {
    Preserve,
    CollapseForFinalReply,
}

pub(crate) fn collapse_echo_with_attribution(
    range: OutputRange,
    echo_events: &[IpcEchoEvent],
    prompt_variants: &[String],
    mode: EchoCollapseMode,
) -> CollapsedOutput {
    const ECHO_MARKER_MIN_BYTES: usize = 512;

    let mut out_bytes: Vec<u8> = Vec::new();
    let mut out_events: Vec<(u64, OutputEventKind)> = Vec::new();
    let mut out_text_spans: Vec<OutputTextSpan> = Vec::new();

    let prompt_variants = prompt_variants_bytes(prompt_variants);
    let mut pending = PendingEchoRun::default();
    let mut echo_idx = 0usize;
    let saw_substantive_output = Cell::new(false);

    let base_offset = range.start_offset;
    let end_offset = range.end_offset;
    let bytes = range.bytes;
    let text_spans = range.text_spans;

    let mut anchored_images: BTreeMap<usize, Vec<OutputEventKind>> = BTreeMap::new();

    let mut events: Vec<(usize, OutputEventKind)> = range
        .events
        .into_iter()
        .filter_map(|event| {
            if event.offset < base_offset || event.offset > end_offset {
                return None;
            }
            let rel = event.offset.saturating_sub(base_offset) as usize;
            match event.kind {
                OutputEventKind::Image {
                    readline_results_seen,
                    ..
                } if readline_results_seen < echo_events.len() => {
                    anchored_images
                        .entry(readline_results_seen)
                        .or_default()
                        .push(event.kind);
                    None
                }
                kind => Some((rel.min(bytes.len()), kind)),
            }
        })
        .collect();
    events.sort_by_key(|(offset, _)| *offset);

    let flush_anchored_images =
        |anchor_idx: usize,
         anchored_images: &mut BTreeMap<usize, Vec<OutputEventKind>>,
         out_bytes: &Vec<u8>,
         out_events: &mut Vec<(u64, OutputEventKind)>| {
            if let Some(images) = anchored_images.remove(&anchor_idx) {
                for image in images {
                    out_events.push((out_bytes.len() as u64, image));
                }
            }
        };

    let mut flush_pending = |out_bytes: &mut Vec<u8>,
                             out_text_spans: &mut Vec<OutputTextSpan>,
                             pending: &mut PendingEchoRun| {
        if pending.is_empty() {
            return;
        }
        let pending = pending.take();
        if !saw_substantive_output.get() {
            return;
        }
        let head = pending.head.as_deref().unwrap_or_default();
        let tail = pending.tail.as_deref().unwrap_or_default();
        if pending.lines >= 2 || pending.bytes >= ECHO_MARKER_MIN_BYTES {
            let head_snip = summarize_echo_line_for_marker(head);
            let tail_snip = summarize_echo_line_for_marker(tail);
            let marker = format!(
                "[repl] echoed input elided: {} lines ({} bytes); head: {}; tail: {}\n",
                pending.lines, pending.bytes, head_snip, tail_snip
            );
            append_text_with_span(
                out_bytes,
                out_text_spans,
                marker.as_bytes(),
                false,
                ContentOrigin::Worker,
            );
        } else {
            append_text_with_span(
                out_bytes,
                out_text_spans,
                &summarize_echo_line_for_output(tail),
                false,
                ContentOrigin::Worker,
            );
        }
    };

    let mut cursor = 0usize;
    for (event_offset, kind) in events {
        let event_offset = event_offset.min(bytes.len());
        if event_offset > cursor {
            consume_text_segment_with_spans(
                &bytes[cursor..event_offset],
                cursor,
                &text_spans,
                echo_events,
                &mut echo_idx,
                &prompt_variants,
                &mut pending,
                &saw_substantive_output,
                mode,
                &mut flush_pending,
                &mut anchored_images,
                &mut out_bytes,
                &mut out_text_spans,
                &mut out_events,
                &flush_anchored_images,
            );
            cursor = event_offset;
        }

        if matches!(kind, OutputEventKind::Text { .. }) {
            flush_pending(&mut out_bytes, &mut out_text_spans, &mut pending);
        }
        out_events.push((out_bytes.len() as u64, kind));
    }

    if cursor < bytes.len() {
        consume_text_segment_with_spans(
            &bytes[cursor..],
            cursor,
            &text_spans,
            echo_events,
            &mut echo_idx,
            &prompt_variants,
            &mut pending,
            &saw_substantive_output,
            mode,
            &mut flush_pending,
            &mut anchored_images,
            &mut out_bytes,
            &mut out_text_spans,
            &mut out_events,
            &flush_anchored_images,
        );
    }

    while let Some((_, images)) = anchored_images.pop_first() {
        for image in images {
            out_events.push((out_bytes.len() as u64, image));
        }
    }

    CollapsedOutput {
        bytes: out_bytes,
        events: out_events,
        text_spans: out_text_spans,
    }
}

#[derive(Default)]
struct PendingEchoRun {
    lines: usize,
    bytes: usize,
    head: Option<Vec<u8>>,
    tail: Option<Vec<u8>>,
}

impl PendingEchoRun {
    fn push(&mut self, line: &[u8]) {
        self.lines = self.lines.saturating_add(1);
        self.bytes = self.bytes.saturating_add(line.len());
        if self.head.is_none() {
            self.head = Some(line.to_vec());
        }
        self.tail = Some(line.to_vec());
    }

    fn take(&mut self) -> PendingEchoRun {
        std::mem::take(self)
    }

    fn is_empty(&self) -> bool {
        self.lines == 0
    }
}

fn strip_trailing_newlines_bytes(bytes: &[u8]) -> &[u8] {
    let mut end = bytes.len();
    while end > 0 && matches!(bytes[end - 1], b'\n' | b'\r') {
        end -= 1;
    }
    &bytes[..end]
}

fn is_ascii_whitespace_only(bytes: &[u8]) -> bool {
    bytes.iter().all(|b| b.is_ascii_whitespace())
}

fn prompt_variants_bytes(prompt_variants: &[String]) -> Vec<Vec<u8>> {
    prompt_variants
        .iter()
        .filter_map(|prompt| {
            let trimmed = prompt.trim_end_matches(['\n', '\r']);
            (!trimmed.is_empty()).then_some(trimmed.as_bytes().to_vec())
        })
        .collect()
}

fn is_prompt_only_fragment(bytes: &[u8], prompt_variants: &[Vec<u8>]) -> bool {
    let trimmed = strip_trailing_newlines_bytes(bytes);
    if trimmed.is_empty() {
        return false;
    }
    prompt_variants.iter().any(|p| p.as_slice() == trimmed)
}

fn summarize_middle(text: &str, head_chars: usize, tail_chars: usize) -> String {
    let total = text.chars().count();
    if total <= head_chars.saturating_add(tail_chars).saturating_add(8) {
        return text.to_string();
    }
    let head = text.chars().take(head_chars).collect::<String>();
    let tail = text
        .chars()
        .rev()
        .take(tail_chars)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    format!("{head} .... [ELIDED] .... {tail}")
}

fn summarize_echo_line_for_marker(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(strip_trailing_newlines_bytes(bytes));
    summarize_middle(&text, 80, 40)
}

fn summarize_echo_line_for_output(bytes: &[u8]) -> Vec<u8> {
    const MAX_CHARS: usize = 220;
    let had_newline = bytes.ends_with(b"\n");
    let text = String::from_utf8_lossy(strip_trailing_newlines_bytes(bytes));
    let summarized = if text.chars().count() > MAX_CHARS {
        summarize_middle(&text, 120, 60)
    } else {
        text.to_string()
    };
    let mut out = summarized.into_bytes();
    if had_newline {
        out.push(b'\n');
    }
    out
}

fn echo_event_prefix_len(line: &[u8], event: &IpcEchoEvent) -> Option<usize> {
    let prompt = event.prompt.as_bytes();
    let consumed = event.line.as_bytes();
    if line.len() == prompt.len().saturating_add(consumed.len()) {
        let (prefix, suffix) = line.split_at(prompt.len());
        if prefix == prompt && suffix == consumed {
            return Some(line.len());
        }
    }

    let consumed = if let Some(consumed) = consumed.strip_suffix(b"\r\n") {
        consumed
    } else if let Some(consumed) = consumed.strip_suffix(b"\n") {
        consumed
    } else {
        return None;
    };
    let prefix_len = prompt.len().saturating_add(consumed.len());
    if line.len() <= prefix_len {
        return None;
    }
    let (prefix, suffix) = line.split_at(prompt.len());
    if prefix != prompt || !suffix.starts_with(consumed) {
        return None;
    }
    Some(prefix_len)
}

fn append_text_with_span(
    out_bytes: &mut Vec<u8>,
    out_text_spans: &mut Vec<OutputTextSpan>,
    bytes: &[u8],
    is_stderr: bool,
    origin: ContentOrigin,
) {
    if bytes.is_empty() {
        return;
    }
    let start_byte = out_bytes.len();
    out_bytes.extend_from_slice(bytes);
    let end_byte = out_bytes.len();
    if let Some(last) = out_text_spans.last_mut()
        && last.is_stderr == is_stderr
        && last.origin == origin
        && last.end_byte == start_byte
    {
        last.end_byte = end_byte;
    } else {
        out_text_spans.push(OutputTextSpan {
            start_byte,
            end_byte,
            is_stderr,
            origin,
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn consume_text_segment_with_spans(
    segment: &[u8],
    segment_start: usize,
    text_spans: &[OutputTextSpan],
    echo_events: &[IpcEchoEvent],
    echo_idx: &mut usize,
    prompt_variants: &[Vec<u8>],
    pending: &mut PendingEchoRun,
    saw_substantive_output: &Cell<bool>,
    mode: EchoCollapseMode,
    flush_pending: &mut impl FnMut(&mut Vec<u8>, &mut Vec<OutputTextSpan>, &mut PendingEchoRun),
    anchored_images: &mut BTreeMap<usize, Vec<OutputEventKind>>,
    out_bytes: &mut Vec<u8>,
    out_text_spans: &mut Vec<OutputTextSpan>,
    out_events: &mut Vec<(u64, OutputEventKind)>,
    flush_anchored_images: &impl Fn(
        usize,
        &mut BTreeMap<usize, Vec<OutputEventKind>>,
        &Vec<u8>,
        &mut Vec<(u64, OutputEventKind)>,
    ),
) {
    let segment_end = segment_start.saturating_add(segment.len());
    let mut cursor = segment_start;
    for span in text_spans {
        if span.end_byte <= segment_start {
            continue;
        }
        if span.start_byte >= segment_end {
            break;
        }
        let start = span.start_byte.max(segment_start);
        let end = span.end_byte.min(segment_end);
        if cursor < start {
            consume_text_segment(
                &segment[cursor - segment_start..start - segment_start],
                false,
                ContentOrigin::Worker,
                echo_events,
                echo_idx,
                prompt_variants,
                pending,
                saw_substantive_output,
                mode,
                flush_pending,
                anchored_images,
                out_bytes,
                out_text_spans,
                out_events,
                flush_anchored_images,
            );
        }
        consume_text_segment(
            &segment[start - segment_start..end - segment_start],
            span.is_stderr,
            span.origin,
            echo_events,
            echo_idx,
            prompt_variants,
            pending,
            saw_substantive_output,
            mode,
            flush_pending,
            anchored_images,
            out_bytes,
            out_text_spans,
            out_events,
            flush_anchored_images,
        );
        cursor = end;
    }
    if cursor < segment_end {
        consume_text_segment(
            &segment[cursor - segment_start..],
            false,
            ContentOrigin::Worker,
            echo_events,
            echo_idx,
            prompt_variants,
            pending,
            saw_substantive_output,
            mode,
            flush_pending,
            anchored_images,
            out_bytes,
            out_text_spans,
            out_events,
            flush_anchored_images,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn consume_text_segment(
    segment: &[u8],
    is_stderr: bool,
    origin: ContentOrigin,
    echo_events: &[IpcEchoEvent],
    echo_idx: &mut usize,
    prompt_variants: &[Vec<u8>],
    pending: &mut PendingEchoRun,
    saw_substantive_output: &Cell<bool>,
    mode: EchoCollapseMode,
    flush_pending: &mut impl FnMut(&mut Vec<u8>, &mut Vec<OutputTextSpan>, &mut PendingEchoRun),
    anchored_images: &mut BTreeMap<usize, Vec<OutputEventKind>>,
    out_bytes: &mut Vec<u8>,
    out_text_spans: &mut Vec<OutputTextSpan>,
    out_events: &mut Vec<(u64, OutputEventKind)>,
    flush_anchored_images: &impl Fn(
        usize,
        &mut BTreeMap<usize, Vec<OutputEventKind>>,
        &Vec<u8>,
        &mut Vec<(u64, OutputEventKind)>,
    ),
) {
    let mut start = 0usize;
    while start < segment.len() {
        let mut end = start;
        while end < segment.len() && segment[end] != b'\n' {
            end += 1;
        }
        if end < segment.len() && segment[end] == b'\n' {
            end += 1;
        }
        let line = &segment[start..end];
        start = end;

        let echo_prefix = if *echo_idx < echo_events.len() {
            echo_event_prefix_len(line, &echo_events[*echo_idx])
        } else {
            None
        };
        if let Some(prefix_len) = echo_prefix {
            flush_anchored_images(*echo_idx, anchored_images, out_bytes, out_events);
            match mode {
                EchoCollapseMode::Preserve => append_text_with_span(
                    out_bytes,
                    out_text_spans,
                    &line[..prefix_len],
                    is_stderr,
                    origin,
                ),
                EchoCollapseMode::CollapseForFinalReply => pending.push(&line[..prefix_len]),
            }
            *echo_idx = echo_idx.saturating_add(1);
            if prefix_len == line.len() {
                continue;
            }
        }

        let line = if let Some(prefix_len) = echo_prefix {
            &line[prefix_len..]
        } else {
            line
        };

        let substantive =
            !is_ascii_whitespace_only(line) && !is_prompt_only_fragment(line, prompt_variants);
        if substantive {
            flush_pending(out_bytes, out_text_spans, pending);
        }
        append_text_with_span(out_bytes, out_text_spans, line, is_stderr, origin);
        if substantive {
            saw_substantive_output.set(true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output_capture::{OutputEvent, OutputRange};
    use crate::pager;
    use crate::worker_protocol::WorkerContent;

    fn echo_event(prompt: &str, line: &str) -> IpcEchoEvent {
        IpcEchoEvent {
            prompt: prompt.to_string(),
            line: line.to_string(),
        }
    }

    #[test]
    fn anchored_image_moves_before_later_echoed_line_output() {
        let bytes = b"> plot(1:10)\n> cat('done\\n')\ndone\n".to_vec();
        let range = OutputRange {
            start_offset: 0,
            end_offset: bytes.len() as u64,
            bytes,
            events: vec![OutputEvent {
                offset: 34,
                kind: OutputEventKind::Image {
                    data: "img".to_string(),
                    mime_type: "image/png".to_string(),
                    id: "plot-1".to_string(),
                    is_new: true,
                    readline_results_seen: 1,
                },
            }],
            text_spans: vec![OutputTextSpan {
                start_byte: 0,
                end_byte: 34,
                is_stderr: false,
                origin: ContentOrigin::Worker,
            }],
        };

        let collapsed = collapse_echo_with_attribution(
            range,
            &[
                echo_event("> ", "plot(1:10)\n"),
                echo_event("> ", "cat('done\\n')\n"),
            ],
            &["> ".to_string()],
            EchoCollapseMode::CollapseForFinalReply,
        );
        let contents = pager::contents_from_collapsed_output(
            collapsed.bytes,
            collapsed.events,
            collapsed.text_spans,
            34,
        );

        assert_eq!(
            contents,
            vec![
                WorkerContent::ContentImage {
                    data: "img".to_string(),
                    mime_type: "image/png".to_string(),
                    id: "plot-1".to_string(),
                    is_new: true,
                },
                WorkerContent::stdout("done\n"),
            ]
        );
    }

    #[test]
    fn preserve_mode_keeps_echo_while_anchoring_image_before_later_input() {
        let bytes = b"> plot(1:10)\n> cat('done\\n')\ndone\n".to_vec();
        let range = OutputRange {
            start_offset: 0,
            end_offset: bytes.len() as u64,
            bytes,
            events: vec![OutputEvent {
                offset: 34,
                kind: OutputEventKind::Image {
                    data: "img".to_string(),
                    mime_type: "image/png".to_string(),
                    id: "plot-1".to_string(),
                    is_new: true,
                    readline_results_seen: 1,
                },
            }],
            text_spans: vec![OutputTextSpan {
                start_byte: 0,
                end_byte: 34,
                is_stderr: false,
                origin: ContentOrigin::Worker,
            }],
        };

        let collapsed = collapse_echo_with_attribution(
            range,
            &[
                echo_event("> ", "plot(1:10)\n"),
                echo_event("> ", "cat('done\\n')\n"),
            ],
            &["> ".to_string()],
            EchoCollapseMode::Preserve,
        );
        let contents = pager::contents_from_collapsed_output(
            collapsed.bytes,
            collapsed.events,
            collapsed.text_spans,
            34,
        );

        assert_eq!(
            contents,
            vec![
                WorkerContent::stdout("> plot(1:10)\n"),
                WorkerContent::ContentImage {
                    data: "img".to_string(),
                    mime_type: "image/png".to_string(),
                    id: "plot-1".to_string(),
                    is_new: true,
                },
                WorkerContent::stdout("> cat('done\\n')\ndone\n"),
            ]
        );
    }
}
