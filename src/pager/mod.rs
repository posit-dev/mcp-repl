use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use memchr::{memchr, memchr_iter, memchr2, memmem};

use crate::output_capture::{OutputBuffer, OutputEventKind, OutputRange};
use crate::worker_protocol::{WorkerContent, WorkerErrorCode, WorkerReply};

mod command;
mod merge;
mod presentation;
mod ranges;
mod search;

use command::{MatchSpec, PagerCommand, SearchPattern, SeekSpec, TailSpec};
use presentation::{
    elision_marker, footer_min, gap_marker_if_needed, non_command_input_message, pager_help_text,
    position_marker, truncate_with_ellipsis,
};
use ranges::{RangeSet, RangeSpan};
use search::{
    HitState, SearchMode, take_hits_next, take_matches, take_search, take_search_next,
    where_in_buffer,
};

pub(crate) const PAGER_PAGE_CHARS_ENV: &str = "MCP_CONSOLE_PAGER_PAGE_CHARS";
const PAGER_PAGE_BYTES_ENV: &str = "MCP_CONSOLE_PAGER_PAGE_BYTES";

const DEFAULT_PAGER_PAGE_CHARS: u64 = 3_500;
const MIN_PAGER_PAGE_CHARS: u64 = 64;
const INPUT_ECHO_MAX_CHARS: usize = 40;
const INPUT_ECHO_TRUNC_SUFFIX: &str = ".... [TRUNCATED]";
const DEFAULT_MATCH_LIMIT: usize = 50;
const MAX_MATCH_LIMIT: usize = 500;
const DEFAULT_MATCH_CONTEXT: usize = 0;
const MAX_MATCH_CONTEXT: usize = 5;
const MATCH_LINE_MAX_BYTES: usize = 200;
const MATCH_BREADCRUMB_MAX_BYTES: usize = 160;
pub(crate) const MAX_IMAGES_PER_PAGE: usize = 2;
pub(crate) const IMAGE_EQUIV_CHARS: u64 = 800;

#[derive(Debug, Clone)]
pub(crate) struct PagerBuffer {
    bytes: Vec<u8>,
    char_to_byte: Vec<usize>,
    line_ends: Vec<u64>,
    cursor: u64,
    events: Vec<PagerEvent>,
    source_end: u64,
}

#[derive(Debug, Clone)]
struct PagerEvent {
    offset: u64,
    kind: OutputEventKind,
}

impl merge::EventView for crate::output_capture::OutputEvent {
    fn offset(&self) -> u64 {
        self.offset
    }

    fn kind(&self) -> &OutputEventKind {
        &self.kind
    }
}

pub(crate) struct SnapshotPage {
    pub(crate) contents: Vec<WorkerContent>,
    pub(crate) pages_left: u64,
    pub(crate) buffer: Option<PagerBuffer>,
    pub(crate) last_range: Option<(u64, u64)>,
    pub(crate) last_range_end_byte: Option<u64>,
}

impl PagerBuffer {
    fn from_range(range: OutputRange) -> Self {
        let base_offset = range.start_offset;
        let end_offset = range.end_offset;
        let bytes = range.bytes;
        let char_to_byte = build_char_index(&bytes);
        let events = range
            .events
            .into_iter()
            .filter_map(|event| {
                if event.offset < base_offset || event.offset > end_offset {
                    return None;
                }
                Some(PagerEvent {
                    offset: char_offset_for_byte_index(
                        &char_to_byte,
                        event.offset.saturating_sub(base_offset) as usize,
                    ),
                    kind: event.kind,
                })
            })
            .collect();
        let line_ends = line_end_offsets(&bytes, &char_to_byte, 0);
        Self {
            bytes,
            char_to_byte,
            line_ends,
            cursor: 0,
            events,
            source_end: end_offset,
        }
    }

    /// Construct a pager buffer from already-collapsed text and event offsets expressed in byte
    /// indices within `bytes`.
    ///
    /// `source_end` tracks the worker output ring offset consumed for this buffer, so the pager
    /// can later append any new output that arrives while pager mode is active.
    pub(crate) fn from_bytes_and_events(
        bytes: Vec<u8>,
        events: Vec<(u64, OutputEventKind)>,
        source_end: u64,
    ) -> Self {
        let char_to_byte = build_char_index(&bytes);
        let line_ends = line_end_offsets(&bytes, &char_to_byte, 0);
        let events = events
            .into_iter()
            .filter_map(|(byte_offset, kind)| {
                let byte_offset: usize = byte_offset.try_into().unwrap_or(usize::MAX);
                if byte_offset > bytes.len() {
                    return None;
                }
                Some(PagerEvent {
                    offset: char_offset_for_byte_index(&char_to_byte, byte_offset),
                    kind,
                })
            })
            .collect();
        Self {
            bytes,
            char_to_byte,
            line_ends,
            cursor: 0,
            events,
            source_end,
        }
    }

    fn len(&self) -> u64 {
        char_len(&self.char_to_byte)
    }

    fn current_offset(&self) -> u64 {
        self.cursor
    }

    fn advance_offset_to(&mut self, offset: u64) {
        self.cursor = offset.min(self.len());
    }

    fn read_text_range(&self, start_offset: u64, end_offset: u64) -> String {
        let end_offset = end_offset.min(self.len());
        let start_offset = start_offset.min(end_offset);
        if start_offset >= end_offset {
            return String::new();
        }
        let start = self.byte_index_for_char_offset(start_offset);
        let end = self.byte_index_for_char_offset(end_offset);
        String::from_utf8_lossy(&self.bytes[start..end]).into_owned()
    }

    fn contents_for_range(&self, start_offset: u64, end_offset: u64) -> Vec<WorkerContent> {
        let end_offset = end_offset.min(self.len());
        let start_offset = start_offset.min(end_offset);
        if start_offset > end_offset {
            return Vec::new();
        }
        let start = self.byte_index_for_char_offset(start_offset);
        let end = self.byte_index_for_char_offset(end_offset);
        let bytes = &self.bytes[start..end];
        merge::merge_bytes_with_events(
            bytes,
            start as u64,
            end as u64,
            &self.events_in_byte_offsets(start_offset, end_offset),
            output_event_to_content,
        )
    }

    fn page_end_offset(&self, start_offset: u64, end_offset: u64, target_bytes: u64) -> u64 {
        let end_offset = end_offset.min(self.len());
        if start_offset >= end_offset {
            return end_offset;
        }

        let effective_start = start_offset.min(end_offset);
        let desired = effective_start.saturating_add(target_bytes).min(end_offset);
        // NOTE: `end_offset` is not necessarily the end of the full buffer. Callers can pass a
        // tighter bound (e.g. the current budget window). Even when `desired == end_offset`, we
        // still prefer snapping to a newline boundary (when available) to avoid splitting lines.

        let idx = self
            .line_ends
            .partition_point(|line_end| *line_end <= desired);
        let Some(line_end) = idx
            .checked_sub(1)
            .and_then(|idx| self.line_ends.get(idx))
            .copied()
        else {
            return desired;
        };

        if line_end <= effective_start {
            return desired;
        }

        // Always snap to the last newline within the requested window. This keeps pages
        // predictable and avoids splitting short, line-oriented outputs mid-line.
        line_end
    }

    fn last_line_end_before(&self, offset: u64) -> Option<u64> {
        let offset = offset.min(self.len());
        let idx = self
            .line_ends
            .partition_point(|line_end| *line_end <= offset);
        if idx == 0 {
            None
        } else {
            self.line_ends.get(idx.saturating_sub(1)).copied()
        }
    }

