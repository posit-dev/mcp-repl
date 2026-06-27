#![cfg_attr(not(target_family = "unix"), allow(dead_code))]

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::ops::Range;
use std::sync::{Arc, Mutex};

pub(crate) use crate::resolved_output::{
    OutputEvent, OutputEventKind, OutputRange, OutputTextSource, OutputTextSpan, ProjectionMode,
    RenderedTextState,
};
use crate::worker_protocol::{ContentOrigin, WorkerContent};

pub(crate) const OUTPUT_RING_CAPACITY_BYTES: usize = 2 * 1024 * 1024;
pub(crate) const FILES_OUTPUT_TIMELINE_CAPACITY_BYTES: usize = 64 * 1024 * 1024;
const OUTPUT_RING_APPEND_CHUNK_MAX_BYTES: usize = 8 * 1024;
pub(crate) const OUTPUT_TRUNCATION_NOTICE: &str =
    "[repl] output truncated (older output dropped)\n";
const OUTPUT_OMISSION_NOTICE: &str = "[repl] output omitted (later content omitted)\n";

#[derive(Clone)]
pub(crate) struct OutputTimeline {
    ring: Arc<OutputRing>,
    state: Arc<Mutex<OutputTimelineState>>,
}

#[derive(Default)]
struct OutputTimelineState {
    utf8_tails: OutputUtf8Tails,
}

impl OutputTimeline {
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn new(ring: Arc<OutputRing>) -> Self {
        Self {
            ring,
            state: Arc::new(Mutex::new(OutputTimelineState::default())),
        }
    }

    pub(crate) fn with_capacity(capacity_bytes: usize) -> Self {
        Self {
            ring: Arc::new(OutputRing::new(capacity_bytes)),
            state: Arc::new(Mutex::new(OutputTimelineState::default())),
        }
    }

    pub(crate) fn with_head_retention_capacity(capacity_bytes: usize) -> Self {
        Self {
            ring: Arc::new(OutputRing::preserving_head(capacity_bytes)),
            state: Arc::new(Mutex::new(OutputTimelineState::default())),
        }
    }

    pub(crate) fn buffer(&self) -> OutputBuffer {
        OutputBuffer::new(self.ring.clone())
    }

    pub(crate) fn clear(&self) {
        let mut guard = self.state.lock().unwrap();
        guard.utf8_tails.clear();
        self.ring.reset();
    }

    pub(crate) fn append_text(&self, bytes: &[u8], is_stderr: bool, origin: ContentOrigin) {
        self.append_text_with_source(bytes, is_stderr, origin, false, OutputTextSource::Raw);
    }

    pub(crate) fn append_text_with_continuation(
        &self,
        bytes: &[u8],
        is_stderr: bool,
        origin: ContentOrigin,
        is_continuation: bool,
    ) {
        self.append_text_with_source(
            bytes,
            is_stderr,
            origin,
            is_continuation,
            OutputTextSource::Raw,
        );
    }

    pub(crate) fn append_ipc_text_with_continuation(
        &self,
        bytes: &[u8],
        is_stderr: bool,
        origin: ContentOrigin,
        is_continuation: bool,
    ) {
        self.append_text_with_source(
            bytes,
            is_stderr,
            origin,
            is_continuation,
            OutputTextSource::Ipc,
        );
    }

    fn append_text_with_source(
        &self,
        bytes: &[u8],
        is_stderr: bool,
        origin: ContentOrigin,
        is_continuation: bool,
        source: OutputTextSource,
    ) {
        if bytes.is_empty() {
            return;
        }
        let key = OutputUtf8TailKey {
            is_stderr,
            origin,
            source,
        };
        let mut guard = self.state.lock().unwrap();
        let pending = guard.utf8_tails.push(
            key,
            bytes,
            is_continuation || matches!(source, OutputTextSource::Raw),
            self.ring.capacity_bytes,
        );
        self.append_pending_locked(pending);
    }

    pub(crate) fn append_image(
        &self,
        id: String,
        mime_type: String,
        data: String,
        is_new: bool,
        _readline_results_seen: usize,
    ) {
        self.append_event(OutputEventKind::Image {
            id,
            data,
            mime_type,
            is_new,
        });
    }

    pub(crate) fn append_text_event(
        &self,
        text: String,
        is_stderr: bool,
        origin: ContentOrigin,
        _readline_results_seen: Option<usize>,
    ) {
        self.append_event(OutputEventKind::Text {
            text,
            is_stderr,
            origin,
        });
    }

    pub(crate) fn append_input_echo(&self, prompt: &str, line: &str) {
        if let Some(kind) = OutputEventKind::input_echo(prompt, line) {
            self.append_event(kind);
        }
    }

    pub(crate) fn append_input_wait(&self) {
        self.append_event(OutputEventKind::InputWait);
    }

    pub(crate) fn append_request_boundary(&self) {
        self.append_event(OutputEventKind::RequestBoundary);
    }

    pub(crate) fn append_session_end(&self) {
        self.append_event(OutputEventKind::SessionEnd);
    }

    pub(crate) fn last_text_ends_with_newline(&self) -> bool {
        self.ring.last_text_ends_with_newline()
    }

    pub(crate) fn seal_utf8_tails(&self) {
        let mut guard = self.state.lock().unwrap();
        let pending = guard.utf8_tails.drain();
        self.append_pending_locked(pending);
    }

    pub(crate) fn seal_utf8_tails_blocking_visible_output(&self) {
        let mut guard = self.state.lock().unwrap();
        let pending = guard.utf8_tails.drain_ready_after_visible_gaps();
        self.append_pending_locked(pending);
    }

    pub(crate) fn has_unflushable_utf8_tail(&self) -> bool {
        self.state.lock().unwrap().utf8_tails.has_unflushable_tail()
    }

    fn append_event(&self, kind: OutputEventKind) {
        let mut guard = self.state.lock().unwrap();
        let pending = guard.utf8_tails.push_event(kind, self.ring.capacity_bytes);
        self.append_pending_locked(pending);
    }

