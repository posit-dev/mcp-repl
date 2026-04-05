use std::fmt::Write as _;

use crate::output_capture::OutputEventKind;
use crate::worker_protocol::{ContentOrigin, TextStream, WorkerContent};

pub(crate) trait EventView {
    fn offset(&self) -> u64;
    fn kind(&self) -> &OutputEventKind;
}

pub(crate) trait TextSpanView {
    fn start(&self) -> u64;
    fn end(&self) -> u64;
    fn is_stderr(&self) -> bool;
    fn origin(&self) -> ContentOrigin;
}

fn push_text(
    contents: &mut Vec<WorkerContent>,
    text: String,
    stream: TextStream,
    origin: ContentOrigin,
) {
    if text.is_empty() {
        return;
    }
    if let Some(WorkerContent::ContentText {
        text: last_text,
        stream: last_stream,
        origin: last_origin,
    }) = contents.last_mut()
        && *last_stream == stream
        && *last_origin == origin
    {
        last_text.push_str(&text);
        return;
    }
    contents.push(WorkerContent::ContentText {
        text,
        stream,
        origin,
    });
}

fn push_default_stdout(contents: &mut Vec<WorkerContent>, bytes: &[u8], start: usize, end: usize) {
    if start >= end || end > bytes.len() {
        return;
    }
    let text = render_bytes(&bytes[start..end]);
    push_text(contents, text, TextStream::Stdout, ContentOrigin::Worker);
}

fn push_span_text(
    contents: &mut Vec<WorkerContent>,
    bytes: &[u8],
    start: usize,
    end: usize,
    span: &impl TextSpanView,
) {
    if start >= end || end > bytes.len() {
        return;
    }
    let text = render_bytes(&bytes[start..end]);
    push_text(
        contents,
        text,
        if span.is_stderr() {
            TextStream::Stderr
        } else {
            TextStream::Stdout
        },
        span.origin(),
    );
}

fn emit_text_range<S: TextSpanView>(
    contents: &mut Vec<WorkerContent>,
    bytes: &[u8],
    base_offset: u64,
    start_offset: u64,
    end_offset: u64,
    spans: &[S],
) {
    let end_offset = end_offset.min(base_offset.saturating_add(bytes.len() as u64));
    let start_offset = start_offset.min(end_offset);
    if start_offset >= end_offset {
        return;
    }

    let mut cursor = start_offset;
    for span in spans {
        if span.end() <= start_offset {
            continue;
        }
        if span.start() >= end_offset {
            break;
        }

        let span_start = span.start().max(start_offset);
        if cursor < span_start {
            push_default_stdout(
                contents,
                bytes,
                cursor.saturating_sub(base_offset) as usize,
                span_start.saturating_sub(base_offset) as usize,
            );
        }

        let span_end = span.end().min(end_offset);
        push_span_text(
            contents,
            bytes,
            span_start.saturating_sub(base_offset) as usize,
            span_end.saturating_sub(base_offset) as usize,
            span,
        );
        cursor = span_end;
    }

    if cursor < end_offset {
        push_default_stdout(
            contents,
            bytes,
            cursor.saturating_sub(base_offset) as usize,
            end_offset.saturating_sub(base_offset) as usize,
        );
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

pub(crate) fn merge_bytes_with_events_and_spans<E: EventView, S: TextSpanView>(
    bytes: &[u8],
    base_offset: u64,
    end_offset: u64,
    spans: &[S],
    events: &[E],
    event_to_content: impl Fn(&OutputEventKind) -> WorkerContent,
) -> Vec<WorkerContent> {
    let mut contents = Vec::new();
    let mut cursor = base_offset;
    for event in events
        .iter()
        .filter(|event| event.offset() >= base_offset && event.offset() <= end_offset)
    {
        if event.offset() > base_offset.saturating_add(bytes.len() as u64) {
            break;
        }
        if event.offset() > cursor {
            emit_text_range(
                &mut contents,
                bytes,
                base_offset,
                cursor,
                event.offset(),
                spans,
            );
        }
        contents.push(event_to_content(event.kind()));
        cursor = event.offset();
    }
    if cursor < end_offset {
        emit_text_range(&mut contents, bytes, base_offset, cursor, end_offset, spans);
    }
    contents
}