    fn find_next_bytes(&self, start_offset: u64, end_offset: u64, needle: &[u8]) -> Option<u64> {
        if needle.is_empty() {
            return None;
        }

        let end_offset = end_offset.min(self.len());
        let start_offset = start_offset.min(end_offset);
        if start_offset >= end_offset {
            return None;
        }

        let start = self.byte_index_for_char_offset(start_offset);
        let end = self.byte_index_for_char_offset(end_offset);
        let haystack = &self.bytes[start..end];
        if needle.len() > haystack.len() {
            return None;
        }

        if needle.len() == 1 {
            return memchr(needle[0], haystack)
                .map(|idx| self.char_offset_for_byte_index(start + idx));
        }

        memmem::find(haystack, needle).map(|idx| self.char_offset_for_byte_index(start + idx))
    }

    fn find_next_bytes_ascii_case_insensitive(
        &self,
        start_offset: u64,
        end_offset: u64,
        needle: &[u8],
    ) -> Option<u64> {
        if needle.is_empty() {
            return None;
        }

        let end_offset = end_offset.min(self.len());
        let start_offset = start_offset.min(end_offset);
        if start_offset >= end_offset {
            return None;
        }

        let start = self.byte_index_for_char_offset(start_offset);
        let end = self.byte_index_for_char_offset(end_offset);
        let haystack = &self.bytes[start..end];
        if needle.len() > haystack.len() {
            return None;
        }

        let needle_len = needle.len();
        let first = ascii_lower(needle[0]);
        let alt = if first.is_ascii_lowercase() {
            first.to_ascii_uppercase()
        } else {
            first
        };

        if needle_len == 1 {
            let pos = if first == alt {
                memchr(first, haystack)
            } else {
                memchr2(first, alt, haystack)
            };
            return pos.map(|idx| self.char_offset_for_byte_index(start + idx));
        }

        let mut offset = 0usize;
        while offset + needle_len <= haystack.len() {
            let remaining = &haystack[offset..];
            let pos = if first == alt {
                memchr(first, remaining)
            } else {
                memchr2(first, alt, remaining)
            }?;
            let idx = offset + pos;
            if idx + needle_len > haystack.len() {
                return None;
            }
            let window = &haystack[idx..idx + needle_len];
            if window
                .iter()
                .zip(needle.iter())
                .all(|(a, b)| ascii_lower(*a) == ascii_lower(*b))
            {
                return Some(self.char_offset_for_byte_index(start + idx));
            }
            offset = idx + 1;
        }

        None
    }

    fn find_next_bytes_with_options(
        &self,
        start_offset: u64,
        end_offset: u64,
        needle: &[u8],
        case_insensitive_ascii: bool,
    ) -> Option<u64> {
        if case_insensitive_ascii {
            self.find_next_bytes_ascii_case_insensitive(start_offset, end_offset, needle)
        } else {
            self.find_next_bytes(start_offset, end_offset, needle)
        }
    }

    fn tail_start_offset_for_lines(&self, end_offset: u64, lines: usize) -> u64 {
        if lines == 0 {
            return end_offset.min(self.len());
        }

        let end_offset = end_offset.min(self.len());
        if self.bytes.is_empty() {
            return end_offset;
        }

        let mut line_ends = self
            .line_ends
            .iter()
            .rev()
            .filter(|line_end| **line_end <= end_offset);
        let last_line_end = line_ends.next();
        let has_trailing_newline =
            matches!(last_line_end, Some(line_end) if *line_end == end_offset);

        // If the buffer ends in a newline, the last line is fully represented in `line_ends`,
        // so we need to skip the trailing newline boundary when computing the start offset.
        let needed_index = if has_trailing_newline {
            lines
        } else {
            lines.saturating_sub(1)
        };

        self.line_ends
            .iter()
            .rev()
            .filter(|line_end| **line_end <= end_offset)
            .nth(needed_index)
            .copied()
            .unwrap_or(0)
    }

    fn line_count(&self) -> usize {
        let mut count = self.line_ends.len();
        if !self.bytes.is_empty() && *self.bytes.last().unwrap_or(&b'\n') != b'\n' {
            count = count.saturating_add(1);
        }
        count
    }

    fn line_start_offset(&self, line: usize) -> Option<u64> {
        if line == 0 {
            return None;
        }
        if line == 1 {
            return Some(0);
        }
        self.line_ends.get(line.saturating_sub(2)).copied()
    }

    fn line_end_offset(&self, line: usize) -> Option<u64> {
        if line == 0 {
            return None;
        }
        if line <= self.line_ends.len() {
            return self.line_ends.get(line - 1).copied();
        }
        if line == self.line_count() {
            return Some(self.len());
        }
        None
    }

    fn line_range_offsets(&self, start_line: usize, end_line: usize) -> Option<(u64, u64)> {
        if start_line == 0 || end_line == 0 {
            return None;
        }
        let (start_line, end_line) = if start_line <= end_line {
            (start_line, end_line)
        } else {
            (end_line, start_line)
        };
        let start_offset = self.line_start_offset(start_line)?;
        let end_offset = self.line_end_offset(end_line)?;
        if start_offset >= end_offset {
            return None;
        }
        Some((start_offset, end_offset))
    }

    fn source_end_offset(&self) -> u64 {
        self.source_end
    }

    fn append_range(&mut self, range: OutputRange) {
        if range.start_offset > self.source_end {
            let gap = range.start_offset.saturating_sub(self.source_end);
            let notice = format!("[pager] output gap detected ({} bytes skipped)\n", gap);
            self.append_bytes(notice.as_bytes());
            self.source_end = range.start_offset;
        }

        if range.bytes.is_empty() && range.events.is_empty() {
            self.source_end = self.source_end.max(range.end_offset);
            return;
        }

        let base_offset = range.start_offset;
        let old_char_len = self.len();
        self.append_bytes(&range.bytes);

        let chunk_char_index = build_char_index(&range.bytes);
        for event in range.events {
            if event.offset < base_offset || event.offset > range.end_offset {
                continue;
            }
            let relative = event.offset.saturating_sub(base_offset) as usize;
            let char_offset = char_offset_for_byte_index(&chunk_char_index, relative);
            self.events.push(PagerEvent {
                offset: old_char_len.saturating_add(char_offset),
                kind: event.kind,
            });
        }

        self.source_end = range.end_offset.max(self.source_end);
    }

    fn append_bytes(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let old_byte_len = self.bytes.len();
        let old_char_len = self.len();
        self.bytes.extend_from_slice(bytes);

        let chunk_index = build_char_index(bytes);
        extend_char_index(&mut self.char_to_byte, &chunk_index, old_byte_len);
        self.line_ends
            .extend(line_end_offsets(bytes, &chunk_index, old_char_len));
    }

    fn byte_index_for_char_offset(&self, offset: u64) -> usize {
        byte_index_for_char_offset(&self.char_to_byte, offset, self.bytes.len())
    }

    fn char_offset_for_byte_index(&self, byte_index: usize) -> u64 {
        char_offset_for_byte_index(&self.char_to_byte, byte_index)
    }

    fn events_in_byte_offsets(&self, start_offset: u64, end_offset: u64) -> Vec<PagerEventByte> {
        let mut events = Vec::new();
        for event in self
            .events
            .iter()
            .filter(|event| event.offset >= start_offset && event.offset <= end_offset)
        {
            let byte_offset = self.byte_index_for_char_offset(event.offset) as u64;
            events.push(PagerEventByte {
                offset: byte_offset,
                kind: event.kind.clone(),
            });
        }
        events
    }

