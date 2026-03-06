use crate::worker_protocol::WorkerContent;

use super::{
    MATCH_BREADCRUMB_MAX_BYTES, MATCH_LINE_MAX_BYTES, MAX_MATCH_LIMIT, MatchSpec, PagerBuffer,
    RangeSet, RangeSpan, SearchPattern, pages_left_for_buffer, truncate_with_ellipsis,
};

#[derive(Debug, Clone)]
pub(super) struct SearchSession {
    pub(super) pattern: SearchPattern,
    pub(super) hits: Vec<SearchHit>,
    pub(super) current_index: usize,
    pub(super) buffer_len: u64,
    next_search_offset: u64,
    complete: bool,
    indexed_from_start: bool,
    headings: HeadingState,
}

#[derive(Debug, Clone)]
pub(super) struct SearchHit {
    pub(super) match_start: u64,
    pub(super) line_idx: usize,
    pub(super) line_start: u64,
    pub(super) line_end: u64,
    pub(super) breadcrumb: String,
}

#[derive(Debug, Clone)]
pub(super) struct HitState {
    pub(super) pattern: SearchPattern,
    pub(super) context: usize,
    pub(super) hit_index: usize,
    pub(super) last_emitted_line: Option<usize>,
    headings: HeadingState,
}

impl HitState {
    pub(super) fn new(pattern: SearchPattern, context: usize) -> Self {
        Self {
            pattern,
            context,
            hit_index: 0,
            last_emitted_line: None,
            headings: HeadingState::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Heading {
    level: usize,
    text: String,
}

#[derive(Debug, Clone)]
struct HeadingState {
    headings: Vec<Heading>,
    last_scanned_line_idx: usize,
    in_fence: Option<char>,
}

impl HeadingState {
    fn new() -> Self {
        Self {
            headings: Vec::new(),
            last_scanned_line_idx: 0,
            in_fence: None,
        }
    }

    fn scan_to(&mut self, buffer: &PagerBuffer, line_idx: usize) {
        if line_idx < self.last_scanned_line_idx {
            return;
        }
        for idx in self.last_scanned_line_idx..=line_idx {
            let line = read_line_text(buffer, idx);
            update_heading_stack(&line, &mut self.in_fence, &mut self.headings);
        }
        self.last_scanned_line_idx = line_idx.saturating_add(1);
    }
}

#[derive(Debug, Clone, Copy)]
struct MatchLine {
    line_idx: usize,
    line_start: u64,
    line_end: u64,
}

#[derive(Debug, Clone, Copy)]
enum MatchSearchResult {
    Found(MatchLine),
    NotFound,
    SeenOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SearchStepOutcome {
    Moved,
    Boundary,
}

fn all_matches_shown_message(pattern: &str) -> String {
    format!(
        "[pager] all matches already shown (pager does not repeat output): {}",
        pattern
    )
}

fn pattern_not_found_message(pattern: &str, start_offset: u64) -> String {
    if start_offset == 0 {
        format!("[pager] pattern not found: {pattern}")
    } else {
        format!(
            "[pager] pattern not found (search is forward-only over unseen output; use `:matches -n all {pattern}` to locate offsets, then `:seek @OFFSET` to jump)"
        )
    }
}

fn find_next_unseen_match_line(
    buffer: &PagerBuffer,
    start_offset: u64,
    pattern: &SearchPattern,
    seen: &RangeSet,
) -> MatchSearchResult {
    let end_offset = buffer.len();
    let total_lines = line_count(buffer);
    if start_offset >= end_offset || total_lines == 0 {
        return MatchSearchResult::NotFound;
    }

    let mut search_offset = start_offset;
    let mut saw_match = false;
    let pattern_bytes = pattern.pattern.as_bytes();
    loop {
        let Some(match_offset) = buffer.find_next_bytes_with_options(
            search_offset,
            end_offset,
            pattern_bytes,
            pattern.case_insensitive_ascii,
        ) else {
            return if saw_match {
                MatchSearchResult::SeenOnly
            } else {
                MatchSearchResult::NotFound
            };
        };
        saw_match = true;
        let line_idx = line_index_for_offset(buffer, match_offset);
        if line_idx >= total_lines {
            return MatchSearchResult::SeenOnly;
        }
        let (line_start, line_end) = line_bounds_for_index(buffer, line_idx);
        if !seen.covers(line_start, line_end) {
            return MatchSearchResult::Found(MatchLine {
                line_idx,
                line_start,
                line_end,
            });
        }
        search_offset = line_end.max(match_offset.saturating_add(1));
        if search_offset >= end_offset {
            return MatchSearchResult::SeenOnly;
        }
    }
}

pub(super) fn where_in_buffer(
    buffer: &PagerBuffer,
    page_bytes: u64,
    pattern: &SearchPattern,
    seen: &RangeSet,
) -> String {
    let start_offset = buffer.current_offset();
    let end_offset = buffer.len();
    if start_offset >= end_offset {
        return "[pager] no remaining output".to_string();
    }

    let line_start = match find_next_unseen_match_line(buffer, start_offset, pattern, seen) {
        MatchSearchResult::Found(matched) => matched.line_start,
        MatchSearchResult::SeenOnly => return all_matches_shown_message(&pattern.pattern),
        MatchSearchResult::NotFound => {
            return pattern_not_found_message(&pattern.pattern, start_offset);
        }
    };

    let mut cursor = start_offset;
    let mut pages_to_skip = 0u64;
    loop {
        let page_end = buffer.page_end_offset(cursor, end_offset, page_bytes.max(1));
        if page_end <= cursor {
            break;
        }
        if page_end <= line_start {
            pages_to_skip = pages_to_skip.saturating_add(1);
            cursor = page_end;
            if cursor >= end_offset {
                break;
            }
        } else {
            break;
        }
    }

    let search_cmd = if pattern.case_insensitive_ascii {
        format!(":/i {}", pattern.pattern)
    } else {
        format!(":/{}", pattern.pattern)
    };

    if pages_to_skip == 0 {
        format!("[pager] match is on the current/next page: use {search_cmd}")
    } else {
        format!(
            "[pager] next match is ~{pages_to_skip} page(s) ahead: use {search_cmd} or `:skip {pages_to_skip}`"
        )
    }
}

fn line_index_for_offset(buffer: &PagerBuffer, offset: u64) -> usize {
    buffer
        .line_ends
        .partition_point(|line_end| *line_end <= offset.min(buffer.len()))
}

fn line_count(buffer: &PagerBuffer) -> usize {
    if buffer.bytes.is_empty() {
        return 0;
    }
    let mut count = buffer.line_ends.len();
    let last_end = buffer.line_ends.last().copied().unwrap_or(0);
    if last_end < buffer.len() {
        count += 1;
    }
    count
}

fn line_bounds_for_index(buffer: &PagerBuffer, idx: usize) -> (u64, u64) {
    let start = if idx == 0 {
        0
    } else {
        buffer
            .line_ends
            .get(idx.saturating_sub(1))
            .copied()
            .unwrap_or(buffer.len())
    };
    let end = buffer.line_ends.get(idx).copied().unwrap_or(buffer.len());
    (start, end)
}

fn read_line_text(buffer: &PagerBuffer, idx: usize) -> String {
    let (start, end) = line_bounds_for_index(buffer, idx);
    let text = buffer.read_text_range(start, end);
    text.trim_end_matches(&['\n', '\r'][..]).to_string()
}

fn strip_trailing_anchor_link(text: &str) -> &str {
    let trimmed = text.trim_end();
    let Some(open_idx) = trimmed.rfind('[') else {
        return trimmed;
    };
    let link = &trimmed[open_idx..];
    if !link.ends_with(')') {
        return trimmed;
    }
    let Some(close_idx) = link.find("](") else {
        return trimmed;
    };
    let label = &link[1..close_idx];
    let target = &link[close_idx + 2..link.len().saturating_sub(1)];
    if !target.starts_with('#') {
        return trimmed;
    }
    if label.chars().any(|ch| ch.is_alphanumeric()) {
        return trimmed;
    }
    trimmed[..open_idx].trim_end()
}

fn parse_atx_heading(line: &str) -> Option<(usize, String)> {
    let trimmed = line.trim_start();
    if !trimmed.starts_with('#') {
        return None;
    }
    let bytes = trimmed.as_bytes();
    let mut level = 0usize;
    while level < bytes.len() && bytes[level] == b'#' {
        level += 1;
    }
    if level == 0 || level > 6 {
        return None;
    }
    let rest = &trimmed[level..];
    let rest_bytes = rest.as_bytes();
    if rest_bytes.first().is_none_or(|b| !b.is_ascii_whitespace()) {
        return None;
    }
    let mut content = rest.trim();
    if content.is_empty() {
        return None;
    }
    content = content.trim_end_matches('#').trim_end();
    content = strip_trailing_anchor_link(content);
    if content.is_empty() {
        return None;
    }
    Some((level, content.to_string()))
}

fn fence_marker(line: &str) -> Option<char> {
    let trimmed = line.trim_start();
    let mut chars = trimmed.chars();
    let first = chars.next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let mut count = 1usize;
    for ch in chars {
        if ch == first {
            count += 1;
        } else {
            break;
        }
    }
    (count >= 3).then_some(first)
}

fn update_heading_stack(line: &str, in_fence: &mut Option<char>, headings: &mut Vec<Heading>) {
    if let Some(marker) = fence_marker(line) {
        if in_fence.is_some_and(|fence| fence == marker) {
            *in_fence = None;
        } else if in_fence.is_none() {
            *in_fence = Some(marker);
        }
        return;
    }

    if in_fence.is_some() {
        return;
    }

    let trimmed = line.trim_start();
    if trimmed.starts_with('>') {
        return;
    }
    if line.starts_with('\t') || line.starts_with("    ") {
        return;
    }

    let Some((level, text)) = parse_atx_heading(trimmed) else {
        return;
    };
    while headings
        .last()
        .is_some_and(|heading| heading.level >= level)
    {
        headings.pop();
    }
    headings.push(Heading { level, text });
}

fn heading_breadcrumb(headings: &[Heading]) -> String {
    if headings.is_empty() {
        return "root".to_string();
    }
    let joined = headings
        .iter()
        .map(|heading| heading.text.as_str())
        .collect::<Vec<_>>()
        .join(" > ");
    truncate_with_ellipsis(&joined, MATCH_BREADCRUMB_MAX_BYTES)
}

fn match_limit_hint(limit: usize) -> &'static str {
    if limit < MAX_MATCH_LIMIT {
        "use `:matches -n all`, `:goto N`, or `:n`/`:p`"
    } else {
        "use `:goto N` or `:n`/`:p`"
    }
}

fn match_header(matches: usize, limit: usize, more_available: bool) -> String {
    let hint = match_limit_hint(limit);
    if more_available {
        format!(
            "[pager] matches: {} shown (limit {}), more available; {}",
            matches, limit, hint
        )
    } else {
        format!("[pager] matches: {} (limit {}); {}", matches, limit, hint)
    }
}

pub(super) fn take_matches(
    buffer: &PagerBuffer,
    spec: &MatchSpec,
    session: &SearchSession,
) -> (Vec<WorkerContent>, RangeSpan, Vec<(u64, u64)>) {
    if session.hits.is_empty() {
        return (
            vec![WorkerContent::stderr(pattern_not_found_message(
                &session.pattern.pattern,
                buffer.current_offset(),
            ))],
            RangeSpan::default(),
            Vec::new(),
        );
    }

    let more_available = session.hits.len() > spec.limit;
    let entries = session.hits.iter().take(spec.limit);
    let total_lines = line_count(buffer);
    let mut output = String::new();
    let mut span = RangeSpan::default();
    let mut view_ranges = Vec::new();

    for (idx, entry) in entries.enumerate() {
        let label = idx + 1;
        if spec.context == 0 {
            let line = read_line_text(buffer, entry.line_idx);
            let snippet = truncate_with_ellipsis(line.trim_end(), MATCH_LINE_MAX_BYTES);
            output.push_str(&format!(
                "#{label} @{} {} | {snippet}\n",
                entry.match_start, entry.breadcrumb
            ));
            let range = (entry.line_start, entry.line_end);
            span.record(Some(range));
            view_ranges.push(range);
            continue;
        }

        output.push_str(&format!(
            "#{label} @{} {}\n",
            entry.match_start, entry.breadcrumb
        ));

        let start_idx = entry.line_idx.saturating_sub(spec.context);
        let end_idx = (entry.line_idx + spec.context).min(total_lines.saturating_sub(1));
        let mut entry_first = None;
        let mut entry_last = None;
        for line_idx in start_idx..=end_idx {
            let (line_start, line_end) = line_bounds_for_index(buffer, line_idx);
            let line = read_line_text(buffer, line_idx);
            let snippet = truncate_with_ellipsis(line.trim_end(), MATCH_LINE_MAX_BYTES);
            let marker = if line_idx == entry.line_idx { ">" } else { " " };
            output.push_str(&format!("  {marker} {snippet}\n"));
            let range = (line_start, line_end);
            span.record(Some(range));
            entry_first.get_or_insert(range.0);
            entry_last = Some(range.1);
        }
        if let (Some(start), Some(end)) = (entry_first, entry_last) {
            view_ranges.push((start, end));
        }
    }

    (
        vec![
            WorkerContent::stderr(match_header(
                session.hits.len().min(spec.limit),
                spec.limit,
                more_available,
            )),
            WorkerContent::stdout(output),
        ],
        span,
        view_ranges,
    )
}

fn clean_breadcrumb(breadcrumb: &str) -> String {
    breadcrumb
        .replace("[¶]", "")
        .replace("[¶]()", "")
        .trim()
        .to_string()
}

fn first_hit_index_for_offset(hits: &[SearchHit], offset: u64) -> Option<usize> {
    hits.iter().position(|hit| hit.match_start >= offset)
}

fn build_search_hit(
    buffer: &PagerBuffer,
    headings: &mut HeadingState,
    match_offset: u64,
    total_lines: usize,
) -> Option<SearchHit> {
    let line_idx = line_index_for_offset(buffer, match_offset);
    if line_idx >= total_lines {
        return None;
    }
    headings.scan_to(buffer, line_idx);
    let (line_start, line_end) = line_bounds_for_index(buffer, line_idx);
    Some(SearchHit {
        match_start: match_offset,
        line_idx,
        line_start,
        line_end,
        breadcrumb: clean_breadcrumb(&heading_breadcrumb(&headings.headings)),
    })
}

pub(super) fn build_full_search_session(
    buffer: &PagerBuffer,
    pattern: &SearchPattern,
    start_offset: u64,
) -> Option<SearchSession> {
    let end_offset = buffer.len();
    let total_lines = line_count(buffer);
    if end_offset == 0 || total_lines == 0 {
        return None;
    }

    let pattern_bytes = pattern.pattern.as_bytes();
    let mut search_offset = 0u64;
    let mut heading_state = HeadingState::new();
    let mut hits = Vec::new();

    loop {
        let Some(match_offset) = buffer.find_next_bytes_with_options(
            search_offset,
            end_offset,
            pattern_bytes,
            pattern.case_insensitive_ascii,
        ) else {
            break;
        };

        let Some(hit) = build_search_hit(buffer, &mut heading_state, match_offset, total_lines)
        else {
            break;
        };
        hits.push(hit);
        search_offset = match_offset.saturating_add(1);
        if search_offset >= end_offset {
            break;
        }
    }

    if hits.is_empty() {
        return None;
    }

    let current_index = first_hit_index_for_offset(&hits, start_offset)?;

    Some(SearchSession {
        pattern: pattern.clone(),
        current_index,
        hits,
        buffer_len: buffer.len(),
        next_search_offset: end_offset,
        complete: true,
        indexed_from_start: true,
        headings: heading_state,
    })
}

pub(super) fn build_forward_search_session(
    buffer: &PagerBuffer,
    pattern: &SearchPattern,
    start_offset: u64,
) -> Option<SearchSession> {
    let end_offset = buffer.len();
    let total_lines = line_count(buffer);
    if end_offset == 0 || total_lines == 0 || start_offset >= end_offset {
        return None;
    }

    let pattern_bytes = pattern.pattern.as_bytes();
    let match_offset = buffer.find_next_bytes_with_options(
        start_offset,
        end_offset,
        pattern_bytes,
        pattern.case_insensitive_ascii,
    )?;

    let mut heading_state = HeadingState::new();
    let hit = build_search_hit(buffer, &mut heading_state, match_offset, total_lines)?;

    Some(SearchSession {
        pattern: pattern.clone(),
        hits: vec![hit],
        current_index: 0,
        buffer_len: buffer.len(),
        next_search_offset: match_offset.saturating_add(1),
        complete: false,
        indexed_from_start: start_offset == 0,
        headings: heading_state,
    })
}

pub(super) fn session_is_complete(session: &SearchSession) -> bool {
    session.complete
}

pub(super) fn session_is_indexed_from_start(session: &SearchSession) -> bool {
    session.indexed_from_start
}

pub(super) fn current_search_anchor(session: &SearchSession) -> Option<u64> {
    session
        .hits
        .get(session.current_index)
        .map(|hit| hit.match_start)
}

pub(super) fn extend_search_session_forward(
    buffer: &PagerBuffer,
    session: &mut SearchSession,
    needed_hits: usize,
) {
    if session.complete || session.hits.is_empty() {
        return;
    }

    let end_offset = buffer.len();
    let total_lines = line_count(buffer);
    let pattern_bytes = session.pattern.pattern.as_bytes();
    while session.hits.len() < needed_hits {
        let Some(match_offset) = buffer.find_next_bytes_with_options(
            session.next_search_offset,
            end_offset,
            pattern_bytes,
            session.pattern.case_insensitive_ascii,
        ) else {
            session.complete = true;
            session.next_search_offset = end_offset;
            return;
        };
        let Some(hit) = build_search_hit(buffer, &mut session.headings, match_offset, total_lines)
        else {
            session.complete = true;
            session.next_search_offset = end_offset;
            return;
        };
        session.hits.push(hit);
        session.next_search_offset = match_offset.saturating_add(1);
        if session.next_search_offset >= end_offset {
            session.complete = true;
            session.next_search_offset = end_offset;
            return;
        }
    }
}

fn prior_view_message(view_history: &[(u64, u64)], match_start: u64) -> Option<String> {
    view_history
        .iter()
        .rev()
        .find(|(start, end)| *start <= match_start && match_start < *end)
        .map(|(start, end)| format!("[pager] shown earlier @{start}..{end}"))
}

pub(super) fn render_search_card(
    buffer: &PagerBuffer,
    session: &SearchSession,
    view_history: &[(u64, u64)],
) -> (Vec<WorkerContent>, Option<(u64, u64)>) {
    let Some(hit) = session.hits.get(session.current_index) else {
        return (
            vec![WorkerContent::stderr(pattern_not_found_message(
                &session.pattern.pattern,
                buffer.current_offset(),
            ))],
            None,
        );
    };

    let header = if session.indexed_from_start && session.complete {
        format!(
            "[pager] search #{}/{} for `{}` @{}",
            session.current_index + 1,
            session.hits.len(),
            session.pattern.pattern,
            hit.match_start
        )
    } else if session.indexed_from_start {
        format!(
            "[pager] search #{}/? for `{}` @{}",
            session.current_index + 1,
            session.pattern.pattern,
            hit.match_start
        )
    } else {
        format!(
            "[pager] search for `{}` @{}",
            session.pattern.pattern, hit.match_start
        )
    };
    let mut contents = vec![WorkerContent::stderr(header)];
    if let Some(message) = prior_view_message(view_history, hit.match_start) {
        contents.push(WorkerContent::stderr(message));
    }

    let line = read_line_text(buffer, hit.line_idx);
    let snippet = truncate_with_ellipsis(line.trim_end(), MATCH_LINE_MAX_BYTES);
    let body = if hit.breadcrumb == "root" {
        format!("> {snippet}\n")
    } else {
        format!("{}\n> {snippet}\n", hit.breadcrumb)
    };
    contents.push(WorkerContent::stdout(body));
    (contents, Some((hit.line_start, hit.line_end)))
}

pub(super) fn move_search_session(
    session: &mut SearchSession,
    count: u64,
    forward: bool,
) -> SearchStepOutcome {
    if session.hits.is_empty() {
        return SearchStepOutcome::Boundary;
    }
    let count = count.max(1) as usize;
    let last = session.hits.len().saturating_sub(1);
    let old = session.current_index;
    session.current_index = if forward {
        session.current_index.saturating_add(count).min(last)
    } else {
        session.current_index.saturating_sub(count)
    };
    if session.current_index == old {
        SearchStepOutcome::Boundary
    } else {
        SearchStepOutcome::Moved
    }
}

pub(super) fn goto_search_hit(session: &mut SearchSession, index: usize) -> bool {
    if index == 0 || index > session.hits.len() {
        return false;
    }
    session.current_index = index - 1;
    true
}

pub(super) fn search_boundary_message(session: &SearchSession, forward: bool) -> String {
    let edge = if forward { "last" } else { "first" };
    if session.indexed_from_start && session.complete {
        format!(
            "[pager] already at the {edge} search hit #{}/{} for `{}`",
            session.current_index + 1,
            session.hits.len(),
            session.pattern.pattern
        )
    } else if session.indexed_from_start {
        format!(
            "[pager] already at the {edge} known search hit #{} for `{}`",
            session.current_index + 1,
            session.pattern.pattern
        )
    } else {
        format!(
            "[pager] already at the {edge} known search hit for `{}`",
            session.pattern.pattern
        )
    }
}

pub(super) fn take_hits_next(
    buffer: &mut PagerBuffer,
    hit_state: &mut HitState,
    page_bytes: u64,
    count: u64,
    seen: &mut RangeSet,
) -> (Vec<WorkerContent>, u64, RangeSpan, Vec<(u64, u64)>) {
    let count = count.clamp(1, MAX_MATCH_LIMIT as u64);
    let mut output = String::new();
    let mut span = RangeSpan::default();
    let mut view_ranges = Vec::new();
    for _ in 0..count {
        match take_next_hit(buffer, hit_state, seen) {
            HitTakeResult::Found(hit_output) => {
                span.record(hit_output.last_range);
                if let (Some((start, _)), Some((_, end))) =
                    (hit_output.first_range, hit_output.last_range)
                {
                    view_ranges.push((start, end));
                }
                output.push_str(&hit_output.text);
            }
            HitTakeResult::SeenOnly => {
                let pages_left_now = pages_left_for_buffer(buffer, page_bytes);
                if output.is_empty() {
                    return (
                        vec![WorkerContent::stderr(all_matches_shown_message(
                            &hit_state.pattern.pattern,
                        ))],
                        pages_left_now,
                        RangeSpan::default(),
                        Vec::new(),
                    );
                }
                return (
                    vec![WorkerContent::stdout(output)],
                    pages_left_now,
                    span,
                    view_ranges,
                );
            }
            HitTakeResult::NotFound => {
                let pages_left_now = pages_left_for_buffer(buffer, page_bytes);
                if output.is_empty() {
                    let start_offset = buffer.current_offset();
                    return (
                        vec![WorkerContent::stderr(pattern_not_found_message(
                            &hit_state.pattern.pattern,
                            start_offset,
                        ))],
                        pages_left_now,
                        RangeSpan::default(),
                        Vec::new(),
                    );
                }
                return (
                    vec![WorkerContent::stdout(output)],
                    pages_left_now,
                    span,
                    view_ranges,
                );
            }
        }
    }

    let pages_left_now = pages_left_for_buffer(buffer, page_bytes);
    if output.is_empty() {
        (Vec::new(), pages_left_now, RangeSpan::default(), Vec::new())
    } else {
        (
            vec![WorkerContent::stdout(output)],
            pages_left_now,
            span,
            view_ranges,
        )
    }
}

struct HitOutput {
    text: String,
    first_range: Option<(u64, u64)>,
    last_range: Option<(u64, u64)>,
}

enum HitTakeResult {
    Found(HitOutput),
    NotFound,
    SeenOnly,
}

fn take_next_hit(
    buffer: &mut PagerBuffer,
    hit_state: &mut HitState,
    seen: &mut RangeSet,
) -> HitTakeResult {
    let start_offset = buffer.current_offset();
    let end_offset = buffer.len();
    if start_offset >= end_offset {
        return HitTakeResult::NotFound;
    }

    let total_lines = line_count(buffer);
    if total_lines == 0 {
        return HitTakeResult::NotFound;
    }
    let matched = match find_next_unseen_match_line(buffer, start_offset, &hit_state.pattern, seen)
    {
        MatchSearchResult::Found(matched) => matched,
        MatchSearchResult::SeenOnly => return HitTakeResult::SeenOnly,
        MatchSearchResult::NotFound => return HitTakeResult::NotFound,
    };

    let line_idx = matched.line_idx;
    hit_state.headings.scan_to(buffer, line_idx);

    buffer.advance_offset_to(matched.line_end);

    hit_state.hit_index = hit_state.hit_index.saturating_add(1);
    let breadcrumb = heading_breadcrumb(&hit_state.headings.headings);
    let mut output = String::new();
    output.push_str(&format!(
        "#{} @{} {breadcrumb}\n",
        hit_state.hit_index, matched.line_start
    ));

    let mut start_idx = line_idx.saturating_sub(hit_state.context);
    let end_idx = (line_idx + hit_state.context).min(total_lines.saturating_sub(1));
    let mut match_line_already_shown = false;
    if let Some(last) = hit_state.last_emitted_line {
        if last >= line_idx {
            match_line_already_shown = true;
        }
        if start_idx <= last {
            start_idx = last.saturating_add(1);
        }
    }

    if start_idx > end_idx {
        if match_line_already_shown {
            output.push_str("  > (match line already shown)\n");
        }
        return HitTakeResult::Found(HitOutput {
            text: output,
            first_range: None,
            last_range: None,
        });
    }

    let mut last_output_line = None;
    let mut first_range = None;
    let mut last_range = None;
    for idx in start_idx..=end_idx {
        let (line_start, line_end) = line_bounds_for_index(buffer, idx);
        if seen.covers(line_start, line_end) {
            continue;
        }
        let line = read_line_text(buffer, idx);
        let snippet = truncate_with_ellipsis(line.trim_end(), MATCH_LINE_MAX_BYTES);
        let marker = if idx == line_idx { ">" } else { " " };
        output.push_str(&format!("  {marker} {snippet}\n"));
        seen.insert(line_start, line_end);
        last_output_line = Some(idx);
        first_range.get_or_insert((line_start, line_end));
        last_range = Some((line_start, line_end));
    }

    if last_output_line.is_none() && match_line_already_shown {
        output.push_str("  > (match line already shown)\n");
    }

    if let Some(last_line) = last_output_line {
        hit_state.last_emitted_line = Some(match hit_state.last_emitted_line {
            Some(last) => last.max(last_line),
            None => last_line,
        });
    }

    HitTakeResult::Found(HitOutput {
        text: output,
        first_range,
        last_range,
    })
}
