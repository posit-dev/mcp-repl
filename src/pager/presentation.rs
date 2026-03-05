use crate::worker_protocol::WorkerContent;

pub(super) fn footer_min(pages_left: u64) -> String {
    if pages_left == 0 {
        "(END)".to_string()
    } else {
        format!("--More-- ({pages_left}p)")
    }
}

pub(super) fn pager_help_text() -> String {
    r#"[pager] core commands:
  <empty>                      next page(s)
  :/pattern                    search forward (use ':/i pattern' for ASCII ignore-case)
  :n [N]                       next page containing last `:/...` pattern
  :a                           show all remaining (deduped)
  :q                           quit pager
  :help                        show this help

[pager] advanced navigation:
  :seek OFFSET | PCT% | line N  (OFFSET is UTF-8 character index; supports k/m suffixes)
  :skip N                      advance without printing
  :where [-i] PATTERN          show how far to next match (no cursor move)
  :matches [-i] [-n N|all] [-C N] PATTERN
  :hits [-i] [-C N] [--count N] PATTERN
  :range START END             show line range (1-based)
  :tail [N|8k]                 go to end (and exit pager)

Non-`:` input auto-dismisses pager and is sent to the backend.
Invalid `:` commands stay in pager; use `:help` for pager commands.
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
    WorkerContent::stderr(format!(
        "[pager] elided output (already shown): @{start}..{end}\n"
    ))
}

pub(super) fn gap_marker_if_needed(
    last_range: Option<(u64, u64)>,
    first_range: Option<(u64, u64)>,
) -> Option<WorkerContent> {
    let (_, last_end) = last_range?;
    let (first_start, _) = first_range?;
    if first_start > last_end {
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