    fn image_offsets_in_range(&self, start_offset: u64, end_offset: u64, limit: usize) -> Vec<u64> {
        if limit == 0 {
            return Vec::new();
        }
        let end_offset = end_offset.min(self.len());
        let start_offset = start_offset.min(end_offset);
        let mut offsets = Vec::new();
        for event in self.events.iter() {
            if event.offset < start_offset {
                continue;
            }
            if event.offset > end_offset {
                break;
            }
            if matches!(event.kind, OutputEventKind::Image { .. }) {
                offsets.push(event.offset);
                if offsets.len() >= limit {
                    break;
                }
            }
        }
        offsets
    }

    fn count_images_in_range(&self, start_offset: u64, end_offset: u64) -> usize {
        let end_offset = end_offset.min(self.len());
        let start_offset = start_offset.min(end_offset);
        let mut count = 0usize;
        for event in self.events.iter() {
            if event.offset < start_offset {
                continue;
            }
            if event.offset > end_offset {
                break;
            }
            if matches!(event.kind, OutputEventKind::Image { .. }) {
                count = count.saturating_add(1);
            }
        }
        count
    }
}

fn ascii_lower(byte: u8) -> u8 {
    if byte.is_ascii_uppercase() {
        byte.to_ascii_lowercase()
    } else {
        byte
    }
}

#[derive(Debug, Clone)]
struct PagerEventByte {
    offset: u64,
    kind: OutputEventKind,
}

impl merge::EventView for PagerEventByte {
    fn offset(&self) -> u64 {
        self.offset
    }

    fn kind(&self) -> &OutputEventKind {
        &self.kind
    }
}

fn build_char_index(bytes: &[u8]) -> Vec<usize> {
    if let Ok(text) = std::str::from_utf8(bytes) {
        let mut index: Vec<usize> = text.char_indices().map(|(idx, _)| idx).collect();
        index.push(bytes.len());
        if index.is_empty() {
            index.push(0);
        }
        return index;
    }

    // Fallback: treat each byte as a "character" if UTF-8 is invalid.
    let mut index: Vec<usize> = (0..=bytes.len()).collect();
    if index.is_empty() {
        index.push(0);
    }
    index
}

fn extend_char_index(target: &mut Vec<usize>, appended: &[usize], base_byte: usize) {
    if target.is_empty() {
        target.push(0);
    }
    for offset in appended.iter().skip(1) {
        target.push(base_byte.saturating_add(*offset));
    }
}

fn char_len(index: &[usize]) -> u64 {
    index.len().saturating_sub(1) as u64
}

fn byte_index_for_char_offset(index: &[usize], offset: u64, len_bytes: usize) -> usize {
    if index.is_empty() {
        return 0;
    }
    let max = index.len().saturating_sub(1) as u64;
    let idx = offset.min(max) as usize;
    index.get(idx).copied().unwrap_or(len_bytes)
}

fn char_offset_for_byte_index(index: &[usize], byte_index: usize) -> u64 {
    if index.is_empty() {
        return 0;
    }
    match index.binary_search(&byte_index) {
        Ok(pos) => pos as u64,
        Err(pos) => pos.saturating_sub(1) as u64,
    }
}

fn line_end_offsets(bytes: &[u8], index: &[usize], base_char: u64) -> Vec<u64> {
    memchr_iter(b'\n', bytes)
        .map(|idx| {
            let end_byte = idx.saturating_add(1);
            base_char.saturating_add(char_offset_for_byte_index(index, end_byte))
        })
        .collect()
}

#[derive(Debug)]
struct PagerState {
    is_error: bool,
    last_range: Option<(u64, u64)>,
    last_emitted: Option<(u64, u64)>,
    buffer: PagerBuffer,
    seen_ranges: RangeSet,
    seen_images: HashSet<String>,
    image_numbers: HashMap<String, u64>,
    next_image_number: u64,
    search_mode: SearchMode,
}

struct CommandOutcome {
    contents: Vec<WorkerContent>,
    pages_left: u64,
    dismiss: bool,
    is_error: bool,
    update_range: bool,
    first_range: Option<(u64, u64)>,
    last_range: Option<(u64, u64)>,
}

impl CommandOutcome {
    fn new(
        contents: Vec<WorkerContent>,
        pages_left: u64,
        is_error: bool,
        update_range: bool,
        first_range: Option<(u64, u64)>,
        last_range: Option<(u64, u64)>,
        dismiss: Option<bool>,
    ) -> Self {
        let dismiss = dismiss.unwrap_or(pages_left == 0);
        Self {
            contents,
            pages_left,
            dismiss,
            is_error,
            update_range,
            first_range,
            last_range,
        }
    }

    fn page(
        contents: Vec<WorkerContent>,
        pages_left: u64,
        is_error: bool,
        first_range: Option<(u64, u64)>,
        last_range: Option<(u64, u64)>,
    ) -> Self {
        Self::new(
            contents,
            pages_left,
            is_error,
            true,
            first_range,
            last_range,
            None,
        )
    }

    fn page_keep(
        contents: Vec<WorkerContent>,
        pages_left: u64,
        is_error: bool,
        first_range: Option<(u64, u64)>,
        last_range: Option<(u64, u64)>,
    ) -> Self {
        Self::new(
            contents,
            pages_left,
            is_error,
            true,
            first_range,
            last_range,
            Some(false),
        )
    }

    fn page_dismiss(
        contents: Vec<WorkerContent>,
        pages_left: u64,
        is_error: bool,
        first_range: Option<(u64, u64)>,
        last_range: Option<(u64, u64)>,
    ) -> Self {
        Self::new(
            contents,
            pages_left,
            is_error,
            true,
            first_range,
            last_range,
            Some(true),
        )
    }

    fn no_range(contents: Vec<WorkerContent>, pages_left: u64, is_error: bool) -> Self {
        Self::new(contents, pages_left, is_error, false, None, None, None)
    }
}

#[derive(Debug, Default)]
pub(crate) struct Pager {
    state: Option<PagerState>,
}

fn pager_reply(
    contents: Vec<WorkerContent>,
    is_error: bool,
    error_code: Option<WorkerErrorCode>,
) -> WorkerReply {
    WorkerReply::Output {
        contents,
        is_error,
        error_code,
        prompt: None,
        prompt_variants: None,
    }
}

impl Pager {
    pub(crate) fn is_active(&self) -> bool {
        self.state.is_some()
    }

