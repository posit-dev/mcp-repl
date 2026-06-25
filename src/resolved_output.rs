use std::fmt::Write as _;

use crate::reply_presentation::input_echo_text;
use crate::worker_protocol::{ContentOrigin, ContentVisibility, TextStream, WorkerContent};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum OutputTextSource {
    #[default]
    Raw,
    Ipc,
}

#[derive(Clone, Debug)]
pub(crate) struct OutputTextSpan {
    pub start_byte: usize,
    pub end_byte: usize,
    pub is_stderr: bool,
    pub origin: ContentOrigin,
    pub source: OutputTextSource,
}

#[derive(Clone)]
pub(crate) struct OutputRange {
    pub start_offset: u64,
    pub end_offset: u64,
    pub bytes: Vec<u8>,
    pub events: Vec<OutputEvent>,
    pub text_spans: Vec<OutputTextSpan>,
}

#[derive(Clone, Debug)]
pub(crate) struct OutputEvent {
    pub offset: u64,
    pub kind: OutputEventKind,
}

#[derive(Clone, Debug)]
pub(crate) enum OutputEventKind {
    Image {
        id: String,
        data: String,
        mime_type: String,
        is_new: bool,
    },
    Text {
        text: String,
        is_stderr: bool,
        origin: ContentOrigin,
    },
    InputEcho {
        text: String,
    },
    InputWait,
    RequestBoundary,
    SessionEnd,
}

impl OutputEventKind {
    pub(crate) fn input_echo(prompt: &str, line: &str) -> Option<Self> {
        input_echo_text(prompt, line).map(|text| Self::InputEcho { text })
    }

    pub(crate) fn text_ends_with_newline(&self) -> Option<bool> {
        match self {
            Self::Text { text, .. } => Some(text.ends_with('\n')),
            Self::Image { .. }
            | Self::InputEcho { .. }
            | Self::InputWait
            | Self::RequestBoundary
            | Self::SessionEnd => None,
        }
    }
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProjectionMode {
    Reply,
    Transcript,
    Pager,
    Bundle,
}

impl ProjectionMode {
    fn input_echo_visibility(self) -> Option<ContentVisibility> {
        match self {
            Self::Reply => None,
            Self::Transcript => Some(ContentVisibility::ReplyAndTranscript),
            Self::Pager | Self::Bundle => Some(ContentVisibility::TranscriptOnly),
        }
    }
}

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

impl EventView for OutputEvent {
    fn offset(&self) -> u64 {
        self.offset
    }

