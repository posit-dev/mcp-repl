use crate::worker_protocol::WorkerContent;

use super::RangeSet;

pub(super) fn footer_min(pages_left: u64) -> String {
    if pages_left == 0 {
        "(END)".to_string()
    } else {
        format!("--More-- ({pages_left}p)")
    }
}

pub(super) fn pager_help_text() -> String {
    r#"[pager] quick rules:
  - Pager commands must start with `:`.
  - Empty input shows next page.
  - Non-`:` input exits pager and runs in backend.

[pager] core:
  :q                           quit pager
  :help                        show this help
  :/PATTERN                    start compact search mode for the full buffer
  :n [N]                       jump to next search hit (or next page if no search is active)
  :p [N]                       jump to previous search hit
  :goto N                      jump to the Nth search hit

[pager] navigation:
  :next [N]                    next page (alias for empty input)
  :skip N                      skip N pages, then show next page
  :seek OFFSET | PCT% | line N jump to location
                               OFFSET is UTF-8 char offset; suffixes: k=1024, m=1024^2, b=bytes
  :tail [N|8k|20l]             jump near end (N lines, or bytes with k/m/b) and exit pager
  :range START END             show line range (1-based)
  :a                           show all remaining (deduped)

[pager] search tools:
  :where [-i] PATTERN          show distance to next match (no cursor move)
  :matches [-i] [-n N|all] [-C N] [PATTERN]
  :hits [-i] [-C N] [--count N] PATTERN
"#
    .to_string()
}

pub(super) fn non_command_input_message(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return "[pager] empty command".to_string();
    }
    if trimmed.starts_with(':') {
        return format!("[pager] unrecognized command: {trimmed} (use :help for pager commands)");
    }
    format!("[pager] input blocked while pager is active: {trimmed} (use :q to exit pager)")
}

pub(super) fn position_marker(
    current_offset: u64,
    total_chars: u64,
    last_range: Option<(u64, u64)>,
) -> String {
    if total_chars == 0 {
        return String::new();
    }

    let permille = ((current_offset.saturating_mul(1000)).saturating_div(total_chars)).min(1000);
    let pct_int = permille / 10;
    let pct_dec = permille % 10;

    let range = match last_range {
        Some((start, end)) if end == current_offset => format!("@{start}..{end}/{total_chars}"),
        _ => format!("@{current_offset}/{total_chars}"),
    };

    format!("{pct_int}.{pct_dec}%, {range}")
}

pub(super) fn elision_marker(start: u64, end: u64) -> WorkerContent {
    // Always include the range: without it, multiple elisions in one page are ambiguous for the
    // MCP consumer to stitch back into a consistent view of the underlying output.
    WorkerContent::server_stderr(format!(
        "[pager] elided output (already shown): @{start}..{end}\n"
    ))
}

pub(super) fn gap_marker_if_needed(
    last_range: Option<(u64, u64)>,
    first_range: Option<(u64, u64)>,
    seen: &RangeSet,
) -> Option<WorkerContent> {
    let (_, last_end) = last_range?;
    let (first_start, _) = first_range?;
    if first_start > last_end && seen.covers(last_end, first_start) {
        return Some(elision_marker(last_end, first_start));
    }
    None
}

pub(super) fn truncate_with_ellipsis(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    if max_bytes <= 3 {
        return "...".chars().take(max_bytes).collect();
    }

    let keep_bytes = max_bytes.saturating_sub(3);
    let mut out = String::with_capacity(max_bytes);
    for (idx, ch) in text.char_indices() {
        if idx >= keep_bytes {
            break;
        }
        out.push(ch);
    }
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pager_meta_messages_use_neutral_prefixes() {
        let help = pager_help_text();
        assert!(help.contains("[pager]"));

        let blocked = non_command_input_message("foo");
        assert!(blocked.starts_with("[pager]"));
    }
}