    pub(crate) fn refresh_from_output(&mut self, output: &OutputBuffer) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        output.start_capture();
        let end_offset = output
            .end_offset()
            .unwrap_or_else(|| state.buffer.source_end_offset());
        let start_offset = state.buffer.source_end_offset();
        if end_offset <= start_offset {
            return;
        }
        let range = output.read_range(start_offset, end_offset);
        output.advance_offset_to(end_offset);
        state.buffer.append_range(range);
    }

    pub(crate) fn activate(&mut self, buffer: PagerBuffer, is_error: bool) {
        self.state = Some(PagerState {
            is_error,
            last_range: None,
            last_emitted: None,
            buffer,
            seen_ranges: RangeSet::default(),
            seen_images: HashSet::new(),
            image_numbers: HashMap::new(),
            next_image_number: 1,
            search_mode: SearchMode::None,
        });
    }

    pub(crate) fn dismiss(&mut self) {
        self.state = None;
    }

    fn dedupe_images(&mut self, contents: &mut [WorkerContent]) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        for content in contents.iter_mut() {
            let WorkerContent::ContentImage { id, .. } = content else {
                continue;
            };
            let image_id = id.clone();
            let num = *state
                .image_numbers
                .entry(image_id.clone())
                .or_insert_with(|| {
                    let next = state.next_image_number;
                    state.next_image_number = state.next_image_number.saturating_add(1);
                    next
                });
            if state.seen_images.contains(&image_id) {
                *content = WorkerContent::stderr(format!("[pager] image #{num} already shown\n"));
            } else {
                state.seen_images.insert(image_id);
            }
        }
    }

    fn footer(&self, pages_left: u64) -> String {
        let Some(state) = self.state.as_ref() else {
            return footer_min(pages_left);
        };

        let marker = position_marker(
            state.buffer.current_offset(),
            state.buffer.len(),
            state.last_range,
        );
        if pages_left == 0 {
            return if marker.is_empty() {
                footer_min(0)
            } else {
                format!("(END, {marker})")
            };
        }
        if marker.is_empty() {
            footer_min(pages_left)
        } else {
            format!("--More-- ({pages_left}p, {marker})")
        }
    }

    fn pages_left_for_help(&self, page_bytes: u64) -> u64 {
        let Some(state) = self.state.as_ref() else {
            return 0;
        };
        pages_left(
            &state.buffer,
            state.buffer.current_offset(),
            state.buffer.len(),
            page_bytes.max(1),
        )
    }

    pub(crate) fn handle_command(&mut self, input: &str) -> WorkerReply {
        if self.state.is_none() {
            return pager_reply(
                vec![WorkerContent::stderr("[pager] no pager active")],
                true,
                None,
            );
        };

        let Some(command) = PagerCommand::parse(input) else {
            let page_bytes = page_bytes();
            let pages_left = self.pages_left_for_help(page_bytes);
            let mut contents = vec![WorkerContent::stderr(non_command_input_message(input))];
            contents.push(WorkerContent::stderr(self.footer(pages_left)));
            return pager_reply(contents, false, None);
        };

        let page_bytes = page_bytes();

        if let PagerCommand::Quit = command {
            let footer = self.footer(0);
            self.dismiss();
            let contents = vec![WorkerContent::stderr(footer)];
            return pager_reply(contents, false, None);
        }

        let should_append_footer = matches!(
            command,
            PagerCommand::Next { .. }
                | PagerCommand::Skip { .. }
                | PagerCommand::All
                | PagerCommand::Range { .. }
                | PagerCommand::Tail { .. }
                | PagerCommand::Search { .. }
                | PagerCommand::Where { .. }
                | PagerCommand::Matches { .. }
                | PagerCommand::Hits { .. }
                | PagerCommand::SearchNext { .. }
                | PagerCommand::Seek { .. }
                | PagerCommand::Help
        );

        let CommandOutcome {
            mut contents,
            pages_left,
            dismiss,
            is_error,
            first_range,
            last_range,
            ..
        } = {
            let state = self.state.as_mut().expect("pager state disappeared");
            let is_error = state.is_error;
            let buffer = &mut state.buffer;
            let outcome = match command {
                PagerCommand::Next { count } => {
                    let (contents, pages_left, span) =
                        take_next_pages(buffer, page_bytes, count, &mut state.seen_ranges);
                    CommandOutcome::page(contents, pages_left, is_error, span.first, span.last)
                }
                PagerCommand::Skip { count } => {
                    let (contents, pages_left, span) =
                        skip_pages_and_take_next(buffer, page_bytes, count, &mut state.seen_ranges);
                    CommandOutcome::page(contents, pages_left, is_error, span.first, span.last)
                }
                PagerCommand::All => {
                    let (contents, pages_left, span) =
                        take_all(buffer, page_bytes, &mut state.seen_ranges);
                    CommandOutcome::page(contents, pages_left, is_error, span.first, span.last)
                }
                PagerCommand::Tail { spec } => {
                    let (contents, pages_left, span) = match spec {
                        TailSpec::Default => take_tail(buffer, page_bytes, &mut state.seen_ranges),
                        TailSpec::Bytes(bytes) => {
                            take_tail(buffer, bytes.max(1), &mut state.seen_ranges)
                        }
                        TailSpec::Lines(lines) => {
                            take_tail_lines(buffer, lines, &mut state.seen_ranges)
                        }
                    };
                    CommandOutcome::page_dismiss(
                        contents, pages_left, is_error, span.first, span.last,
                    )
                }
                PagerCommand::Search { pattern } => {
                    state.search_mode = SearchMode::Page(pattern.clone());
                    let (contents, pages_left, span) =
                        take_search(buffer, page_bytes, &pattern, &mut state.seen_ranges);
                    CommandOutcome::page_keep(contents, pages_left, is_error, span.first, span.last)
                }
                PagerCommand::Where { pattern } => {
                    let pages_left = pages_left_for_buffer(buffer, page_bytes);
                    let contents = vec![WorkerContent::stderr(where_in_buffer(
                        buffer,
                        page_bytes,
                        &pattern,
                        &state.seen_ranges,
                    ))];
                    CommandOutcome::new(contents, pages_left, is_error, true, None, None, None)
                }
                PagerCommand::Matches { spec } => {
                    let pages_left = pages_left_for_buffer(buffer, page_bytes);
                    let (contents, is_error) = take_matches(buffer, &spec, &mut state.seen_ranges);
                    CommandOutcome::no_range(contents, pages_left, is_error)
                }
                PagerCommand::Hits { spec } => {
                    let mut hit_state = HitState::new(spec.pattern.clone(), spec.context);
                    let (contents, pages_left, span) = take_hits_next(
                        buffer,
                        &mut hit_state,
                        page_bytes,
                        spec.count,
                        &mut state.seen_ranges,
                    );
                    CommandOutcome::page_keep(contents, pages_left, is_error, span.first, span.last)
                }
                PagerCommand::SearchNext { count } => {
                    let (contents, pages_left, span) = match &mut state.search_mode {
                        SearchMode::Page(pattern) => take_search_next(
                            buffer,
                            page_bytes,
                            pattern,
                            count,
                            &mut state.seen_ranges,
                        ),
                        SearchMode::None => {
                            take_next_pages(buffer, page_bytes, count, &mut state.seen_ranges)
                        }
                    };
                    CommandOutcome::page_keep(contents, pages_left, is_error, span.first, span.last)
                }
                PagerCommand::Range { start, end } => {
                    let pages_left = pages_left_for_buffer(buffer, page_bytes);
                    let (mut contents, span) =
                        take_line_range(buffer, start, end, &mut state.seen_ranges);
                    if contents.is_empty() {
                        contents.push(WorkerContent::stderr(
                            "[pager] no remaining output in range".to_string(),
                        ));
                    }
                    CommandOutcome::new(
                        contents, pages_left, is_error, false, span.first, span.last, None,
                    )
                }
                PagerCommand::Seek { spec } => {
                    let end_offset = buffer.len();
                    let pages_left = pages_left_for_buffer(buffer, page_bytes);
                    let (desired_offset, error_message) = match spec {
                        SeekSpec::Offset(offset) => (Some(offset.min(end_offset)), None),
                        SeekSpec::Percent(percent) => (
                            Some(end_offset.saturating_mul(percent).saturating_div(100)),
                            None,
                        ),
                        SeekSpec::Line(line) => match buffer.line_start_offset(line) {
                            Some(offset) => (Some(offset), None),
                            None => (None, Some(format!("[pager] line out of range: {line}"))),
                        },
                    };

                    if let Some(message) = error_message {
                        let contents = vec![WorkerContent::stderr(message)];
                        CommandOutcome::no_range(contents, pages_left, is_error)
                    } else {
                        let desired_offset = desired_offset.expect("seek offset missing");
                        buffer.advance_offset_to(desired_offset);
                        let (contents, pages_left, span) =
                            take_next_page(buffer, page_bytes, &mut state.seen_ranges);
                        CommandOutcome::page(contents, pages_left, is_error, span.first, span.last)
                    }
                }
                PagerCommand::Help => {
                    let pages_left = pages_left_for_buffer(buffer, page_bytes);
                    let contents = vec![WorkerContent::stderr(pager_help_text())];
                    CommandOutcome::no_range(contents, pages_left, is_error)
                }
                PagerCommand::Quit => {
                    unreachable!("handled above");
                }
            };

            if outcome.update_range {
                state.last_range = outcome.last_range;
            }
            outcome
        };

        self.dedupe_images(&mut contents);
        if let Some(state) = self.state.as_mut() {
            if let Some(marker) = gap_marker_if_needed(state.last_emitted, first_range) {
                contents.insert(0, marker);
            }
            if let Some(last) = last_range {
                state.last_emitted = Some(last);
            }
        }

        if should_append_footer {
            if dismiss {
                let footer = self.footer(0);
                self.dismiss();
                contents.push(WorkerContent::stderr(footer));
            } else {
                contents.push(WorkerContent::stderr(self.footer(pages_left)));
            }
        }

        pager_reply(contents, is_error, None)
    }
}