    fn kind(&self) -> &OutputEventKind {
        &self.kind
    }
}

fn push_text_with_merge(
    contents: &mut Vec<WorkerContent>,
    text: String,
    stream: TextStream,
    origin: ContentOrigin,
    merge_with_previous: bool,
) -> bool {
    if text.is_empty() {
        return false;
    }
    if let Some(WorkerContent::ContentText {
        text: last_text,
        stream: last_stream,
        origin: last_origin,
        visibility,
    }) = contents.last_mut()
        && merge_with_previous
        && *last_stream == stream
        && *last_origin == origin
        && visibility.is_reply_visible()
    {
        last_text.push_str(&text);
        return true;
    }
    contents.push(WorkerContent::ContentText {
        text,
        stream,
        origin,
        visibility: Default::default(),
    });
    true
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RenderedTextState {
    stream: TextStream,
    origin: ContentOrigin,
    terminated: bool,
}

impl RenderedTextState {
    pub(crate) fn continuation(stream: TextStream, origin: ContentOrigin) -> Self {
        Self {
            stream,
            origin,
            terminated: false,
        }
    }
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
    let needs_separator = previous_text.is_some_and(|state| !state.terminated);
    if needs_separator {
        format!("\nstderr: {rendered}")
    } else {
        format!("stderr: {rendered}")
    }
}

fn rendered_state(stream: TextStream, origin: ContentOrigin, text: &str) -> RenderedTextState {
    RenderedTextState {
        stream,
        origin,
        terminated: text.ends_with('\n'),
    }
}

fn push_default_stdout(
    contents: &mut Vec<WorkerContent>,
    bytes: &[u8],
    start: usize,
    end: usize,
    merge_with_previous: bool,
    last_rendered_text: &mut Option<RenderedTextState>,
) -> bool {
    if start >= end || end > bytes.len() {
        return false;
    }
    let text = render_bytes(bytes, start, end);
    let pushed = push_text_with_merge(
        contents,
        text.clone(),
        TextStream::Stdout,
        ContentOrigin::Worker,
        merge_with_previous,
    );
    if pushed {
        *last_rendered_text = Some(rendered_state(
            TextStream::Stdout,
            ContentOrigin::Worker,
            &text,
        ));
    }
    pushed
}

fn push_span_text(
    contents: &mut Vec<WorkerContent>,
    bytes: &[u8],
    start: usize,
    end: usize,
    span: &impl TextSpanView,
    merge_with_previous: bool,
    last_rendered_text: &mut Option<RenderedTextState>,
) -> bool {
    if start >= end || end > bytes.len() {
        return false;
    }
    let rendered = render_bytes(bytes, start, end);
    let stream = if span.is_stderr() {
        TextStream::Stderr
    } else {
        TextStream::Stdout
    };
    let text = if span.is_stderr() {
        render_stderr_text(*last_rendered_text, span.origin(), rendered)
    } else {
        rendered
    };
    let pushed = push_text_with_merge(
        contents,
        text.clone(),
        stream,
        span.origin(),
        merge_with_previous,
    );
    if pushed {
        *last_rendered_text = Some(rendered_state(stream, span.origin(), &text));
    }
    pushed
}

fn push_generated_echo(
    contents: &mut Vec<WorkerContent>,
    text: &str,
    projection_mode: ProjectionMode,
    merge_with_previous_echo: bool,
) -> (bool, Option<RenderedTextState>) {
    if text.is_empty() {
        return (false, None);
    }
    let Some(visibility) = projection_mode.input_echo_visibility() else {
        return (false, None);
    };
    let next = match visibility {
        ContentVisibility::ReplyAndTranscript => WorkerContent::worker_stdout(text.to_string()),
        ContentVisibility::TranscriptOnly => WorkerContent::worker_stdout_transcript_only(text),
    };
    let WorkerContent::ContentText {
        text,
        stream,
        origin,
        visibility,
    } = next
    else {
        unreachable!("generated echoes are text");
    };
    let rendered_text = visibility
        .is_reply_visible()
        .then(|| rendered_state(stream, origin, &text));
    if let Some(WorkerContent::ContentText {
        text: last_text,
        stream: last_stream,
        origin: last_origin,
        visibility: last_visibility,
    }) = contents.last_mut()
        && merge_with_previous_echo
        && *last_stream == stream
        && *last_origin == origin
        && *last_visibility == visibility
    {
        last_text.push_str(&text);
        return (true, rendered_text);
    }
    contents.push(WorkerContent::ContentText {
        text,
        stream,
        origin,
        visibility,
    });
    (true, rendered_text)
}

struct TextRangeEmitter<'a, S> {
    contents: &'a mut Vec<WorkerContent>,
    bytes: &'a [u8],
    base_offset: u64,
    spans: &'a [S],
    last_rendered_text: &'a mut Option<RenderedTextState>,
}

impl<S: TextSpanView> TextRangeEmitter<'_, S> {
    fn emit(&mut self, start_offset: u64, end_offset: u64, merge_with_previous: bool) -> bool {
        let end_offset = end_offset.min(self.base_offset.saturating_add(self.bytes.len() as u64));
        let start_offset = start_offset.min(end_offset);
        if start_offset >= end_offset {
            return false;
        }

        let mut emitted = false;
        let mut can_merge = merge_with_previous;
        let mut cursor = start_offset;
        for span in self.spans {
            if span.end() <= start_offset {
                continue;
            }
            if span.start() >= end_offset {
                break;
            }

            let span_start = span.start().max(start_offset);
            if cursor < span_start
                && push_default_stdout(
                    self.contents,
                    self.bytes,
                    cursor.saturating_sub(self.base_offset) as usize,
                    span_start.saturating_sub(self.base_offset) as usize,
                    can_merge,
                    self.last_rendered_text,
                )
            {
                emitted = true;
                can_merge = true;
            }

            let span_end = span.end().min(end_offset);
            if push_span_text(
                self.contents,
                self.bytes,
                span_start.saturating_sub(self.base_offset) as usize,
                span_end.saturating_sub(self.base_offset) as usize,
                span,
                can_merge,
                self.last_rendered_text,
            ) {
                emitted = true;
                can_merge = true;
            }
            cursor = span_end;
        }

        if cursor < end_offset
            && push_default_stdout(
                self.contents,
                self.bytes,
                cursor.saturating_sub(self.base_offset) as usize,
                end_offset.saturating_sub(self.base_offset) as usize,
                can_merge,
                self.last_rendered_text,
            )
        {
            emitted = true;
        }
        emitted
    }
}

