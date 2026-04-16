#![cfg_attr(not(target_family = "unix"), allow(dead_code))]

use std::collections::VecDeque;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::worker_protocol::ContentOrigin;

static OUTPUT_RING: OnceLock<Arc<OutputRing>> = OnceLock::new();
static LAST_REPLY_MARKER_OFFSET: AtomicU64 = AtomicU64::new(u64::MAX);

pub(crate) const OUTPUT_RING_CAPACITY_BYTES: usize = 2 * 1024 * 1024;
const STDERR_PREFIX: &[u8] = b"stderr: ";

pub(crate) fn ensure_output_ring(capacity_bytes: usize) -> Arc<OutputRing> {
    OUTPUT_RING
        .get_or_init(|| Arc::new(OutputRing::new(capacity_bytes)))
        .clone()
}

fn output_ring_opt() -> Option<Arc<OutputRing>> {
    OUTPUT_RING.get().cloned()
}

pub(crate) fn reset_output_ring() {
    if let Some(ring) = OUTPUT_RING.get() {
        ring.reset();
    }
}

pub(crate) fn set_last_reply_marker_offset(offset: u64) {
    LAST_REPLY_MARKER_OFFSET.store(offset, Ordering::SeqCst);
}

pub(crate) fn update_last_reply_marker_offset_max(offset: u64) {
    let mut current = LAST_REPLY_MARKER_OFFSET.load(Ordering::SeqCst);
    loop {
        if current != u64::MAX && current >= offset {
            return;
        }
        match LAST_REPLY_MARKER_OFFSET.compare_exchange(
            current,
            offset,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => return,
            Err(next) => current = next,
        }
    }
}

fn last_reply_marker_offset() -> Option<u64> {
    let value = LAST_REPLY_MARKER_OFFSET.load(Ordering::SeqCst);
    (value != u64::MAX).then_some(value)
}

pub(crate) fn reset_last_reply_marker_offset() {
    LAST_REPLY_MARKER_OFFSET.store(u64::MAX, Ordering::SeqCst);
}

#[cfg(test)]
pub(crate) fn output_ring_test_mutex() -> &'static Mutex<()> {
    static TEST_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
    TEST_MUTEX.get_or_init(|| Mutex::new(()))
}

#[derive(Clone)]
pub(crate) struct OutputTimeline {
    ring: Arc<OutputRing>,
}

impl OutputTimeline {
    pub(crate) fn new(ring: Arc<OutputRing>) -> Self {
        Self { ring }
    }

    pub(crate) fn append_text(&self, bytes: &[u8], is_stderr: bool, origin: ContentOrigin) {
        if bytes.is_empty() {
            return;
        }
        if !is_stderr {
            self.ring.append_bytes(bytes, false, origin);
            return;
        }

        // Keep stderr attribution in-band (as text) while ensuring the prefix starts on a new
        // line. This avoids confusing merges like `> xstderr: ...` when stdout/stderr reader
        // threads append chunks out-of-order.
        //
        // NOTE: We always insert a leading newline once any output has been captured. This is
        // conservative (it can introduce blank lines), but it prevents `stderr:` from being
        // spliced into the middle of a partially-read stdout line.
        let mut payload = Vec::with_capacity(STDERR_PREFIX.len() + bytes.len() + 1);
        if !self.ring.is_empty() {
            payload.push(b'\n');
        }
        payload.extend_from_slice(STDERR_PREFIX);
        payload.extend_from_slice(bytes);
        self.ring.append_bytes(&payload, true, origin);
    }

    pub(crate) fn append_image(
        &self,
        id: String,
        mime_type: String,
        data: String,
        is_new: bool,
        readline_results_seen: usize,
    ) {
        self.ring
            .append_image_event(id, mime_type, data, is_new, readline_results_seen);
    }
}

#[derive(Clone, Default)]
pub(crate) struct OutputBuffer {
    cursor: Arc<Mutex<OutputCursor>>,
}

#[derive(Default)]
struct OutputCursor {
    offset: Option<u64>,
}

impl OutputBuffer {
    pub(crate) fn current_offset(&self) -> Option<u64> {
        let guard = self.cursor.lock().unwrap();
        guard.offset
    }

    pub(crate) fn end_offset(&self) -> Option<u64> {
        output_ring_opt().map(|ring| ring.end_offset())
    }

    pub(crate) fn saw_stderr_in_range(&self, start_offset: u64, end_offset: u64) -> bool {
        let Some(ring) = output_ring_opt() else {
            return false;
        };
        ring.saw_stderr_in_range(start_offset, end_offset)
    }

    pub(crate) fn read_range(&self, start_offset: u64, end_offset: u64) -> OutputRange {
        let Some(ring) = output_ring_opt() else {
            return OutputRange::empty(start_offset, end_offset);
        };
        ring.read_range(start_offset, end_offset)
    }