pub(crate) fn maybe_activate_and_append_footer(
    pager: &mut Pager,
    contents: &mut Vec<WorkerContent>,
    pages_left: u64,
    is_error: bool,
    buffer: Option<PagerBuffer>,
    last_range: Option<(u64, u64)>,
) {
    if pages_left == 0 {
        return;
    }
    let Some(buffer) = buffer else {
        return;
    };
    pager.activate(buffer, is_error);
    if let Some(state) = pager.state.as_mut() {
        let range = last_range;
        state.last_range = range;
        state.last_emitted = range;
        if let Some((start, end)) = range {
            state.seen_ranges.insert(start, end);
        }
    }
    pager.dedupe_images(contents);
    contents.push(WorkerContent::stderr(pager.footer(pages_left)));
}

fn contents_from_output_range(range: OutputRange) -> Vec<WorkerContent> {
    if range.bytes.is_empty() && range.events.is_empty() {
        return Vec::new();
    }
    merge::merge_bytes_with_events(
        &range.bytes,
        range.start_offset,
        range.end_offset,
        &range.events,
        output_event_to_content,
    )
}

fn output_event_to_content(kind: &OutputEventKind) -> WorkerContent {
    match kind {
        OutputEventKind::Image {
            data,
            mime_type,
            id,
            is_new,
        } => WorkerContent::ContentImage {
            data: data.clone(),
            mime_type: mime_type.clone(),
            id: id.clone(),
            is_new: *is_new,
        },
        OutputEventKind::Text { text, is_stderr } => {
            if *is_stderr {
                WorkerContent::stderr(text.clone())
            } else {
                WorkerContent::stdout(text.clone())
            }
        }
    }
}

pub(crate) fn take_range_from_ring(output: &OutputBuffer, end_offset: u64) -> Vec<WorkerContent> {
    let start_offset = output.current_offset().unwrap_or(end_offset);
    let range = output.read_range(start_offset, end_offset);
    output.advance_offset_to(end_offset);
    contents_from_output_range(range)
}

pub(crate) fn take_snapshot_page_from_ring(
    output: &OutputBuffer,
    end_offset: u64,
    target_bytes: u64,
) -> SnapshotPage {
    let Some(range) = snapshot_from_ring(output, end_offset) else {
        return SnapshotPage {
            contents: Vec::new(),
            pages_left: 0,
            buffer: None,
            last_range: None,
            last_range_end_byte: None,
        };
    };
    take_snapshot_page_from_buffer(PagerBuffer::from_range(range), target_bytes)
}

pub(crate) fn take_snapshot_page_from_buffer(
    mut buffer: PagerBuffer,
    target_bytes: u64,
) -> SnapshotPage {
    if buffer.bytes.is_empty() {
        let contents = buffer.contents_for_range(0, buffer.len());
        return SnapshotPage {
            contents,
            pages_left: 0,
            buffer: Some(buffer),
            last_range: None,
            last_range_end_byte: None,
        };
    }
    let mut seen = RangeSet::default();
    let (contents, pages_left, span) = take_next_page(&mut buffer, target_bytes, &mut seen);
    let last_range_end_byte = span
        .last
        .map(|(_, end)| buffer.byte_index_for_char_offset(end) as u64);
    SnapshotPage {
        contents,
        pages_left,
        buffer: Some(buffer),
        last_range: span.last,
        last_range_end_byte,
    }
}

fn snapshot_from_ring(output: &OutputBuffer, end_offset: u64) -> Option<OutputRange> {
    output.start_capture();
    let start_offset = output.current_offset().unwrap_or(end_offset);
    let range = output.read_range(start_offset, end_offset);
    output.advance_offset_to(end_offset);
    if range.bytes.is_empty() && range.events.is_empty() {
        return None;
    }
    Some(range)
}

fn take_next_page(
    buffer: &mut PagerBuffer,
    target_bytes: u64,
    seen: &mut RangeSet,
) -> (Vec<WorkerContent>, u64, RangeSpan) {
    let target_bytes = target_bytes.max(1);
    let end_offset = buffer.len();
    let mut cursor = buffer.current_offset();
    let mut remaining_budget = target_bytes;
    let mut contents = Vec::new();
    let mut span = RangeSpan::default();
    let mut pending_gap: Option<(u64, u64)> = None;

    while remaining_budget > 0 && cursor < end_offset {
        if let Some((_, range_end)) = seen.range_containing(cursor) {
            let gap_end = range_end.min(end_offset);
            pending_gap = Some(match pending_gap {
                Some((gap_start, _)) => (gap_start, gap_end),
                None => (cursor, gap_end),
            });
            cursor = gap_end;
            continue;
        }

        let next_seen = seen
            .next_range_start_after(cursor)
            .unwrap_or(end_offset)
            .min(end_offset);
        if next_seen <= cursor {
            break;
        }

        let desired_end = cursor
            .saturating_add(remaining_budget)
            .min(next_seen)
            .max(cursor);
        let visible_end = page_end_offset_with_images(
            buffer,
            cursor,
            next_seen,
            remaining_budget,
            MAX_IMAGES_PER_PAGE,
        );
        if visible_end <= cursor {
            break;
        }

        if let Some((gap_start, gap_end)) = pending_gap.take() {
            contents.push(elision_marker(gap_start, gap_end));
        }

        let segment_contents = buffer.contents_for_range(cursor, visible_end);
        if !segment_contents.is_empty() {
            contents.extend(segment_contents);
        }
        seen.insert(cursor, visible_end);
        span.record(Some((cursor, visible_end)));

        let images = buffer.count_images_in_range(cursor, visible_end);
        let mut used_budget = (visible_end - cursor)
            .saturating_add((images as u64).saturating_mul(IMAGE_EQUIV_CHARS));
        if visible_end < desired_end {
            // If the pager snaps to a boundary before exhausting the budget (newline or image
            // policy), treat the remainder as consumed. Otherwise we risk emitting tiny trailing
            // fragments (e.g. splitting the next line mid-token).
            used_budget = remaining_budget;
        }
        if used_budget >= remaining_budget {
            cursor = visible_end;
            break;
        }
        remaining_budget = remaining_budget.saturating_sub(used_budget);
        cursor = visible_end;
    }

    if let Some((gap_start, gap_end)) = pending_gap.take() {
        contents.push(elision_marker(gap_start, gap_end));
    }

    buffer.advance_offset_to(cursor);
    let pages_left = pages_left_for_buffer(buffer, target_bytes);
    (contents, pages_left, span)
}