fn render_bytes(bytes: &[u8], start: usize, end: usize) -> String {
    let mut out = String::new();
    let mut remaining = &bytes[start..end];
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

pub(crate) fn render_bytes_with_events_and_spans_with_state<E: EventView, S: TextSpanView>(
    bytes: &[u8],
    base_offset: u64,
    end_offset: u64,
    spans: &[S],
    events: &[E],
    projection_mode: ProjectionMode,
    previous_rendered_text: Option<RenderedTextState>,
) -> (Vec<WorkerContent>, Option<RenderedTextState>) {
    let mut contents = Vec::new();
    let mut cursor = base_offset;
    let mut last_content_was_input_echo = false;
    let mut last_rendered_text = previous_rendered_text;
    {
        let mut text_emitter = TextRangeEmitter {
            contents: &mut contents,
            bytes,
            base_offset,
            spans,
            last_rendered_text: &mut last_rendered_text,
        };

        for event in events
            .iter()
            .filter(|event| event.offset() >= base_offset && event.offset() <= end_offset)
        {
            if event.offset() > base_offset.saturating_add(bytes.len() as u64) {
                break;
            }
            if event.offset() > cursor
                && text_emitter.emit(cursor, event.offset(), !last_content_was_input_echo)
            {
                last_content_was_input_echo = false;
            }
            match event.kind() {
                OutputEventKind::InputEcho { text } => {
                    let (pushed_echo, rendered_text) = push_generated_echo(
                        text_emitter.contents,
                        text,
                        projection_mode,
                        last_content_was_input_echo,
                    );
                    if let Some(rendered_text) = rendered_text {
                        *text_emitter.last_rendered_text = Some(rendered_text);
                    }
                    if pushed_echo {
                        last_content_was_input_echo = true;
                    }
                }
                OutputEventKind::Image { .. } | OutputEventKind::Text { .. } => {
                    text_emitter
                        .contents
                        .push(output_event_to_content(event.kind()));
                    last_content_was_input_echo = false;
                    *text_emitter.last_rendered_text = None;
                }
                OutputEventKind::InputWait => {
                    last_content_was_input_echo = false;
                }
                OutputEventKind::RequestBoundary | OutputEventKind::SessionEnd => {
                    *text_emitter.last_rendered_text = None;
                    last_content_was_input_echo = false;
                }
            }
            cursor = event.offset();
        }
        if cursor < end_offset {
            text_emitter.emit(cursor, end_offset, !last_content_was_input_echo);
        }
    }
    (contents, last_rendered_text)
}

#[cfg(test)]
pub(crate) fn contents_from_output_range(
    range: OutputRange,
    projection_mode: ProjectionMode,
) -> Vec<WorkerContent> {
    contents_from_output_range_with_state(range, projection_mode, None).0
}

pub(crate) fn contents_from_output_range_with_state(
    range: OutputRange,
    projection_mode: ProjectionMode,
    previous_rendered_text: Option<RenderedTextState>,
) -> (Vec<WorkerContent>, Option<RenderedTextState>) {
    if range.bytes.is_empty() && range.events.is_empty() {
        return (Vec::new(), previous_rendered_text);
    }
    let base_offset = range.start_offset;
    let end_offset = range.end_offset;
    let events: Vec<OutputEvent> = range
        .events
        .into_iter()
        .filter_map(|mut event| {
            if event.offset < base_offset || event.offset > end_offset {
                return None;
            }
            event.offset = event.offset.saturating_sub(base_offset);
            Some(event)
        })
        .collect();
    let end_offset = range.bytes.len() as u64;
    render_bytes_with_events_and_spans_with_state(
        &range.bytes,
        0,
        end_offset,
        &range.text_spans,
        &events,
        projection_mode,
        previous_rendered_text,
    )
}

fn output_event_to_content(kind: &OutputEventKind) -> WorkerContent {
    match kind {
        OutputEventKind::Image {
            data,
            mime_type,
            id,
            is_new,
            ..
        } => WorkerContent::ContentImage {
            data: data.clone(),
            mime_type: mime_type.clone(),
            id: id.clone(),
            is_new: *is_new,
        },
        OutputEventKind::Text {
            text,
            is_stderr,
            origin,
            ..
        } => {
            if *is_stderr {
                match origin {
                    ContentOrigin::Worker => WorkerContent::worker_stderr(text.clone()),
                    ContentOrigin::Server => WorkerContent::server_stderr(text.clone()),
                }
            } else {
                match origin {
                    ContentOrigin::Worker => WorkerContent::worker_stdout(text.clone()),
                    ContentOrigin::Server => WorkerContent::server_stdout(text.clone()),
                }
            }
        }
        OutputEventKind::InputEcho { .. } => unreachable!("input echo is handled by policy"),
        OutputEventKind::InputWait
        | OutputEventKind::RequestBoundary
        | OutputEventKind::SessionEnd => {
            unreachable!("timeline markers do not materialize")
        }
    }
}

impl TextSpanView for OutputTextSpan {
    fn start(&self) -> u64 {
        self.start_byte as u64
    }

    fn end(&self) -> u64 {
        self.end_byte as u64
    }

    fn is_stderr(&self) -> bool {
        self.is_stderr
    }

    fn origin(&self) -> ContentOrigin {
        self.origin
    }
}