    fn append_pending_locked(&self, pending: Vec<OutputPendingEntry>) {
        for entry in pending {
            match entry {
                OutputPendingEntry::Text(entry) => {
                    self.ring.append_bytes_with_source(
                        &entry.bytes,
                        entry.key.is_stderr,
                        entry.key.origin,
                        entry.key.source,
                    );
                }
                OutputPendingEntry::Event(kind) => self.ring.append_materialized_event(kind),
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OutputUtf8TailKey {
    is_stderr: bool,
    origin: ContentOrigin,
    source: OutputTextSource,
}

struct OutputUtf8Tail {
    key: OutputUtf8TailKey,
    bytes: Vec<u8>,
    sealed: bool,
}

enum OutputPendingEntry {
    Text(OutputUtf8Tail),
    Event(OutputEventKind),
}

impl OutputPendingEntry {
    fn is_marker_event(&self) -> bool {
        matches!(
            self,
            OutputPendingEntry::Event(
                OutputEventKind::InputWait
                    | OutputEventKind::RequestBoundary
                    | OutputEventKind::SessionEnd
            )
        )
    }
}

#[derive(Default)]
struct OutputUtf8Tails {
    entries: Vec<OutputPendingEntry>,
}

impl OutputUtf8Tails {
    fn push(
        &mut self,
        key: OutputUtf8TailKey,
        bytes: &[u8],
        continue_previous_tail: bool,
        side_buffer_capacity_bytes: usize,
    ) -> Vec<OutputPendingEntry> {
        if bytes.is_empty() {
            return Vec::new();
        }
        if continue_previous_tail
            && let Some(index) = self.entries.iter().position(|entry| match entry {
                OutputPendingEntry::Text(entry) => {
                    entry.key == key && !entry.sealed && !entry.is_flushable()
                }
                OutputPendingEntry::Event(_) => false,
            })
        {
            let OutputPendingEntry::Text(entry) = &mut self.entries[index] else {
                unreachable!("matching pending UTF-8 entry must be text");
            };
            entry.bytes.extend_from_slice(bytes);
        } else {
            if let Some(entry) = self.entries.iter_mut().find_map(|entry| match entry {
                OutputPendingEntry::Text(entry)
                    if entry.key == key && !entry.sealed && !entry.is_flushable() =>
                {
                    Some(entry)
                }
                OutputPendingEntry::Text(_) | OutputPendingEntry::Event(_) => None,
            }) {
                entry.sealed = true;
            }
            self.entries.push(OutputPendingEntry::Text(OutputUtf8Tail {
                key,
                bytes: bytes.to_vec(),
                sealed: false,
            }));
        }
        self.drain_ready_bounded(side_buffer_capacity_bytes)
    }

    fn push_event(
        &mut self,
        kind: OutputEventKind,
        side_buffer_capacity_bytes: usize,
    ) -> Vec<OutputPendingEntry> {
        self.entries.push(OutputPendingEntry::Event(kind));
        self.drain_ready_bounded(side_buffer_capacity_bytes)
    }

    fn drain(&mut self) -> Vec<OutputPendingEntry> {
        for entry in &mut self.entries {
            if let OutputPendingEntry::Text(entry) = entry {
                entry.sealed = true;
            }
        }
        self.drain_ready()
    }

    fn clear(&mut self) {
        self.entries.clear();
    }

    fn drain_ready(&mut self) -> Vec<OutputPendingEntry> {
        let mut ready = Vec::new();
        while !self.entries.is_empty() {
            let OutputPendingEntry::Text(front) = &mut self.entries[0] else {
                ready.push(self.entries.remove(0));
                continue;
            };
            if front.sealed {
                ready.push(materialize_pending_entry(self.entries.remove(0)));
                continue;
            }

            let flushable_len = flushable_prefix_len(&front.bytes);
            if flushable_len == 0 {
                break;
            }
            if flushable_len == front.bytes.len() {
                ready.push(self.entries.remove(0));
                continue;
            }
            let key = front.key;
            let bytes = front.bytes[..flushable_len].to_vec();
            front.bytes.drain(..flushable_len);
            ready.push(OutputPendingEntry::Text(OutputUtf8Tail {
                key,
                bytes,
                sealed: true,
            }));
            break;
        }
        ready
    }

    fn drain_ready_bounded(
        &mut self,
        side_buffer_capacity_bytes: usize,
    ) -> Vec<OutputPendingEntry> {
        let mut ready = self.drain_ready();
        if self.blocked_side_buffer_bytes() > side_buffer_capacity_bytes {
            ready.extend(self.drain_ready_after_visible_gaps());
        }
        ready
    }

    fn drain_ready_after_visible_gaps(&mut self) -> Vec<OutputPendingEntry> {
        let mut ready = Vec::new();
        while !self.entries.is_empty() {
            let has_later_visible_entries = self
                .entries
                .iter()
                .skip(1)
                .any(|entry| !entry.is_marker_event());
            let OutputPendingEntry::Text(front) = &mut self.entries[0] else {
                ready.push(self.entries.remove(0));
                continue;
            };
            if front.sealed {
                ready.push(materialize_pending_entry(self.entries.remove(0)));
                continue;
            }

            let flushable_len = flushable_prefix_len(&front.bytes);
            if flushable_len == 0 {
                if has_later_visible_entries {
                    front.sealed = true;
                    ready.push(materialize_pending_entry(self.entries.remove(0)));
                    continue;
                }
                break;
            }
            if flushable_len == front.bytes.len() {
                ready.push(self.entries.remove(0));
                continue;
            }
            let key = front.key;
            let bytes = front.bytes[..flushable_len].to_vec();
            front.bytes.drain(..flushable_len);
            ready.push(OutputPendingEntry::Text(OutputUtf8Tail {
                key,
                bytes,
                sealed: true,
            }));
            if !has_later_visible_entries {
                break;
            }
        }
        ready
    }

    fn blocked_side_buffer_bytes(&self) -> usize {
        let Some(OutputPendingEntry::Text(front)) = self.entries.first() else {
            return 0;
        };
        if front.sealed || front.is_flushable() {
            return 0;
        }
        self.entries.iter().skip(1).fold(0usize, |total, entry| {
            total.saturating_add(pending_entry_size_bytes(entry))
        })
    }

    fn has_unflushable_tail(&self) -> bool {
        self.entries.iter().any(|entry| {
            matches!(
                entry,
                OutputPendingEntry::Text(entry) if !entry.sealed && !entry.is_flushable()
            )
        })
    }
}

impl OutputUtf8Tail {
    fn is_flushable(&self) -> bool {
        self.sealed || flushable_prefix_len(&self.bytes) == self.bytes.len()
    }
}

fn materialize_pending_entry(entry: OutputPendingEntry) -> OutputPendingEntry {
    match entry {
        OutputPendingEntry::Text(entry) => OutputPendingEntry::Text(entry.materialize()),
        OutputPendingEntry::Event(kind) => OutputPendingEntry::Event(kind),
    }
}

impl OutputUtf8Tail {
    fn materialize(mut self) -> Self {
        if self.sealed {
            self.bytes = escape_incomplete_utf8_tail(&self.bytes);
        }
        self
    }
}

fn escape_incomplete_utf8_tail(bytes: &[u8]) -> Vec<u8> {
    let flushable_len = flushable_prefix_len(bytes);
    if flushable_len == bytes.len() {
        return bytes.to_vec();
    }

    let mut escaped = Vec::with_capacity(
        flushable_len.saturating_add(bytes.len().saturating_sub(flushable_len).saturating_mul(4)),
    );
    escaped.extend_from_slice(&bytes[..flushable_len]);
    let mut tail = String::new();
    for byte in &bytes[flushable_len..] {
        let _ = write!(&mut tail, "\\x{byte:02X}");
    }
    escaped.extend_from_slice(tail.as_bytes());
    escaped
}

fn pending_entry_size_bytes(entry: &OutputPendingEntry) -> usize {
    match entry {
        OutputPendingEntry::Text(entry) => entry.bytes.len(),
        OutputPendingEntry::Event(kind) => event_size_bytes(kind),
    }
}

#[derive(Clone)]
pub(crate) struct OutputBuffer {
    cursor: Arc<Mutex<OutputCursor>>,
    ring: Arc<OutputRing>,
}

#[derive(Default)]
struct OutputCursor {
    offset: Option<u64>,
    last_rendered_text: Option<RenderedTextState>,
}

impl OutputBuffer {
    pub(crate) fn new(ring: Arc<OutputRing>) -> Self {
        Self {
            cursor: Arc::new(Mutex::new(OutputCursor::default())),
            ring,
        }
    }

    fn ring(&self) -> Arc<OutputRing> {
        self.ring.clone()
    }

    pub(crate) fn current_offset(&self) -> Option<u64> {
        let guard = self.cursor.lock().unwrap();
        guard.offset
    }

    pub(crate) fn end_offset(&self) -> Option<u64> {
        Some(self.ring().end_offset())
    }

    pub(crate) fn saw_stderr_in_range(&self, start_offset: u64, end_offset: u64) -> bool {
        let ring = self.ring();
        ring.saw_stderr_in_range(start_offset, end_offset)
    }

    pub(crate) fn read_range(&self, start_offset: u64, end_offset: u64) -> OutputRange {
        let ring = self.ring();
        ring.read_range(start_offset, end_offset)
    }

    pub(crate) fn start_capture(&self) {
        {
            let guard = self.cursor.lock().unwrap();
            if guard.offset.is_some() {
                return;
            }
        }

        let ring = self.ring();
        let start_offset = ring.start_offset();

        let mut guard = self.cursor.lock().unwrap();
        if guard.offset.is_none() {
            guard.offset = Some(start_offset);
        }
    }

    fn read_offset_with_ring(&self) -> Option<(u64, Arc<OutputRing>)> {
        let ring = self.ring();
        let offset = {
            let guard = self.cursor.lock().unwrap();
            guard.offset?
        };
        Some((offset, ring))
    }

    fn read_offset_state_with_ring(
        &self,
    ) -> Option<(u64, Option<RenderedTextState>, Arc<OutputRing>)> {
        let ring = self.ring();
        let (offset, last_rendered_text) = {
            let guard = self.cursor.lock().unwrap();
            (guard.offset?, guard.last_rendered_text)
        };
        Some((offset, last_rendered_text, ring))
    }

    pub(crate) fn advance_offset_to(&self, offset: u64) {
        let mut guard = self.cursor.lock().unwrap();
        guard.offset = Some(offset);
        guard.last_rendered_text = None;
        drop(guard);
        self.ring().consume_to(offset);
    }

    pub(crate) fn rendered_text_state(&self) -> Option<RenderedTextState> {
        let guard = self.cursor.lock().unwrap();
        guard.last_rendered_text
    }

    pub(crate) fn advance_offset_to_with_rendered_text_and_boundary_events(
        &self,
        offset: u64,
        last_rendered_text: Option<RenderedTextState>,
        boundary_events_consumed: usize,
    ) {
        let mut guard = self.cursor.lock().unwrap();
        guard.offset = Some(offset);
        guard.last_rendered_text = last_rendered_text;
        drop(guard);
        self.ring()
            .consume_to_with_boundary_events(offset, boundary_events_consumed);
    }

    pub fn has_pending_output(&self) -> bool {
        let Some((offset, ring)) = self.read_offset_with_ring() else {
            return false;
        };
        ring.end_offset() > offset || ring.has_materialized_events_at_or_after(offset)
    }

    pub(crate) fn clear(&self) {
        let mut guard = self.cursor.lock().unwrap();
        guard.offset = None;
        guard.last_rendered_text = None;
        drop(guard);
        self.ring().reset();
    }

    pub(crate) fn current_progress_seq(&self) -> u64 {
        self.ring().progress_seq()
    }

    pub(crate) fn current_settle_state(&self) -> TimelineSettleState {
        let Some((offset, ring)) = self.read_offset_with_ring() else {
            return TimelineSettleState {
                progress_seq: self.current_progress_seq(),
                input_echoes_seen: 0,
                has_image: false,
            };
        };
        ring.settle_state_at_or_after(offset)
    }

    pub(crate) fn drain_formatted(
        &self,
        projection_mode: ProjectionMode,
        flush_incomplete: bool,
    ) -> FormattedPendingOutput {
        let Some((start_offset, previous_rendered_text, ring)) = self.read_offset_state_with_ring()
        else {
            return FormattedPendingOutput::default();
        };
        let end_offset = ring.end_offset();
        let mut range = ring.read_range(start_offset, end_offset);
        let consumed_end = if flush_incomplete {
            range.end_offset
        } else {
            trim_range_to_complete_utf8(&mut range)
        };
        let boundary_events_consumed = output_range_boundary_event_count(&range, consumed_end);
        let saw_stderr = ring.saw_stderr_in_range(start_offset.min(consumed_end), consumed_end);
        let (contents, last_rendered_text) =
            crate::resolved_output::contents_from_output_range_with_state(
                range,
                projection_mode,
                previous_rendered_text,
            );
        self.advance_offset_to_with_rendered_text_and_boundary_events(
            consumed_end,
            last_rendered_text,
            boundary_events_consumed,
        );
        FormattedPendingOutput {
            contents,
            saw_stderr,
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct FormattedPendingOutput {
    pub(crate) contents: Vec<WorkerContent>,
    pub(crate) saw_stderr: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TimelineSettleState {
    pub(crate) progress_seq: u64,
    pub(crate) input_echoes_seen: usize,
    pub(crate) has_image: bool,
}

pub(crate) struct OutputRing {
    capacity_bytes: usize,
    retention: OutputRetention,
    inner: Mutex<OutputRingInner>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutputRetention {
    Tail,
    Head,
}

struct OutputRingInner {
    chunks: VecDeque<OutputChunk>,
    line_ends: VecDeque<u64>,
    events: VecDeque<OutputEvent>,
    start_offset: u64,
    end_offset: u64,
    buffered_bytes: usize,
    buffered_event_bytes: usize,
    progress_seq: u64,
}

struct OutputChunk {
    start_offset: u64,
    bytes: Arc<[u8]>,
    range: Range<usize>,
    is_stderr: bool,
    origin: ContentOrigin,
    source: OutputTextSource,
}

struct OutputSlice {
    bytes: Arc<[u8]>,
    range: Range<usize>,
    is_stderr: bool,
    origin: ContentOrigin,
    source: OutputTextSource,
}

struct CollectedRange {
    slices: Vec<OutputSlice>,
    events: Vec<OutputEvent>,
    start_offset: u64,
    end_offset: u64,
}

impl OutputRing {
    fn new(capacity_bytes: usize) -> Self {
        Self::with_retention(capacity_bytes, OutputRetention::Tail)
    }

    fn preserving_head(capacity_bytes: usize) -> Self {
        Self::with_retention(capacity_bytes, OutputRetention::Head)
    }

    fn with_retention(capacity_bytes: usize, retention: OutputRetention) -> Self {
        Self {
            capacity_bytes,
            retention,
            inner: Mutex::new(OutputRingInner {
                chunks: VecDeque::new(),
                line_ends: VecDeque::new(),
                events: VecDeque::new(),
                start_offset: 0,
                end_offset: 0,
                buffered_bytes: 0,
                buffered_event_bytes: 0,
                progress_seq: 0,
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_capacity(capacity_bytes: usize) -> Self {
        Self::new(capacity_bytes)
    }

    fn start_offset(&self) -> u64 {
        self.inner.lock().unwrap().start_offset
    }

    #[cfg(test)]
    pub(crate) fn append_bytes(&self, bytes: &[u8], is_stderr: bool, origin: ContentOrigin) {
        self.append_bytes_with_source(bytes, is_stderr, origin, OutputTextSource::Raw);
    }

    pub(crate) fn append_bytes_with_source(
        &self,
        bytes: &[u8],
        is_stderr: bool,
        origin: ContentOrigin,
        source: OutputTextSource,
    ) {
        if bytes.is_empty() {
            return;
        }

        {
            let mut guard = self.inner.lock().unwrap();
            guard.note_progress();
        }

        match self.retention {
            OutputRetention::Tail => {
                self.append_bytes_retaining_tail(bytes, is_stderr, origin, source);
            }
            OutputRetention::Head => {
                self.append_bytes_retaining_head(bytes, is_stderr, origin, source);
            }
        }
    }

    fn append_bytes_retaining_tail(
        &self,
        bytes: &[u8],
        is_stderr: bool,
        origin: ContentOrigin,
        source: OutputTextSource,
    ) {
        let mut dropped_any = false;
        let mut remaining = bytes;
        let max_chunk_len = output_ring_append_chunk_len(self.capacity_bytes);
        while !remaining.is_empty() {
            let chunk_len = remaining.len().min(max_chunk_len);
            let (head, tail) = remaining.split_at(chunk_len);
            remaining = tail;

            let mut guard = self.inner.lock().unwrap();
            let dropped = guard.make_room_for(head.len(), self.capacity_bytes);
            dropped_any |= dropped.dropped_visible();
            Self::append_chunk_locked(&mut guard, head, is_stderr, origin, source);
        }

        if dropped_any {
            let mut guard = self.inner.lock().unwrap();
            let notice_offset = guard.end_offset;
            self.append_truncation_notice_locked(&mut guard, notice_offset, 0);
        }
    }

    fn append_bytes_retaining_head(
        &self,
        bytes: &[u8],
        is_stderr: bool,
        origin: ContentOrigin,
        source: OutputTextSource,
    ) {
        let mut remaining = bytes;
        let max_chunk_len = output_ring_append_chunk_len(self.capacity_bytes);
        while !remaining.is_empty() {
            let chunk_len = remaining.len().min(max_chunk_len);
            let (head, tail) = remaining.split_at(chunk_len);
            remaining = tail;

            let mut guard = self.inner.lock().unwrap();
            let incoming_len = chunk_len.saturating_add(remaining.len());
            if guard.drop_input_echoes_for_room(incoming_len, self.capacity_bytes) > 0 {
                self.append_omission_notice_locked(&mut guard);
            }
            if guard.total_buffered_bytes().saturating_add(incoming_len) <= self.capacity_bytes {
                Self::append_chunk_locked(&mut guard, head, is_stderr, origin, source);
                continue;
            }

            let reserve = self.head_omission_notice_reserve_locked(&guard);
            let effective_capacity = self.capacity_bytes.saturating_sub(reserve);
            if guard.drop_input_echoes_for_room(incoming_len, effective_capacity) > 0 {
                self.append_omission_notice_locked(&mut guard);
            }
            let available = effective_capacity.saturating_sub(guard.total_buffered_bytes());
            if available == 0 {
                self.append_omission_notice_locked(&mut guard);
                break;
            }
            let retained_len = chunk_len.min(available);
            Self::append_chunk_locked(&mut guard, &head[..retained_len], is_stderr, origin, source);
            if retained_len < chunk_len {
                self.append_omission_notice_locked(&mut guard);
                break;
            }
        }
    }

    fn append_chunk_locked(
        guard: &mut OutputRingInner,
        bytes: &[u8],
        is_stderr: bool,
        origin: ContentOrigin,
        source: OutputTextSource,
    ) {
        if bytes.is_empty() {
            return;
        }
        let newline_indices: Vec<usize> = bytes
            .iter()
            .enumerate()
            .filter_map(|(idx, byte)| (*byte == b'\n').then_some(idx))
            .collect();
        let bytes: Arc<[u8]> = bytes.to_vec().into();
        let bytes_len = bytes.len();
        let start_offset = guard.end_offset;
        guard.end_offset = guard
            .end_offset
            .saturating_add(bytes_len.try_into().unwrap_or(u64::MAX));
        guard.buffered_bytes = guard.buffered_bytes.saturating_add(bytes_len);

        for idx in newline_indices {
            let offset = start_offset.saturating_add((idx + 1) as u64);
            guard.line_ends.push_back(offset);
        }

        guard.chunks.push_back(OutputChunk {
            start_offset,
            bytes,
            range: 0..bytes_len,
            is_stderr,
            origin,
            source,
        });
    }

    pub(crate) fn end_offset(&self) -> u64 {
        self.inner.lock().unwrap().end_offset
    }

    #[cfg(test)]
    pub(crate) fn append_event(&self, offset: u64, kind: OutputEventKind) {
        let mut guard = self.inner.lock().unwrap();
        guard.note_progress();
        let event_bytes = event_size_bytes(&kind);
        if event_bytes > self.capacity_bytes {
            return;
        }

        let dropped = guard.make_room_for(event_bytes, self.capacity_bytes);
        let mut event_offset = offset.max(guard.start_offset);
        if dropped.dropped_visible() {
            self.append_truncation_notice_locked(&mut guard, event_offset, event_bytes);
            event_offset = event_offset.max(guard.start_offset);
        }
        guard.buffered_event_bytes = guard.buffered_event_bytes.saturating_add(event_bytes);
        guard.events.push_back(OutputEvent {
            offset: event_offset,
            kind,
        });
    }

    fn append_materialized_event(&self, kind: OutputEventKind) {
        match kind {
            OutputEventKind::Image { .. } | OutputEventKind::Text { .. } => {
                self.append_visible_event(kind);
            }
            OutputEventKind::InputEcho { .. } => self.append_input_echo_event(kind),
            OutputEventKind::InputWait
            | OutputEventKind::RequestBoundary
            | OutputEventKind::SessionEnd => self.append_marker_event(kind),
        }
    }

    fn append_visible_event(&self, kind: OutputEventKind) {
        let mut guard = self.inner.lock().unwrap();
        guard.note_progress();
        let event_bytes = event_size_bytes(&kind);
        if event_bytes > self.capacity_bytes {
            if self.retention == OutputRetention::Head {
                self.append_omission_notice_locked(&mut guard);
            }
            return;
        }

        let mut event_offset = guard.end_offset.max(guard.start_offset);
        match self.retention {
            OutputRetention::Tail => {
                let dropped = guard.make_room_for(event_bytes, self.capacity_bytes);
                if dropped.dropped_visible() {
                    self.append_truncation_notice_locked(&mut guard, event_offset, event_bytes);
                    event_offset = event_offset.max(guard.start_offset);
                }
            }
            OutputRetention::Head => {
                if guard.drop_input_echoes_for_room(event_bytes, self.capacity_bytes) > 0 {
                    self.append_omission_notice_locked(&mut guard);
                }
                if guard.total_buffered_bytes().saturating_add(event_bytes) > self.capacity_bytes {
                    let reserve = self.head_omission_notice_reserve_locked(&guard);
                    let effective_capacity = self.capacity_bytes.saturating_sub(reserve);
                    if guard.drop_input_echoes_for_room(event_bytes, effective_capacity) > 0 {
                        self.append_omission_notice_locked(&mut guard);
                    }
                    if guard.total_buffered_bytes().saturating_add(event_bytes) > effective_capacity
                    {
                        self.append_omission_notice_locked(&mut guard);
                        return;
                    }
                }
            }
        }
        guard.buffered_event_bytes = guard.buffered_event_bytes.saturating_add(event_bytes);
        guard.events.push_back(OutputEvent {
            offset: event_offset,
            kind,
        });
    }

    fn append_input_echo_event(&self, kind: OutputEventKind) {
        let mut guard = self.inner.lock().unwrap();
        guard.note_progress();
        let event_bytes = event_size_bytes(&kind);

        match self.retention {
            OutputRetention::Tail => {
                if event_bytes > self.capacity_bytes {
                    return;
                }
                if !guard.make_room_for_input_echo(event_bytes, self.capacity_bytes) {
                    return;
                }
            }
            OutputRetention::Head => {
                if event_bytes > self.capacity_bytes {
                    self.append_omission_notice_locked(&mut guard);
                    return;
                }
                if guard.has_omission_notice() {
                    return;
                }
                if guard.total_buffered_bytes().saturating_add(event_bytes) > self.capacity_bytes {
                    self.append_omission_notice_locked(&mut guard);
                    return;
                }
            }
        }

        let event_offset = guard.end_offset.max(guard.start_offset);
        guard.buffered_event_bytes = guard.buffered_event_bytes.saturating_add(event_bytes);
        guard.events.push_back(OutputEvent {
            offset: event_offset,
            kind,
        });
    }

    pub(crate) fn append_marker_event(&self, kind: OutputEventKind) {
        let mut guard = self.inner.lock().unwrap();
        guard.note_progress();
        let event_offset = guard.end_offset.max(guard.start_offset);
        guard.events.push_back(OutputEvent {
            offset: event_offset,
            kind,
        });
    }

    pub(crate) fn read_range(&self, start_offset: u64, end_offset: u64) -> OutputRange {
        let collected = self.collect_range(start_offset, Some(end_offset));
        let bytes = assemble_bytes(&collected.slices);
        let mut text_spans: Vec<OutputTextSpan> = Vec::new();
        let mut cursor = 0usize;
        for slice in &collected.slices {
            let slice_len = slice.range.len();
            if slice_len == 0 {
                continue;
            }
            let start_byte = cursor;
            let end_byte = start_byte.saturating_add(slice_len);
            if let Some(last) = text_spans.last_mut()
                && last.is_stderr == slice.is_stderr
                && last.origin == slice.origin
                && last.source == slice.source
                && last.end_byte == start_byte
            {
                last.end_byte = end_byte;
            } else {
                text_spans.push(OutputTextSpan {
                    start_byte,
                    end_byte,
                    is_stderr: slice.is_stderr,
                    origin: slice.origin,
                    source: slice.source,
                });
            }
            cursor = end_byte;
        }
        OutputRange {
            start_offset: collected.start_offset,
            end_offset: collected.end_offset,
            bytes,
            events: collected.events,
            text_spans,
        }
    }

    pub(crate) fn saw_stderr_in_range(&self, start_offset: u64, end_offset: u64) -> bool {
        let guard = self.inner.lock().unwrap();
        let end_offset = end_offset.min(guard.end_offset);
        if start_offset >= end_offset {
            return false;
        }

        let effective_start = start_offset.max(guard.start_offset);
        for chunk in guard.chunks.iter() {
            if chunk.start_offset >= end_offset {
                break;
            }
            let chunk_len: u64 = chunk.range.len().try_into().unwrap_or(u64::MAX);
            let chunk_end = chunk.start_offset.saturating_add(chunk_len);
            if chunk_end <= effective_start {
                continue;
            }
            if chunk.is_stderr {
                return true;
            }
        }
        false
    }

    fn has_materialized_events_at_or_after(&self, offset: u64) -> bool {
        let guard = self.inner.lock().unwrap();
        guard.events.iter().any(|event| {
            event.offset >= offset
                && matches!(
                    event.kind,
                    OutputEventKind::Image { .. } | OutputEventKind::Text { .. }
                )
        })
    }

    fn last_text_ends_with_newline(&self) -> bool {
        let guard = self.inner.lock().unwrap();
        let last_chunk = guard.chunks.back().and_then(|chunk| {
            chunk
                .range
                .is_empty()
                .then_some((chunk.start_offset, true))
                .or_else(|| {
                    chunk.range.clone().last().map(|idx| {
                        (
                            chunk.start_offset.saturating_add(chunk.range.len() as u64),
                            chunk.bytes[idx] == b'\n',
                        )
                    })
                })
        });
        let last_event = guard.events.iter().rev().find_map(|event| {
            event
                .kind
                .text_ends_with_newline()
                .map(|ends_with_newline| (event.offset, ends_with_newline))
        });
        match (last_chunk, last_event) {
            (Some((chunk_offset, chunk_newline)), Some((event_offset, event_newline))) => {
                if event_offset >= chunk_offset {
                    event_newline
                } else {
                    chunk_newline
                }
            }
            (Some((_, chunk_newline)), None) => chunk_newline,
            (None, Some((_, event_newline))) => event_newline,
            (None, None) => true,
        }
    }

    fn progress_seq(&self) -> u64 {
        self.inner.lock().unwrap().progress_seq
    }

    fn settle_state_at_or_after(&self, offset: u64) -> TimelineSettleState {
        let guard = self.inner.lock().unwrap();
        let mut input_echoes_seen = 0usize;
        let mut has_image = false;
        for event in guard.events.iter().filter(|event| event.offset >= offset) {
            match event.kind {
                OutputEventKind::InputEcho { .. } => {
                    input_echoes_seen = input_echoes_seen.saturating_add(1);
                }
                OutputEventKind::Image { .. } => has_image = true,
                OutputEventKind::Text { .. }
                | OutputEventKind::InputWait
                | OutputEventKind::RequestBoundary
                | OutputEventKind::SessionEnd => {}
            }
        }
        TimelineSettleState {
            progress_seq: guard.progress_seq,
            input_echoes_seen,
            has_image,
        }
    }

    fn consume_to(&self, offset: u64) {
        let mut guard = self.inner.lock().unwrap();
        let offset = offset.min(guard.end_offset);
        if offset < guard.start_offset {
            return;
        }
        if offset == guard.start_offset {
            guard.cleanup_front();
            return;
        }
        guard.trim_to_offset(offset);
    }

    fn consume_to_with_boundary_events(&self, offset: u64, boundary_events_consumed: usize) {
        let mut guard = self.inner.lock().unwrap();
        let offset = offset.min(guard.end_offset);
        if offset < guard.start_offset {
            return;
        }
        guard.trim_to_offset_consuming_boundary_events(offset, boundary_events_consumed);
    }

    fn reset(&self) {
        let mut guard = self.inner.lock().unwrap();
        guard.chunks.clear();
        guard.line_ends.clear();
        guard.events.clear();
        guard.start_offset = 0;
        guard.end_offset = 0;
        guard.buffered_bytes = 0;
        guard.buffered_event_bytes = 0;
        guard.progress_seq = 0;
    }

    fn collect_range(&self, start_offset: u64, end_offset: Option<u64>) -> CollectedRange {
        let guard = self.inner.lock().unwrap();
        let end_offset = end_offset.unwrap_or(guard.end_offset).min(guard.end_offset);

        let effective_start = start_offset.max(guard.start_offset);

        let mut slices = Vec::new();
        if effective_start < end_offset {
            for chunk in guard.chunks.iter() {
                if chunk.start_offset >= end_offset {
                    break;
                }
                let chunk_len: u64 = chunk.range.len().try_into().unwrap_or(u64::MAX);
                let chunk_end = chunk.start_offset.saturating_add(chunk_len);
                if chunk_end <= effective_start {
                    continue;
                }

                let slice_start_offset =
                    effective_start.saturating_sub(chunk.start_offset) as usize;
                let slice_end_offset =
                    end_offset.saturating_sub(chunk.start_offset).min(chunk_len) as usize;

                if slice_start_offset >= slice_end_offset {
                    continue;
                }

                let chunk_start = chunk.range.start;
                let slice_start = chunk_start.saturating_add(slice_start_offset);
                let slice_end = chunk_start.saturating_add(slice_end_offset);
                if slice_start >= slice_end || slice_end > chunk.range.end {
                    continue;
                }

                slices.push(OutputSlice {
                    bytes: chunk.bytes.clone(),
                    range: slice_start..slice_end,
                    is_stderr: chunk.is_stderr,
                    origin: chunk.origin,
                    source: chunk.source,
                });
            }
        }

        let mut events = Vec::new();
        if effective_start <= end_offset {
            for event in guard.events.iter() {
                if event.offset < effective_start {
                    continue;
                }
                if event.offset > end_offset {
                    break;
                }
                events.push(event.clone());
            }
        }

        CollectedRange {
            slices,
            events,
            start_offset: effective_start,
            end_offset,
        }
    }

    fn append_truncation_notice_locked(
        &self,
        guard: &mut OutputRingInner,
        offset: u64,
        extra_bytes: usize,
    ) {
        let notice_kind = OutputEventKind::Text {
            text: OUTPUT_TRUNCATION_NOTICE.to_string(),
            is_stderr: false,
            origin: ContentOrigin::Server,
        };
        let notice_bytes = event_size_bytes(&notice_kind);
        if notice_bytes.saturating_add(extra_bytes) > self.capacity_bytes {
            return;
        }
        let _ = guard.make_room_for(
            notice_bytes.saturating_add(extra_bytes),
            self.capacity_bytes,
        );
        let notice_offset = offset.max(guard.start_offset);
        guard.note_progress();
        guard.buffered_event_bytes = guard.buffered_event_bytes.saturating_add(notice_bytes);
        guard.events.push_back(OutputEvent {
            offset: notice_offset,
            kind: notice_kind,
        });
    }

    fn head_omission_notice_reserve_locked(&self, guard: &OutputRingInner) -> usize {
        if self.retention != OutputRetention::Head || guard.has_omission_notice() {
            return 0;
        }
        let notice_bytes = event_size_bytes(&omission_notice_kind());
        if notice_bytes <= self.capacity_bytes {
            notice_bytes
        } else {
            0
        }
    }

    fn append_omission_notice_locked(&self, guard: &mut OutputRingInner) {
        if self.retention != OutputRetention::Head || guard.has_omission_notice() {
            return;
        }
        let notice_kind = omission_notice_kind();
        let notice_bytes = event_size_bytes(&notice_kind);
        if notice_bytes > self.capacity_bytes {
            return;
        }
        guard.trim_tail_for_room(notice_bytes, self.capacity_bytes);
        if guard.total_buffered_bytes().saturating_add(notice_bytes) > self.capacity_bytes {
            return;
        }
        let notice_offset = guard.retained_end_offset();
        guard.note_progress();
        guard.buffered_event_bytes = guard.buffered_event_bytes.saturating_add(notice_bytes);
        guard.events.push_back(OutputEvent {
            offset: notice_offset,
            kind: notice_kind,
        });
    }
}

impl OutputRingInner {
    fn note_progress(&mut self) {
        self.progress_seq = self.progress_seq.saturating_add(1);
    }

    fn total_buffered_bytes(&self) -> usize {
        self.buffered_bytes
            .saturating_add(self.buffered_event_bytes)
    }

    fn has_omission_notice(&self) -> bool {
        self.events.iter().any(|event| match &event.kind {
            OutputEventKind::Text {
                text,
                origin: ContentOrigin::Server,
                ..
            } => text == OUTPUT_OMISSION_NOTICE,
            _ => false,
        })
    }

    fn pop_front_event(&mut self) -> bool {
        if let Some(event) = self.events.pop_front() {
            self.buffered_event_bytes = self
                .buffered_event_bytes
                .saturating_sub(event_size_bytes(&event.kind));
            return true;
        }
        false
    }

    fn pop_oldest_input_echo_event(&mut self) -> bool {
        let Some(index) = self
            .events
            .iter()
            .position(|event| matches!(event.kind, OutputEventKind::InputEcho { .. }))
        else {
            return false;
        };
        let Some(event) = self.events.remove(index) else {
            return false;
        };
        self.buffered_event_bytes = self
            .buffered_event_bytes
            .saturating_sub(event_size_bytes(&event.kind));
        true
    }

    fn pop_back_event(&mut self) -> Option<OutputEvent> {
        let event = self.events.pop_back()?;
        self.buffered_event_bytes = self
            .buffered_event_bytes
            .saturating_sub(event_size_bytes(&event.kind));
        Some(event)
    }

    fn drop_input_echoes_for_room(&mut self, needed_bytes: usize, capacity_bytes: usize) -> usize {
        let mut dropped = 0usize;
        while self.total_buffered_bytes().saturating_add(needed_bytes) > capacity_bytes {
            if !self.pop_oldest_input_echo_event() {
                break;
            }
            dropped = dropped.saturating_add(1);
        }
        dropped
    }

    fn trim_tail_for_room(&mut self, needed_bytes: usize, capacity_bytes: usize) -> DropStats {
        let mut dropped = DropStats::default();
        while self.total_buffered_bytes().saturating_add(needed_bytes) > capacity_bytes {
            let last_chunk_end = self
                .chunks
                .back()
                .map(|chunk| chunk.start_offset.saturating_add(chunk.range.len() as u64));
            let last_event_offset = self.events.back().map(|event| event.offset);

            if last_event_offset.is_some()
                && last_event_offset.unwrap_or(0) >= last_chunk_end.unwrap_or(0)
            {
                let Some(event) = self.pop_back_event() else {
                    break;
                };
                if !matches!(event.kind, OutputEventKind::InputEcho { .. }) {
                    dropped.dropped_visible_events =
                        dropped.dropped_visible_events.saturating_add(1);
                }
                continue;
            }

            let excess = self
                .total_buffered_bytes()
                .saturating_add(needed_bytes)
                .saturating_sub(capacity_bytes);
            let Some(back) = self.chunks.back_mut() else {
                break;
            };
            let drop_len = excess.min(back.range.len());
            if drop_len == 0 {
                break;
            }
            back.range.end = back.range.end.saturating_sub(drop_len);
            self.buffered_bytes = self.buffered_bytes.saturating_sub(drop_len);
            dropped.dropped_bytes = dropped.dropped_bytes.saturating_add(drop_len as u64);
            if back.range.is_empty() {
                let _ = self.chunks.pop_back();
            }
            self.cleanup_back();
        }
        dropped
    }

    fn make_room_for_input_echo(&mut self, needed_bytes: usize, capacity_bytes: usize) -> bool {
        if needed_bytes >= capacity_bytes {
            return false;
        }
        self.drop_input_echoes_for_room(needed_bytes, capacity_bytes);
        self.total_buffered_bytes().saturating_add(needed_bytes) <= capacity_bytes
    }

    fn make_room_for(&mut self, needed_bytes: usize, capacity_bytes: usize) -> DropStats {
        let mut dropped = DropStats::default();
        if needed_bytes >= capacity_bytes {
            // If a single chunk consumes the full capacity, drop everything else.
            dropped.dropped_bytes = self.end_offset.saturating_sub(self.start_offset);
            for event in &self.events {
                if !matches!(event.kind, OutputEventKind::InputEcho { .. }) {
                    dropped.dropped_visible_events =
                        dropped.dropped_visible_events.saturating_add(1);
                }
            }
            self.chunks.clear();
            self.line_ends.clear();
            self.events.clear();
            self.start_offset = self.end_offset;
            self.buffered_bytes = 0;
            self.buffered_event_bytes = 0;
            return dropped;
        }

        let _ = self.drop_input_echoes_for_room(needed_bytes, capacity_bytes);

        while self.total_buffered_bytes().saturating_add(needed_bytes) > capacity_bytes {
            if !self.chunks.is_empty() {
                let before = self.start_offset;
                let Some(front) = self.chunks.front() else {
                    break;
                };
                let front_len: u64 = front.range.len().try_into().unwrap_or(u64::MAX);
                let front_end = front.start_offset.saturating_add(front_len);
                let target = front_end.max(self.start_offset.saturating_add(1));
                self.trim_to_offset(target);
                dropped.dropped_bytes = dropped
                    .dropped_bytes
                    .saturating_add(self.start_offset.saturating_sub(before));
                continue;
            }

            if !self.events.is_empty() {
                if self.pop_front_event() {
                    dropped.dropped_visible_events =
                        dropped.dropped_visible_events.saturating_add(1);
                }
                continue;
            }

            break;
        }

        dropped
    }

    fn trim_to_offset(&mut self, offset: u64) {
        let offset = offset.min(self.end_offset);
        if offset <= self.start_offset {
            return;
        }

        while let Some(front) = self.chunks.front_mut() {
            let front_len: u64 = front.range.len().try_into().unwrap_or(u64::MAX);
            let front_end = front.start_offset.saturating_add(front_len);

            if front_end <= offset {
                let consumed = self.chunks.pop_front().unwrap();
                let consumed_len = consumed.range.len();
                self.start_offset = front_end;
                self.buffered_bytes = self.buffered_bytes.saturating_sub(consumed_len);
            } else if front.start_offset < offset {
                let delta_u64 = offset.saturating_sub(front.start_offset);
                let delta: usize = (delta_u64 as usize).min(front.range.len());
                front.start_offset = front.start_offset.saturating_add(delta as u64);
                front.range.start = front.range.start.saturating_add(delta);
                self.start_offset = offset;
                self.buffered_bytes = self.buffered_bytes.saturating_sub(delta);
            } else {
                self.start_offset = offset;
            }

            self.cleanup_front();

            if self.start_offset >= offset {
                break;
            }
        }

        if self.chunks.is_empty() {
            self.start_offset = offset;
            self.cleanup_front();
        }
    }

    fn trim_to_offset_consuming_boundary_events(
        &mut self,
        offset: u64,
        boundary_events_consumed: usize,
    ) {
        let offset = offset.min(self.end_offset);
        if offset < self.start_offset {
            return;
        }
        if offset == self.start_offset {
            self.cleanup_front_consuming_boundary_events(boundary_events_consumed);
            return;
        }

        while let Some(front) = self.chunks.front_mut() {
            let front_len: u64 = front.range.len().try_into().unwrap_or(u64::MAX);
            let front_end = front.start_offset.saturating_add(front_len);

            if front_end <= offset {
                let consumed = self.chunks.pop_front().unwrap();
                let consumed_len = consumed.range.len();
                self.start_offset = front_end;
                self.buffered_bytes = self.buffered_bytes.saturating_sub(consumed_len);
            } else if front.start_offset < offset {
                let delta_u64 = offset.saturating_sub(front.start_offset);
                let delta: usize = (delta_u64 as usize).min(front.range.len());
                front.start_offset = front.start_offset.saturating_add(delta as u64);
                front.range.start = front.range.start.saturating_add(delta);
                self.start_offset = offset;
                self.buffered_bytes = self.buffered_bytes.saturating_sub(delta);
            } else {
                self.start_offset = offset;
            }

            self.cleanup_front_before_boundary();

            if self.start_offset >= offset {
                break;
            }
        }

        if self.chunks.is_empty() {
            self.start_offset = offset;
            self.cleanup_front_before_boundary();
        }
        self.cleanup_front_consuming_boundary_events(boundary_events_consumed);
    }

    fn retained_end_offset(&self) -> u64 {
        self.chunks
            .back()
            .map(|chunk| chunk.start_offset.saturating_add(chunk.range.len() as u64))
            .unwrap_or(self.start_offset)
            .max(self.start_offset)
    }

    fn cleanup_front_before_boundary(&mut self) {
        while matches!(self.line_ends.front(), Some(line_end) if *line_end <= self.start_offset) {
            let _ = self.line_ends.pop_front();
        }
        while matches!(self.events.front(), Some(event) if event.offset < self.start_offset) {
            if self.pop_front_event() {
                // Dropping due to consumer progress; not tracked as truncation.
            }
        }
    }

    fn cleanup_front(&mut self) {
        self.cleanup_front_before_boundary();
        while matches!(self.events.front(), Some(event) if event.offset <= self.start_offset) {
            if self.pop_front_event() {
                // Dropping due to consumer progress; not tracked as truncation.
            }
        }
    }

    fn cleanup_front_consuming_boundary_events(&mut self, boundary_events_consumed: usize) {
        self.cleanup_front_before_boundary();
        for _ in 0..boundary_events_consumed {
            if !matches!(self.events.front(), Some(event) if event.offset == self.start_offset) {
                break;
            }
            if self.pop_front_event() {
                // Dropping due to consumer progress; not tracked as truncation.
            }
        }
    }

    fn cleanup_back(&mut self) {
        let retained_end = self.retained_end_offset();
        while matches!(self.line_ends.back(), Some(line_end) if *line_end > retained_end) {
            let _ = self.line_ends.pop_back();
        }
        while matches!(self.events.back(), Some(event) if event.offset > retained_end) {
            let _ = self.pop_back_event();
        }
    }
}

#[derive(Default, Clone, Copy)]
struct DropStats {
    dropped_bytes: u64,
    dropped_visible_events: usize,
}

impl DropStats {
    fn dropped_visible(self) -> bool {
        self.dropped_bytes > 0 || self.dropped_visible_events > 0
    }
}

fn event_size_bytes(kind: &OutputEventKind) -> usize {
    match kind {
        OutputEventKind::Image {
            data,
            mime_type,
            id,
            is_new: _,
        } => data
            .len()
            .saturating_add(mime_type.len())
            .saturating_add(id.len())
            .saturating_add(32),
        OutputEventKind::Text { text, .. } | OutputEventKind::InputEcho { text } => {
            text.len().saturating_add(16)
        }
        OutputEventKind::InputWait
        | OutputEventKind::RequestBoundary
        | OutputEventKind::SessionEnd => 0,
    }
}

fn omission_notice_kind() -> OutputEventKind {
    OutputEventKind::Text {
        text: OUTPUT_OMISSION_NOTICE.to_string(),
        is_stderr: false,
        origin: ContentOrigin::Server,
    }
}

fn assemble_bytes(slices: &[OutputSlice]) -> Vec<u8> {
    if slices.is_empty() {
        return Vec::new();
    }

    if slices.len() == 1 {
        let slice = &slices[0];
        return slice.bytes[slice.range.clone()].to_vec();
    }

    let total_bytes: usize = slices.iter().map(|slice| slice.range.len()).sum();
    let mut bytes = Vec::with_capacity(total_bytes);
    for slice in slices {
        bytes.extend_from_slice(&slice.bytes[slice.range.clone()]);
    }
    bytes
}

fn trim_range_to_complete_utf8(range: &mut OutputRange) -> u64 {
    let flushable_len = flushable_prefix_len(&range.bytes);
    if flushable_len == range.bytes.len() {
        return range.end_offset;
    }
    range.bytes.truncate(flushable_len);
    let consumed_end = range.start_offset.saturating_add(flushable_len as u64);
    range.end_offset = consumed_end;
    range.events.retain(|event| event.offset <= consumed_end);
    range.text_spans.retain_mut(|span| {
        if span.start_byte >= flushable_len {
            return false;
        }
        span.end_byte = span.end_byte.min(flushable_len);
        span.start_byte < span.end_byte
    });
    consumed_end
}

fn output_range_boundary_event_count(range: &OutputRange, offset: u64) -> usize {
    range
        .events
        .iter()
        .filter(|event| event.offset == offset)
        .count()
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
    offset
}

fn output_ring_append_chunk_len(capacity_bytes: usize) -> usize {
    capacity_bytes
        .max(1)
        .saturating_div(16)
        .clamp(1, OUTPUT_RING_APPEND_CHUNK_MAX_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn output_ring_truncates_instead_of_blocking() {
        let ring = OutputRing::with_capacity(64);
        let payload = (0..200u8).collect::<Vec<_>>();
        ring.append_bytes(&payload, false, ContentOrigin::Worker);

        let end = ring.end_offset();
        let range = ring.read_range(0, end);
        assert!(
            range.start_offset > 0,
            "expected some output to be truncated"
        );
        assert!(
            range.bytes.len() <= 64,
            "buffered bytes should not exceed capacity"
        );
    }

    #[test]
    fn clear_waits_for_in_flight_timeline_append() {
        let timeline = OutputTimeline::with_capacity(64);
        let output = timeline.buffer();
        let payload = vec![b'x'; 2 * 1024 * 1024];
        let payload_len = payload.len() as u64;

        std::thread::scope(|scope| {
            let writer = timeline.clone();
            let handle = scope.spawn(move || {
                writer.append_text(&payload, false, ContentOrigin::Worker);
            });

            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let end_offset = output.end_offset().expect("output ring should exist");
                if end_offset > 0 && end_offset < payload_len {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "expected to observe an in-flight append before it completed"
                );
                std::thread::yield_now();
            }

            timeline.clear();
            handle.join().expect("append thread should not panic");
        });

        assert_eq!(
            output.end_offset(),
            Some(0),
            "clear should not leave stale bytes from an in-flight append"
        );
    }

    #[test]
    fn output_ring_truncates_old_events() {
        let ring = OutputRing::with_capacity(128);
        ring.append_bytes(b"hello\n", false, ContentOrigin::Worker);
        for idx in 0..10 {
            let data = "x".repeat(80);
            ring.append_event(
                ring.end_offset(),
                OutputEventKind::Image {
                    id: format!("plot-{idx}"),
                    data,
                    mime_type: format!("image/{idx}"),
                    is_new: true,
                },
            );
        }
        let end = ring.end_offset();
        let range = ring.read_range(0, end);
        assert!(
            range.events.len() < 10,
            "expected oldest events to be dropped to stay within capacity"
        );
    }

    #[test]
    fn output_ring_emits_truncation_notice_event() {
        let ring = OutputRing::with_capacity(128);
        ring.append_bytes(b"header\n", false, ContentOrigin::Worker);
        let payload = vec![b'x'; 512];
        ring.append_bytes(&payload, false, ContentOrigin::Worker);

        let end = ring.end_offset();
        let range = ring.read_range(0, end);
        let truncation_events: Vec<&OutputEvent> = range
            .events
            .iter()
            .filter(|event| match &event.kind {
                OutputEventKind::Text { text, .. } => text.contains("output truncated"),
                _ => false,
            })
            .collect();
        assert!(
            !truncation_events.is_empty(),
            "expected a truncation notice event when output is dropped"
        );
        let last = range.events.last().expect("events should be present");
        match &last.kind {
            OutputEventKind::Text { text, .. } => {
                assert!(
                    text.contains("output truncated"),
                    "expected truncation notice to be last event"
                );
            }
            _ => panic!("expected truncation notice as the last event"),
        }
    }

    #[test]
    fn append_event_clamps_offset_after_truncation() {
        let ring = OutputRing::with_capacity(64);
        ring.append_bytes(&[b'a'; 64], false, ContentOrigin::Worker);
        ring.append_bytes(&[b'b'; 128], false, ContentOrigin::Worker);
        ring.append_event(
            0,
            OutputEventKind::Image {
                id: "plot-1".to_string(),
                data: "img".to_string(),
                mime_type: "image/png".to_string(),
                is_new: true,
            },
        );

        let end = ring.end_offset();
        let range = ring.read_range(0, end);
        let image_event = range
            .events
            .iter()
            .find(|event| matches!(event.kind, OutputEventKind::Image { .. }))
            .expect("expected an image event");
        assert!(
            image_event.offset >= range.start_offset,
            "expected event offset to be clamped into retained range"
        );
    }

    #[test]
    fn head_retention_marks_visible_events_dropped_at_capacity() {
        let timeline = OutputTimeline::with_head_retention_capacity(128);
        let output = timeline.buffer();

        timeline.append_text(&[b'x'; 120], false, ContentOrigin::Worker);
        timeline.append_image(
            "plot-1".to_string(),
            "image/png".to_string(),
            "image-data".to_string(),
            true,
            0,
        );

        let range = output.read_range(
            0,
            output
                .end_offset()
                .expect("output should have an end offset"),
        );
        assert!(
            range.events.iter().any(|event| matches!(
                &event.kind,
                OutputEventKind::Text {
                    text,
                    origin: ContentOrigin::Server,
                    ..
                } if text.contains("output omitted")
            )),
            "expected a server omission notice when a later visible event cannot fit"
        );
    }

    #[test]
    fn head_retention_marks_input_echo_dropped_for_later_text() {
        let timeline = OutputTimeline::with_head_retention_capacity(160);
        let output = timeline.buffer();
        let line = format!("{}\n", "a".repeat(96));

        timeline.append_input_echo("p> ", &line);
        timeline.append_text(&[b'x'; 80], false, ContentOrigin::Worker);

        let range = output.read_range(
            0,
            output
                .end_offset()
                .expect("output should have an end offset"),
        );
        assert!(
            range.events.iter().any(|event| matches!(
                &event.kind,
                OutputEventKind::Text {
                    text,
                    origin: ContentOrigin::Server,
                    ..
                } if text.contains("output omitted")
            )),
            "expected a server omission notice when a hidden input echo is dropped for later text"
        );
        assert_eq!(
            range.bytes.len(),
            80,
            "later visible text should be retained after the hidden input echo is dropped"
        );
    }

    #[test]
    fn head_retention_keeps_text_that_fits_full_capacity_without_omission() {
        let notice_bytes = event_size_bytes(&omission_notice_kind());
        let capacity = notice_bytes.saturating_add(64);
        let timeline = OutputTimeline::with_head_retention_capacity(capacity);
        let output = timeline.buffer();
        let payload = vec![b'x'; capacity];

        timeline.append_text(&payload, false, ContentOrigin::Worker);

        let range = output.read_range(
            0,
            output
                .end_offset()
                .expect("output should have an end offset"),
        );
        assert_eq!(
            range.bytes.len(),
            capacity,
            "text that fits the full head-retention capacity should be retained"
        );
        assert!(
            range.events.iter().all(|event| !matches!(
                &event.kind,
                OutputEventKind::Text { text, .. } if text.contains("output omitted")
            )),
            "did not expect an omission notice when all output fits"
        );
    }

    #[test]
    fn boundary_event_appended_after_snapshot_survives_advance() {
        let timeline = OutputTimeline::with_capacity(1024);
        let output = timeline.buffer();
        output.start_capture();

        timeline.append_text(b"ready", false, ContentOrigin::Worker);
        let start = output
            .current_offset()
            .expect("output capture should start");
        let end = output
            .end_offset()
            .expect("output should have an end offset");
        let range = output.read_range(start, end);

        timeline.append_image(
            "plot-1".to_string(),
            "image/png".to_string(),
            "image-data".to_string(),
            true,
            0,
        );
        let boundary_events_consumed = output_range_boundary_event_count(&range, range.end_offset);
        output.advance_offset_to_with_rendered_text_and_boundary_events(
            range.end_offset,
            None,
            boundary_events_consumed,
        );

        let formatted = output.drain_formatted(ProjectionMode::Bundle, true);
        assert!(
            formatted.contents.iter().any(|content| matches!(
                content,
                WorkerContent::ContentImage { id, .. } if id == "plot-1"
            )),
            "expected the boundary image appended after the snapshot to survive"
        );
    }

    #[test]
    fn timeline_reports_unflushable_utf8_tail_until_raw_bytes_complete() {
        let timeline = OutputTimeline::with_capacity(1024);
        let output = timeline.buffer();
        output.start_capture();

        assert!(
            !timeline.has_unflushable_utf8_tail(),
            "fresh timeline should not report a pending UTF-8 tail"
        );

        timeline.append_text(&[0xC3], false, ContentOrigin::Worker);

        assert!(
            timeline.has_unflushable_utf8_tail(),
            "split UTF-8 lead byte should stay visible to settle logic while held off-ring"
        );
        assert!(
            !output.has_pending_output(),
            "incomplete UTF-8 tail should not be materialized before it is completed or sealed"
        );

        timeline.append_input_wait();

        assert!(
            timeline.has_unflushable_utf8_tail(),
            "input_wait markers should not seal an otherwise completable raw UTF-8 tail"
        );

        timeline.append_text(&[0xA9, b'\n'], false, ContentOrigin::Worker);

        assert!(
            !timeline.has_unflushable_utf8_tail(),
            "completed UTF-8 sequence should clear the pending-tail state"
        );

        let formatted = output.drain_formatted(ProjectionMode::Bundle, true);
        let text = formatted
            .contents
            .iter()
            .filter_map(|content| match content {
                WorkerContent::ContentText { text, .. } => Some(text.as_str()),
                WorkerContent::ContentImage { .. } => None,
            })
            .collect::<String>();
        assert!(
            text.contains("é\n"),
            "completed split UTF-8 bytes should render as text, got: {text:?}"
        );
        assert!(
            !text.contains("\\xC3") && !text.contains("\\xA9"),
            "completed split UTF-8 bytes should not be escaped, got: {text:?}"
        );
    }

    #[test]
    fn preserves_control_delim_bytes_in_output() {
        let ring = OutputRing::with_capacity(64);
        ring.append_bytes(&[0x1e, b'a', 0x1e], false, ContentOrigin::Worker);
        let end = ring.end_offset();
        let range = ring.read_range(0, end);
        assert!(
            range.bytes.contains(&0x1e),
            "expected to preserve 0x1e bytes in captured output"
        );
        assert!(range.events.is_empty(), "did not expect any events");
    }

    fn next_u32(seed: &mut u32) -> u32 {
        *seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        *seed
    }

    #[test]
    fn output_ring_capacity_invariants_hold_under_random_appends() {
        let capacity = 256usize;
        let ring = OutputRing::with_capacity(capacity);
        let mut seed = 1u32;
        let mut last_start = 0u64;

        for _ in 0..500 {
            let value = next_u32(&mut seed);
            if value.is_multiple_of(3) {
                let len = (value % 512) as usize;
                ring.append_bytes(&vec![b'x'; len], false, ContentOrigin::Worker);
            } else {
                let len = (value % 256) as usize;
                let text = "x".repeat(len);
                ring.append_event(
                    ring.end_offset(),
                    OutputEventKind::Text {
                        text,
                        is_stderr: false,
                        origin: ContentOrigin::Worker,
                    },
                );
            }

            let end = ring.end_offset();
            let range = ring.read_range(0, end);
            let events_bytes: usize = range
                .events
                .iter()
                .map(|event| event_size_bytes(&event.kind))
                .sum();

            assert!(
                range.bytes.len().saturating_add(events_bytes) <= capacity,
                "buffered content exceeded capacity"
            );
            assert!(
                range.start_offset >= last_start,
                "start offset should be monotonic"
            );
            assert_eq!(range.end_offset, end, "range end should match ring end");
            for event in &range.events {
                assert!(
                    event.offset >= range.start_offset && event.offset <= range.end_offset,
                    "event offset outside retained range"
                );
            }
            last_start = range.start_offset;
        }
    }
}