fn page_end_offset_with_images(
    buffer: &PagerBuffer,
    start_offset: u64,
    end_offset: u64,
    target_bytes: u64,
    max_images: usize,
) -> u64 {
    let end_offset = end_offset.min(buffer.len());
    if start_offset >= end_offset {
        return end_offset;
    }
    let remaining_text = end_offset.saturating_sub(start_offset);
    let remaining_images = buffer.count_images_in_range(start_offset, end_offset);
    if remaining_text <= target_bytes && (max_images == 0 || remaining_images <= max_images) {
        return end_offset;
    }
    if max_images == 0 {
        return buffer.page_end_offset(start_offset, end_offset, target_bytes);
    }

    let image_offsets = buffer.image_offsets_in_range(start_offset, end_offset, max_images + 1);
    let max_k = image_offsets.len().min(max_images);
    let mut best_end = start_offset;

    for k in 0..=max_k {
        let image_cost = (k as u64).saturating_mul(IMAGE_EQUIV_CHARS);
        let text_budget = target_bytes.saturating_sub(image_cost);
        let budget_end = start_offset.saturating_add(text_budget);
        let segment_start = if k == 0 {
            start_offset
        } else {
            image_offsets[k - 1]
        };
        let segment_end_exclusive = if k < image_offsets.len() {
            image_offsets[k]
        } else {
            end_offset.saturating_add(1)
        };
        let Some(segment_end_inclusive) = segment_end_exclusive.checked_sub(1) else {
            continue;
        };
        let mut candidate_end = end_offset.min(budget_end).min(segment_end_inclusive);
        if candidate_end < segment_start {
            continue;
        }
        let aligned_budget = candidate_end.saturating_sub(start_offset);
        if aligned_budget > 0 {
            let aligned = buffer.page_end_offset(start_offset, candidate_end, aligned_budget);
            if aligned >= segment_start {
                candidate_end = aligned;
            }
        }
        if candidate_end > best_end {
            best_end = candidate_end;
        }
    }

    best_end
}

fn take_next_pages(
    buffer: &mut PagerBuffer,
    target_bytes: u64,
    count: u64,
    seen: &mut RangeSet,
) -> (Vec<WorkerContent>, u64, RangeSpan) {
    let count = count.clamp(1, 50);
    let mut contents = Vec::new();
    let mut pages_left = 0;
    let mut span = RangeSpan::default();
    for _ in 0..count {
        let (page_contents, left, range) = take_next_page(buffer, target_bytes, seen);
        pages_left = left;
        if page_contents.is_empty() {
            break;
        }
        contents.extend(page_contents);
        span.record(range.first);
        span.record(range.last);
        if pages_left == 0 {
            break;
        }
    }
    if contents.is_empty() {
        (Vec::new(), pages_left, RangeSpan::default())
    } else {
        (contents, pages_left, span)
    }
}

fn advance_cursor_for_page(
    buffer: &PagerBuffer,
    start_offset: u64,
    end_offset: u64,
    page_bytes: u64,
    seen: &RangeSet,
) -> u64 {
    let end_offset = end_offset.min(buffer.len());
    let mut cursor = start_offset.min(end_offset);
    let mut remaining_budget = page_bytes.max(1);

    while remaining_budget > 0 && cursor < end_offset {
        if let Some((_, range_end)) = seen.range_containing(cursor) {
            cursor = range_end.min(end_offset);
            continue;
        }

        let next_seen = seen
            .next_range_start_after(cursor)
            .unwrap_or(end_offset)
            .min(end_offset);
        if next_seen <= cursor {
            break;
        }

        let desired_end = cursor
            .saturating_add(remaining_budget)
            .min(next_seen)
            .max(cursor);
        let visible_end = page_end_offset_with_images(
            buffer,
            cursor,
            next_seen,
            remaining_budget,
            MAX_IMAGES_PER_PAGE,
        );
        if visible_end <= cursor {
            break;
        }

        let images = buffer.count_images_in_range(cursor, visible_end);
        let mut used_budget = (visible_end - cursor)
            .saturating_add((images as u64).saturating_mul(IMAGE_EQUIV_CHARS));
        if visible_end < desired_end {
            used_budget = remaining_budget;
        }
        if used_budget >= remaining_budget {
            return visible_end;
        }
        remaining_budget = remaining_budget.saturating_sub(used_budget);
        cursor = visible_end;
    }

    cursor
}

fn skip_pages_and_take_next(
    buffer: &mut PagerBuffer,
    page_bytes: u64,
    count: u64,
    seen: &mut RangeSet,
) -> (Vec<WorkerContent>, u64, RangeSpan) {
    let count = count.clamp(1, 500);
    let end_offset = buffer.len();
    let mut cursor = buffer.current_offset();
    if cursor >= end_offset {
        return (Vec::new(), 0, RangeSpan::default());
    }

    for _ in 0..count {
        let next = advance_cursor_for_page(buffer, cursor, end_offset, page_bytes, seen);
        if next <= cursor {
            break;
        }
        cursor = next;
        if cursor >= end_offset {
            break;
        }
    }

    buffer.advance_offset_to(cursor);
    take_next_page(buffer, page_bytes, seen)
}

fn take_unseen_segments_in_range(
    buffer: &PagerBuffer,
    start_offset: u64,
    end_offset: u64,
    seen: &mut RangeSet,
) -> (Vec<WorkerContent>, RangeSpan) {
    let mut cursor = start_offset.min(end_offset);
    let mut out = Vec::new();
    let mut span = RangeSpan::default();
    let mut pending_gap: Option<(u64, u64)> = None;

    while cursor < end_offset {
        if let Some((_, range_end)) = seen.range_containing(cursor) {
            let gap_end = range_end.min(end_offset);
            pending_gap = Some(match pending_gap {
                Some((gap_start, _)) => (gap_start, gap_end),
                None => (cursor, gap_end),
            });
            cursor = gap_end;
            continue;
        }

        let next_seen = seen
            .next_range_start_after(cursor)
            .unwrap_or(end_offset)
            .min(end_offset);
        if next_seen <= cursor {
            break;
        }

        let contents = buffer.contents_for_range(cursor, next_seen);
        if !contents.is_empty() {
            if let Some((gap_start, gap_end)) = pending_gap.take() {
                out.push(elision_marker(gap_start, gap_end));
            }
            out.extend(contents);
            seen.insert(cursor, next_seen);
            span.record(Some((cursor, next_seen)));
        }
        cursor = next_seen;
    }

    (out, span)
}

fn take_all(
    buffer: &mut PagerBuffer,
    page_bytes: u64,
    seen: &mut RangeSet,
) -> (Vec<WorkerContent>, u64, RangeSpan) {
    let start_offset = buffer.current_offset();
    let end_offset = buffer.len();
    let (contents, span) = take_unseen_segments_in_range(buffer, start_offset, end_offset, seen);
    buffer.advance_offset_to(end_offset);
    let pages_left = pages_left_for_buffer(buffer, page_bytes);
    (contents, pages_left, span)
}

fn take_tail(
    buffer: &mut PagerBuffer,
    page_bytes: u64,
    seen: &mut RangeSet,
) -> (Vec<WorkerContent>, u64, RangeSpan) {
    let current = buffer.current_offset();
    let end_offset = buffer.len();
    let desired_start = current.max(end_offset.saturating_sub(page_bytes));
    let snapped = buffer
        .last_line_end_before(desired_start)
        .unwrap_or(desired_start);
    take_tail_from(buffer, snapped, seen)
}