    pub(crate) fn start_capture(&self) {
        {
            let guard = self.cursor.lock().unwrap();
            if guard.offset.is_some() {
                return;
            }
        }

        let Some(ring) = output_ring_opt() else {
            return;
        };
        let start_offset = ring.start_offset();

        let mut guard = self.cursor.lock().unwrap();
        if guard.offset.is_none() {
            guard.offset = Some(start_offset);
        }
    }

    fn read_offset_with_ring(&self) -> Option<(u64, Arc<OutputRing>)> {
        let ring = output_ring_opt()?;
        let offset = {
            let guard = self.cursor.lock().unwrap();
            guard.offset?
        };
        Some((offset, ring))
    }

    pub(crate) fn advance_offset_to(&self, offset: u64) {
        let mut guard = self.cursor.lock().unwrap();
        guard.offset = Some(offset);
        drop(guard);
        if let Some(ring) = output_ring_opt() {
            ring.consume_to(offset);
        }
    }

    pub fn has_pending_output(&self) -> bool {
        let Some((offset, ring)) = self.read_offset_with_ring() else {
            return false;
        };
        ring.end_offset() > offset || ring.has_events_at_or_after(offset)
    }

    pub fn pending_output_since_last_reply(&self) -> bool {
        let Some((offset, ring)) = self.read_offset_with_ring() else {
            return false;
        };
        if ring.end_offset() <= offset && !ring.has_events_at_or_after(offset) {
            return false;
        }
        let Some(last_reply_marker) = last_reply_marker_offset() else {
            return false;
        };
        offset >= last_reply_marker
    }
}

pub(crate) struct OutputRing {
    capacity_bytes: usize,
    inner: Mutex<OutputRingInner>,
}

struct OutputRingInner {
    chunks: VecDeque<OutputChunk>,
    line_ends: VecDeque<u64>,
    events: VecDeque<OutputEvent>,
    start_offset: u64,
    end_offset: u64,
    buffered_bytes: usize,
    buffered_event_bytes: usize,
}

struct OutputChunk {
    start_offset: u64,
    bytes: Arc<[u8]>,
    range: Range<usize>,
    is_stderr: bool,
    origin: ContentOrigin,
}

struct OutputSlice {
    bytes: Arc<[u8]>,
    range: Range<usize>,
    is_stderr: bool,
    origin: ContentOrigin,
}

#[derive(Clone, Debug)]
pub(crate) struct OutputTextSpan {
    pub start_byte: usize,
    pub end_byte: usize,
    pub is_stderr: bool,
    pub origin: ContentOrigin,
}

pub(crate) struct OutputRange {
    pub start_offset: u64,
    pub end_offset: u64,
    pub bytes: Vec<u8>,
    pub events: Vec<OutputEvent>,
    pub text_spans: Vec<OutputTextSpan>,
}

impl OutputRange {
    fn empty(start_offset: u64, end_offset: u64) -> Self {
        Self {
            start_offset,
            end_offset,
            bytes: Vec::new(),
            events: Vec::new(),
            text_spans: Vec::new(),
        }
    }
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
        readline_results_seen: usize,
    },
    Text {
        text: String,
        is_stderr: bool,
        origin: ContentOrigin,
    },
}

struct CollectedRange {
    slices: Vec<OutputSlice>,
    events: Vec<OutputEvent>,
    start_offset: u64,
    end_offset: u64,
}