fn take_tail_lines(
    buffer: &mut PagerBuffer,
    lines: usize,
    seen: &mut RangeSet,
) -> (Vec<WorkerContent>, u64, RangeSpan) {
    let end_offset = buffer.len();
    let desired_start = buffer.tail_start_offset_for_lines(end_offset, lines);
    take_tail_from(buffer, desired_start, seen)
}

fn take_tail_from(
    buffer: &mut PagerBuffer,
    start_offset: u64,
    seen: &mut RangeSet,
) -> (Vec<WorkerContent>, u64, RangeSpan) {
    let current = buffer.current_offset();
    let end_offset = buffer.len();
    let start = current.max(start_offset).min(end_offset);
    let (contents, span) = take_unseen_segments_in_range(buffer, start, end_offset, seen);
    buffer.advance_offset_to(end_offset);
    if contents.is_empty() {
        (Vec::new(), 0, RangeSpan::default())
    } else {
        (contents, 0, span)
    }
}

fn pages_left(buffer: &PagerBuffer, current_offset: u64, end_offset: u64, page_bytes: u64) -> u64 {
    let end_offset = end_offset.min(buffer.len());
    let current_offset = current_offset.min(end_offset);
    let remaining_text = end_offset.saturating_sub(current_offset);
    let remaining_images = buffer.count_images_in_range(current_offset, end_offset);
    if remaining_text == 0 && remaining_images == 0 {
        return 0;
    }
    let page_bytes = page_bytes.max(1);
    if remaining_text <= page_bytes
        && (MAX_IMAGES_PER_PAGE == 0 || remaining_images <= MAX_IMAGES_PER_PAGE)
    {
        return 0;
    }
    let image_cost = (remaining_images as u64).saturating_mul(IMAGE_EQUIV_CHARS);
    let cost_pages = remaining_text
        .saturating_add(image_cost)
        .div_ceil(page_bytes);
    let image_pages = if MAX_IMAGES_PER_PAGE == 0 {
        0
    } else {
        (remaining_images as u64).div_ceil(MAX_IMAGES_PER_PAGE as u64)
    };
    cost_pages.max(image_pages)
}

pub(crate) fn page_bytes() -> u64 {
    static PAGE_BYTES: OnceLock<u64> = OnceLock::new();
    *PAGE_BYTES.get_or_init(|| {
        let parsed = std::env::var(PAGER_PAGE_CHARS_ENV)
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .or_else(|| {
                std::env::var(PAGER_PAGE_BYTES_ENV)
                    .ok()
                    .and_then(|value| value.trim().parse::<u64>().ok())
            });
        parsed
            .unwrap_or(DEFAULT_PAGER_PAGE_CHARS)
            .max(MIN_PAGER_PAGE_CHARS)
    })
}

pub(crate) fn resolve_page_bytes(override_bytes: Option<u64>) -> u64 {
    override_bytes
        .unwrap_or_else(page_bytes)
        .max(MIN_PAGER_PAGE_CHARS)
}

pub(crate) fn build_input_echo(input: &str) -> Option<String> {
    let normalized = input.replace("\r\n", "\n");
    let trimmed = normalized.trim_end_matches('\n').trim_end();
    if trimmed.is_empty() {
        return None;
    }

    let mut lines = trimmed.lines();
    let first_line = lines.next().unwrap_or_default().trim_end();
    let remaining_lines = lines.next().is_some();
    let mut summary: String = first_line.chars().take(INPUT_ECHO_MAX_CHARS).collect();
    let needs_truncate = remaining_lines || first_line.chars().count() > INPUT_ECHO_MAX_CHARS;
    if needs_truncate {
        summary.push_str(INPUT_ECHO_TRUNC_SUFFIX);
    }

    // Avoid a `> ` prefix here: it is indistinguishable from the backend prompt in transcripts.
    let prefix = "[repl] input: ";
    let mut echo = String::with_capacity(prefix.len() + summary.len() + 1);
    echo.push_str(prefix);
    echo.push_str(&summary);
    echo.push('\n');
    Some(echo)
}

fn pages_left_for_buffer(buffer: &PagerBuffer, page_bytes: u64) -> u64 {
    pages_left(
        buffer,
        buffer.current_offset(),
        buffer.len(),
        page_bytes.max(1),
    )
}

fn take_line_range(
    buffer: &PagerBuffer,
    start_line: usize,
    end_line: usize,
    seen: &mut RangeSet,
) -> (Vec<WorkerContent>, RangeSpan) {
    let Some((start_offset, end_offset)) = buffer.line_range_offsets(start_line, end_line) else {
        return (
            vec![WorkerContent::stderr(
                "[pager] line range out of bounds".to_string(),
            )],
            RangeSpan::default(),
        );
    };

    let (contents, span) = take_unseen_segments_in_range(buffer, start_offset, end_offset, seen);
    (contents, span)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output_capture::OutputEvent;
    use crate::worker_protocol::TextStream;

    fn text_from_reply(reply: WorkerReply) -> String {
        let WorkerReply::Output { contents, .. } = reply;
        let mut out = String::new();
        for content in contents {
            match content {
                WorkerContent::ContentText { text, .. } => out.push_str(&text),
                WorkerContent::ContentImage { .. } => out.push_str("<image>"),
            }
        }
        out
    }

    fn activate_pager_with_text(text: &str) -> Pager {
        let range = OutputRange {
            start_offset: 0,
            end_offset: text.len() as u64,
            bytes: text.as_bytes().to_vec(),
            events: Vec::new(),
        };
        let buffer = PagerBuffer::from_range(range);
        let mut pager = Pager::default();
        pager.activate(buffer, false);
        pager
    }

    #[test]
    fn pager_buffer_uses_utf8_character_offsets() {
        let text = "éx\n";
        let bytes = text.as_bytes().to_vec();
        let range = OutputRange {
            start_offset: 0,
            end_offset: bytes.len() as u64,
            bytes,
            events: Vec::new(),
        };
        let buffer = PagerBuffer::from_range(range);

        assert_eq!(buffer.len(), 3, "expected UTF-8 char length");
        assert_eq!(buffer.line_end_offset(1), Some(3));
        assert_eq!(
            buffer.find_next_bytes_with_options(0, buffer.len(), b"x", false),
            Some(1)
        );
    }

    #[test]
    fn take_unseen_segments_inserts_elision_marker() {
        let text = "line1\nline2\nline3\n";
        let bytes = text.as_bytes().to_vec();
        let range = OutputRange {
            start_offset: 0,
            end_offset: bytes.len() as u64,
            bytes,
            events: Vec::new(),
        };
        let buffer = PagerBuffer::from_range(range);
        let mut seen = RangeSet::default();
        seen.insert(6, 12);

        let (contents, _span) = take_unseen_segments_in_range(&buffer, 0, buffer.len(), &mut seen);

        assert_eq!(contents.len(), 3);
        let first = match &contents[0] {
            WorkerContent::ContentText { text, stream } => {
                assert!(matches!(stream, TextStream::Stdout));
                text.as_str()
            }
            _ => panic!("expected stdout content"),
        };
        assert_eq!(first, "line1\n");

        let marker = match &contents[1] {
            WorkerContent::ContentText { text, stream } => {
                assert!(matches!(stream, TextStream::Stderr));
                text.as_str()
            }
            _ => panic!("expected elision marker"),
        };
        assert!(
            marker.contains("elided output")
                && marker.contains("already shown")
                && marker.contains("@6..12"),
            "unexpected marker: {marker}"
        );

        let last = match &contents[2] {
            WorkerContent::ContentText { text, stream } => {
                assert!(matches!(stream, TextStream::Stdout));
                text.as_str()
            }
            _ => panic!("expected stdout content"),
        };
        assert_eq!(last, "line3\n");
    }

    #[test]
    fn gap_marker_emits_for_forward_jump() {
        let marker = gap_marker_if_needed(Some((0, 5)), Some((10, 12))).expect("expected marker");
        let text = match marker {
            WorkerContent::ContentText { text, stream } => {
                assert!(matches!(stream, TextStream::Stderr));
                text
            }
            _ => panic!("expected text marker"),
        };
        assert!(text.contains("@5..10"));
    }

    #[test]
    fn page_end_respects_image_limit() {
        let text = "A\nB\nC\nD\n";
        let bytes = text.as_bytes().to_vec();
        let events = vec![
            OutputEvent {
                offset: 2,
                kind: OutputEventKind::Image {
                    id: "plot-1".to_string(),
                    data: "a".to_string(),
                    mime_type: "image/png".to_string(),
                    is_new: true,
                },
            },
            OutputEvent {
                offset: 4,
                kind: OutputEventKind::Image {
                    id: "plot-2".to_string(),
                    data: "b".to_string(),
                    mime_type: "image/png".to_string(),
                    is_new: true,
                },
            },
            OutputEvent {
                offset: 6,
                kind: OutputEventKind::Image {
                    id: "plot-3".to_string(),
                    data: "c".to_string(),
                    mime_type: "image/png".to_string(),
                    is_new: true,
                },
            },
        ];
        let range = OutputRange {
            start_offset: 0,
            end_offset: bytes.len() as u64,
            bytes,
            events,
        };
        let buffer = PagerBuffer::from_range(range);

        let page_end =
            page_end_offset_with_images(&buffer, 0, buffer.len(), 5_000, MAX_IMAGES_PER_PAGE);
        assert!(page_end < 6, "expected page to stop before third image");
        assert!(page_end >= 4, "expected page to include first two images");
    }

    #[test]
    fn pager_dedupes_images_with_markers() {
        let range = OutputRange {
            start_offset: 0,
            end_offset: 0,
            bytes: Vec::new(),
            events: Vec::new(),
        };
        let buffer = PagerBuffer::from_range(range);
        let mut pager = Pager::default();
        pager.activate(buffer, false);

        let mut contents = vec![
            WorkerContent::ContentImage {
                data: "a".to_string(),
                mime_type: "image/png".to_string(),
                id: "plot-1".to_string(),
                is_new: true,
            },
            WorkerContent::stdout("after\n"),
            WorkerContent::ContentImage {
                data: "b".to_string(),
                mime_type: "image/png".to_string(),
                id: "plot-1".to_string(),
                is_new: false,
            },
        ];

        pager.dedupe_images(&mut contents);

        assert!(matches!(contents[0], WorkerContent::ContentImage { .. }));
        let marker = match &contents[2] {
            WorkerContent::ContentText { text, stream } => {
                assert!(matches!(stream, TextStream::Stderr));
                text.as_str()
            }
            _ => panic!("expected marker text"),
        };
        assert!(
            marker.contains("image #1 already shown"),
            "unexpected marker: {marker}"
        );
    }

    #[test]
    fn pager_dedupes_images_assigns_sequence_numbers() {
        let range = OutputRange {
            start_offset: 0,
            end_offset: 0,
            bytes: Vec::new(),
            events: Vec::new(),
        };
        let buffer = PagerBuffer::from_range(range);
        let mut pager = Pager::default();
        pager.activate(buffer, false);

        let mut contents = vec![
            WorkerContent::ContentImage {
                data: "a".to_string(),
                mime_type: "image/png".to_string(),
                id: "plot-1".to_string(),
                is_new: true,
            },
            WorkerContent::ContentImage {
                data: "b".to_string(),
                mime_type: "image/png".to_string(),
                id: "plot-2".to_string(),
                is_new: true,
            },
            WorkerContent::ContentImage {
                data: "c".to_string(),
                mime_type: "image/png".to_string(),
                id: "plot-1".to_string(),
                is_new: false,
            },
            WorkerContent::ContentImage {
                data: "d".to_string(),
                mime_type: "image/png".to_string(),
                id: "plot-2".to_string(),
                is_new: false,
            },
        ];

        pager.dedupe_images(&mut contents);

        let marker_one = match &contents[2] {
            WorkerContent::ContentText { text, stream } => {
                assert!(matches!(stream, TextStream::Stderr));
                text.as_str()
            }
            _ => panic!("expected marker text"),
        };
        assert!(
            marker_one.contains("image #1 already shown"),
            "unexpected marker: {marker_one}"
        );

        let marker_two = match &contents[3] {
            WorkerContent::ContentText { text, stream } => {
                assert!(matches!(stream, TextStream::Stderr));
                text.as_str()
            }
            _ => panic!("expected marker text"),
        };
        assert!(
            marker_two.contains("image #2 already shown"),
            "unexpected marker: {marker_two}"
        );
    }

    #[test]
    fn page_end_snaps_to_newline_even_when_budget_bound_is_end_offset() {
        // Repro case: `page_end_offset_with_images()` passes `end_offset` equal to the current
        // budget boundary. `PagerBuffer::page_end_offset()` must still snap to a newline to avoid
        // splitting lines mid-token.
        let text = (1..=2000)
            .map(|i| format!("L{:04}\n", i))
            .collect::<String>();
        let bytes = text.as_bytes().to_vec();
        let range = OutputRange {
            start_offset: 0,
            end_offset: bytes.len() as u64,
            bytes,
            events: Vec::new(),
        };
        let buffer = PagerBuffer::from_range(range);

        let page_end =
            page_end_offset_with_images(&buffer, 0, buffer.len(), 3500, MAX_IMAGES_PER_PAGE);
        assert!(
            page_end > 0 && page_end < 3500,
            "expected newline snap before budget end"
        );
        let byte_end = buffer.byte_index_for_char_offset(page_end);
        assert!(byte_end > 0, "expected non-empty page");
        assert_eq!(
            buffer.bytes[byte_end - 1],
            b'\n',
            "expected page to end on a newline boundary"
        );
    }

    #[test]
    fn blocks_non_command_input_while_pager_active() {
        let mut pager = activate_pager_with_text("line1\nline2\n");
        let text = text_from_reply(pager.handle_command("n\n"));
        assert!(
            text.contains("input blocked while pager is active"),
            "unexpected pager response: {text}"
        );
        assert!(pager.is_active(), "pager should remain active");
    }

    #[test]
    fn search_next_stays_on_last_slash_search_after_hits() {
        let text = "foo\n".repeat(2000);
        let mut pager = activate_pager_with_text(&text);

        let _ = pager.handle_command(":/foo\n");
        let _ = pager.handle_command(":hits zzz\n");
        let next = text_from_reply(pager.handle_command(":n\n"));

        assert!(
            next.contains("foo"),
            "expected :n to continue last /search, got: {next}"
        );
        assert!(
            !next.contains("zzz"),
            "expected :n not to switch to :hits pattern, got: {next}"
        );
    }

    #[test]
    fn seek_then_search_finds_near_end_match_in_large_buffer() {
        let mut text = (0..4000)
            .map(|i| format!("line-{i:04}\n"))
            .collect::<String>();
        text.push_str("manual-target-token\n");

        let mut pager = activate_pager_with_text(&text);
        let _ = pager.handle_command(":seek 80%\n");
        let found = text_from_reply(pager.handle_command(":/manual-target-token\n"));
        assert!(
            found.contains("manual-target-token"),
            "expected to find token after seek, got: {found}"
        );
    }
}