impl OutputRing {
    fn new(capacity_bytes: usize) -> Self {
        Self {
            capacity_bytes,
            inner: Mutex::new(OutputRingInner {
                chunks: VecDeque::new(),
                line_ends: VecDeque::new(),
                events: VecDeque::new(),
                start_offset: 0,
                end_offset: 0,
                buffered_bytes: 0,
                buffered_event_bytes: 0,
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

    pub(crate) fn append_bytes(&self, bytes: &[u8], is_stderr: bool, origin: ContentOrigin) {
        if bytes.is_empty() {
            return;
        }

        let mut dropped_any = false;
        let mut remaining = bytes;
        while !remaining.is_empty() {
            let chunk_len = remaining.len().min(self.capacity_bytes.max(1));
            let (head, tail) = remaining.split_at(chunk_len);
            remaining = tail;

            let newline_indices: Vec<usize> = head
                .iter()
                .enumerate()
                .filter_map(|(idx, byte)| (*byte == b'\n').then_some(idx))
                .collect();
            let bytes: Arc<[u8]> = head.to_vec().into();
            let bytes_len = bytes.len();

            let mut guard = self.inner.lock().unwrap();
            let dropped = guard.make_room_for(bytes_len, self.capacity_bytes);
            dropped_any |= dropped.dropped_any();

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
            });
        }

        if dropped_any {
            self.append_truncation_notice(self.end_offset(), 0);
        }
    }

    pub(crate) fn end_offset(&self) -> u64 {
        self.inner.lock().unwrap().end_offset
    }

    fn is_empty(&self) -> bool {
        let guard = self.inner.lock().unwrap();
        guard.chunks.is_empty() && guard.events.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn append_event(&self, offset: u64, kind: OutputEventKind) {
        let mut guard = self.inner.lock().unwrap();
        let event_bytes = event_size_bytes(&kind);
        if event_bytes > self.capacity_bytes {
            return;
        }

        let dropped = guard.make_room_for(event_bytes, self.capacity_bytes);
        let mut event_offset = offset.max(guard.start_offset);
        if dropped.dropped_any() {
            self.append_truncation_notice_locked(&mut guard, event_offset, event_bytes);
            event_offset = event_offset.max(guard.start_offset);
        }
        guard.buffered_event_bytes = guard.buffered_event_bytes.saturating_add(event_bytes);
        guard.events.push_back(OutputEvent {
            offset: event_offset,
            kind,
        });
    }

    pub(crate) fn append_image_event(
        &self,
        id: String,
        mime_type: String,
        data: String,
        is_new: bool,
        readline_results_seen: usize,
    ) {
        let mut guard = self.inner.lock().unwrap();
        let kind = OutputEventKind::Image {
            id,
            data,
            mime_type,
            is_new,
            readline_results_seen,
        };
        let event_bytes = event_size_bytes(&kind);
        if event_bytes > self.capacity_bytes {
            return;
        }

        let dropped = guard.make_room_for(event_bytes, self.capacity_bytes);
        let mut event_offset = guard.end_offset.max(guard.start_offset);
        if dropped.dropped_any() {
            self.append_truncation_notice_locked(&mut guard, event_offset, event_bytes);
            event_offset = event_offset.max(guard.start_offset);
        }
        guard.buffered_event_bytes = guard.buffered_event_bytes.saturating_add(event_bytes);
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
                && last.end_byte == start_byte
            {
                last.end_byte = end_byte;
            } else {
                text_spans.push(OutputTextSpan {
                    start_byte,
                    end_byte,
                    is_stderr: slice.is_stderr,
                    origin: slice.origin,
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

    fn has_events_at_or_after(&self, offset: u64) -> bool {
        let guard = self.inner.lock().unwrap();
        guard.events.iter().any(|event| event.offset >= offset)
    }

    fn consume_to(&self, offset: u64) {
        let mut guard = self.inner.lock().unwrap();
        let offset = offset.min(guard.end_offset);
        if offset <= guard.start_offset {
            return;
        }
        guard.trim_to_offset(offset);
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

    fn append_truncation_notice(&self, offset: u64, extra_bytes: usize) {
        let mut guard = self.inner.lock().unwrap();
        self.append_truncation_notice_locked(&mut guard, offset, extra_bytes);
    }

    fn append_truncation_notice_locked(
        &self,
        guard: &mut OutputRingInner,
        offset: u64,
        extra_bytes: usize,
    ) {
        let notice_kind = OutputEventKind::Text {
            text: "[repl] output truncated (older output dropped)\n".to_string(),
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
        guard.buffered_event_bytes = guard.buffered_event_bytes.saturating_add(notice_bytes);
        guard.events.push_back(OutputEvent {
            offset: notice_offset,
            kind: notice_kind,
        });
    }
}

impl OutputRingInner {
    fn total_buffered_bytes(&self) -> usize {
        self.buffered_bytes
            .saturating_add(self.buffered_event_bytes)
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

    fn make_room_for(&mut self, needed_bytes: usize, capacity_bytes: usize) -> DropStats {
        let mut dropped = DropStats::default();
        if needed_bytes >= capacity_bytes {
            // If a single chunk consumes the full capacity, drop everything else.
            dropped.dropped_bytes = self.end_offset.saturating_sub(self.start_offset);
            dropped.dropped_events = self.events.len();
            self.chunks.clear();
            self.line_ends.clear();
            self.events.clear();
            self.start_offset = self.end_offset;
            self.buffered_bytes = 0;
            self.buffered_event_bytes = 0;
            return dropped;
        }

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
                    dropped.dropped_events = dropped.dropped_events.saturating_add(1);
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

    fn cleanup_front(&mut self) {
        while matches!(self.line_ends.front(), Some(line_end) if *line_end <= self.start_offset) {
            let _ = self.line_ends.pop_front();
        }
        while matches!(self.events.front(), Some(event) if event.offset <= self.start_offset) {
            if self.pop_front_event() {
                // Dropping due to consumer progress; not tracked as truncation.
            }
        }
    }
}

#[derive(Default, Clone, Copy)]
struct DropStats {
    dropped_bytes: u64,
    dropped_events: usize,
}

impl DropStats {
    fn dropped_any(self) -> bool {
        self.dropped_bytes > 0 || self.dropped_events > 0
    }
}

fn event_size_bytes(kind: &OutputEventKind) -> usize {
    match kind {
        OutputEventKind::Image {
            data,
            mime_type,
            id,
            is_new: _,
            readline_results_seen: _,
        } => data
            .len()
            .saturating_add(mime_type.len())
            .saturating_add(id.len())
            .saturating_add(32),
        OutputEventKind::Text { text, .. } => text.len().saturating_add(16),
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

#[cfg(test)]
mod tests {
    use super::*;

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
                    readline_results_seen: 0,
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
                readline_results_seen: 0,
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
