use crate::backend::Backend;
use crate::ipc::IpcEchoEvent;
use crate::worker_protocol::{ContentOrigin, TextStream, WorkerContent};

pub(crate) fn echo_transcript_from_events(events: &[IpcEchoEvent]) -> Option<String> {
    if !should_trim_echo_prefix(events) {
        return None;
    }
    let mut transcript = String::new();
    for event in events {
        if !is_visible_echo_event(event) {
            continue;
        }
        transcript.push_str(&event.prompt);
        transcript.push_str(&event.line);
    }
    Some(transcript)
}

pub(crate) fn should_trim_echo_prefix(events: &[IpcEchoEvent]) -> bool {
    events.iter().any(is_visible_echo_event)
}

pub(crate) fn should_drop_echo_only_contents(events: &[IpcEchoEvent]) -> bool {
    events.iter().any(is_visible_echo_event)
}

fn is_visible_echo_event(event: &IpcEchoEvent) -> bool {
    event.source == crate::output_capture::OutputTextSource::Raw
}

pub(crate) fn maybe_trim_echo_prefix(
    contents: &mut Vec<WorkerContent>,
    echo_prefix: Option<&str>,
    trim_enabled: bool,
) {
    if !trim_enabled {
        return;
    }
    let Some(echo_prefix) = echo_prefix else {
        return;
    };
    let _ = trim_matching_echo_prefix_from_contents(contents, echo_prefix);
}

fn trim_matching_echo_prefix_from_contents(
    contents: &mut Vec<WorkerContent>,
    echo_prefix: &str,
) -> bool {
    if echo_prefix.is_empty() {
        return false;
    }

    let mut remaining = echo_prefix;
    for content in contents.iter() {
        if remaining.is_empty() {
            break;
        }
        let WorkerContent::ContentText { text, stream, .. } = content else {
            return false;
        };
        if !matches!(stream, TextStream::Stdout) {
            return false;
        }
        if remaining.len() >= text.len() {
            if !remaining.starts_with(text.as_str()) {
                return false;
            }
            remaining = &remaining[text.len()..];
        } else {
            if !text.starts_with(remaining) {
                return false;
            }
            remaining = "";
        }
    }

    if !remaining.is_empty() {
        return false;
    }

    let mut remaining = echo_prefix;
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
            _ => return false,
        };

        if remove_current {
            contents.remove(idx);
            continue;
        }
        idx = idx.saturating_add(1);
    }

    true
}

pub(crate) fn trim_matching_echo_event_suffix_from_contents(
    contents: &mut Vec<WorkerContent>,
    echo_events: &[IpcEchoEvent],
) -> bool {
    for start in 0..echo_events.len() {
        if !should_drop_echo_only_contents(&echo_events[start..]) {
            continue;
        }
        let Some(echo_prefix) = echo_transcript_from_events(&echo_events[start..]) else {
            continue;
        };
        if trim_matching_echo_prefix_from_contents(contents, &echo_prefix) {
            return true;
        }
    }
    false
}

pub(crate) fn trim_echo_prefix_after_leading_nonstdout_contents(
    contents: &mut Vec<WorkerContent>,
    echo_prefix: Option<&str>,
) -> bool {
    let Some(echo_prefix) = echo_prefix else {
        return false;
    };
    if echo_prefix.is_empty() {
        return false;
    }

    let start_idx = contents
        .iter()
        .position(|content| {
            matches!(
                content,
                WorkerContent::ContentText {
                    stream: TextStream::Stdout,
                    origin: ContentOrigin::Worker,
                    ..
                }
            )
        })
        .unwrap_or(contents.len());
    if start_idx >= contents.len() {
        return false;
    }

    let mut remaining = echo_prefix;
    for content in contents.iter().skip(start_idx) {
        if remaining.is_empty() {
            break;
        }
        let WorkerContent::ContentText {
            text,
            stream,
            origin,
        } = content
        else {
            return false;
        };
        if !matches!(stream, TextStream::Stdout) || !matches!(origin, ContentOrigin::Worker) {
            return false;
        }
        if remaining.len() >= text.len() {
            if !remaining.starts_with(text.as_str()) {
                return false;
            }
            remaining = &remaining[text.len()..];
        } else {
            if !text.starts_with(remaining) {
                return false;
            }
            remaining = "";
        }
    }

    if !remaining.is_empty() {
        return false;
    }

    let mut idx = start_idx;
    let mut remaining = echo_prefix;
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
            _ => return false,
        };

        if remove_current {
            contents.remove(idx);
            continue;
        }
        idx = idx.saturating_add(1);
    }

    true
}

pub(crate) fn drop_echo_only_contents(contents: &mut Vec<WorkerContent>, echo: &str) -> bool {
    if echo.is_empty() {
        return false;
    }

    let mut remaining = echo;
    for content in contents.iter() {
        let WorkerContent::ContentText {
            text,
            stream,
            origin,
        } = content
        else {
            return false;
        };
        if !matches!(stream, TextStream::Stdout) || !matches!(origin, ContentOrigin::Worker) {
            return false;
        }
        if remaining.len() >= text.len() {
            if !remaining.starts_with(text.as_str()) {
                return false;
            }
            remaining = &remaining[text.len()..];
        } else {
            return false;
        }
    }

    if !remaining.is_empty() {
        return false;
    }

    contents.clear();
    true
}

pub(crate) fn trim_echo_then_append_protocol_warnings(
    contents: &mut Vec<WorkerContent>,
    echo: Option<&str>,
    trim_enabled: bool,
    drop_echo_only_enabled: bool,
    warnings: &[String],
) {
    maybe_trim_echo_prefix(contents, echo, trim_enabled);
    if drop_echo_only_enabled && let Some(echo) = echo {
        let _ = drop_echo_only_contents(contents, echo);
    }
    append_protocol_warnings(contents, warnings);
}

pub(crate) fn normalize_prompt(prompt: Option<String>) -> Option<String> {
    prompt.filter(|value| !value.is_empty())
}

fn normalize_input_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

pub(crate) fn fallback_prompt_variants(
    prompt: Option<&str>,
    prompt_variants: Option<&[String]>,
) -> Vec<String> {
    let mut variants = Vec::new();
    if let Some(prompt_variants) = prompt_variants {
        for prompt in prompt_variants {
            push_fallback_prompt_variant(&mut variants, prompt);
        }
    }
    if let Some(prompt) = prompt {
        push_fallback_prompt_variant(&mut variants, prompt);
    }
    variants
}

fn push_fallback_prompt_variant(variants: &mut Vec<String>, prompt: &str) {
    let prompt = prompt.trim_end_matches(['\n', '\r']);
    if prompt.is_empty() {
        return;
    }
    if !variants.iter().any(|existing| existing == prompt) {
        variants.push(prompt.to_string());
    }
    if let Some(alt) = swap_fallback_prompt_variant(prompt)
        && alt != prompt
        && !variants.iter().any(|existing| existing == &alt)
    {
        variants.push(alt);
    }
}

fn swap_fallback_prompt_variant(prompt: &str) -> Option<String> {
    let core = prompt.trim_end_matches(|ch: char| ch.is_whitespace());
    let suffix = &prompt[core.len()..];
    let swapped_core = if core == ">" {
        Some("+".to_string())
    } else if core == "+" {
        Some(">".to_string())
    } else if core == ">>>" {
        Some("...".to_string())
    } else if core == "..." {
        Some(">>>".to_string())
    } else if core.starts_with("Browse[") && (core.ends_with('>') || core.ends_with('+')) {
        let mut swapped = core.to_string();
        let last = swapped.pop()?;
        let replacement = match last {
            '>' => '+',
            '+' => '>',
            _ => return None,
        };
        swapped.push(replacement);
        Some(swapped)
    } else {
        None
    };
    swapped_core.map(|core| format!("{core}{suffix}"))
}

pub(crate) fn build_input_transcript(prompt: Option<&str>, input: &str) -> Option<String> {
    let prompt = prompt?;
    let normalized = normalize_input_newlines(input);
    let trimmed = normalized.trim_end_matches('\n').trim_end();
    if trimmed.is_empty() || trimmed.contains('\n') {
        return None;
    }
    Some(format!("{prompt}{trimmed}\n"))
}

fn trim_line_endings(text: &str) -> &str {
    text.trim_end_matches(['\n', '\r'])
}

fn line_matches_input_echo(line: &str, input_line: &str, prompt_variants: &[String]) -> bool {
    let line = trim_line_endings(line);
    if input_line.is_empty() {
        return line.is_empty() || prompt_variants.iter().any(|prompt| line == prompt);
    }
    if line == input_line {
        return true;
    }
    prompt_variants.iter().any(|prompt| {
        line.strip_prefix(prompt)
            .is_some_and(|rest| rest == input_line)
    })
}

fn trim_leading_text_prefix(contents: &mut Vec<WorkerContent>, mut prefix_bytes: usize) -> bool {
    if prefix_bytes == 0 {
        return false;
    }
    let mut idx = 0usize;
    while idx < contents.len() && prefix_bytes > 0 {
        let remove_current = match &mut contents[idx] {
            WorkerContent::ContentText {
                text,
                stream,
                origin,
            } if matches!(stream, TextStream::Stdout)
                && matches!(origin, ContentOrigin::Worker) =>
            {
                if prefix_bytes >= text.len() {
                    prefix_bytes -= text.len();
                    text.clear();
                    true
                } else {
                    if !text.is_char_boundary(prefix_bytes) {
                        return false;
                    }
                    *text = text[prefix_bytes..].to_string();
                    prefix_bytes = 0;
                    false
                }
            }
            _ => break,
        };
        if remove_current {
            contents.remove(idx);
        } else {
            idx = idx.saturating_add(1);
        }
    }
    prefix_bytes == 0
}

pub(crate) fn trim_leading_input_echo_from_contents(
    contents: &mut Vec<WorkerContent>,
    input: Option<&str>,
    prompt_variants: &[String],
) -> bool {
    let Some(input) = input else {
        return false;
    };
    let normalized_input = normalize_input_newlines(input);
    let trimmed_input = normalized_input.trim_end_matches('\n');
    if trimmed_input.is_empty() {
        return false;
    }
    let input_lines: Vec<&str> = trimmed_input.split('\n').collect();
    let last_nonempty_input = input_lines
        .iter()
        .rev()
        .find(|line| !line.is_empty())
        .copied();

    let mut leading_text = String::new();
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
        leading_text.push_str(text);
    }
    if leading_text.is_empty() {
        return false;
    }

    let output_lines: Vec<&str> = leading_text.split_inclusive('\n').collect();
    let mut output_idx = 0usize;
    let mut input_idx = 0usize;
    let mut trim_bytes = 0usize;

    while output_idx < output_lines.len() && input_idx < input_lines.len() {
        let line = output_lines[output_idx];
        if !line_matches_input_echo(line, input_lines[input_idx], prompt_variants) {
            break;
        }
        trim_bytes += line.len();
        output_idx += 1;
        input_idx += 1;
    }
    if input_idx != input_lines.len() {
        return false;
    }

    while output_idx < output_lines.len() {
        let line = trim_line_endings(output_lines[output_idx]);
        let matches_prompt_only = prompt_variants.iter().any(|prompt| line == prompt);
        let matches_last_duplicate = last_nonempty_input.is_some_and(|last| {
            prompt_variants
                .iter()
                .any(|prompt| line.strip_prefix(prompt).is_some_and(|rest| rest == last))
        });
        if !matches_prompt_only && !matches_last_duplicate {
            break;
        }
        trim_bytes += output_lines[output_idx].len();
        output_idx += 1;
    }

    trim_leading_text_prefix(contents, trim_bytes)
}

pub(crate) fn append_protocol_warnings(contents: &mut Vec<WorkerContent>, warnings: &[String]) {
    for warning in warnings {
        contents.push(WorkerContent::server_stderr(format!("[repl] {warning}")));
    }
}

pub(crate) fn append_prompt_if_missing(contents: &mut Vec<WorkerContent>, prompt: Option<String>) {
    let Some(prompt) = prompt else {
        return;
    };
    if prompt.is_empty() {
        return;
    }
    if let Some(WorkerContent::ContentText { text, .. }) = contents
        .iter()
        .rev()
        .find(|content| matches!(content, WorkerContent::ContentText { .. }))
        && text.ends_with(&prompt)
    {
        return;
    }
    contents.push(WorkerContent::worker_stdout(prompt));
}

fn append_prompt(contents: &mut Vec<WorkerContent>, prompt: Option<String>) {
    let Some(prompt) = prompt else {
        return;
    };
    if prompt.is_empty() {
        return;
    }
    contents.push(WorkerContent::worker_stdout(prompt));
}

pub(crate) fn reconcile_completion_prompt(
    contents: &mut Vec<WorkerContent>,
    prompt: Option<String>,
    backend: Backend,
) {
    match backend {
        Backend::Python => reconcile_python_completion_prompt(contents, prompt),
        Backend::R => append_prompt(contents, prompt),
    }
}

pub(crate) fn reconcile_trailing_completion_prompt(
    contents: &mut Vec<WorkerContent>,
    prompt: Option<String>,
    backend: Backend,
) {
    match backend {
        Backend::Python => reconcile_python_completion_prompt(contents, prompt),
        Backend::R => append_prompt(contents, prompt),
    }
}

pub(crate) fn reconcile_polled_completion_prompt(
    contents: &mut Vec<WorkerContent>,
    prompt: Option<String>,
    backend: Backend,
) {
    match backend {
        Backend::Python => reconcile_python_polled_completion_prompt(contents, prompt),
        Backend::R => append_prompt_if_missing(contents, prompt),
    }
}

#[cfg(target_family = "unix")]
fn reconcile_python_completion_prompt(contents: &mut Vec<WorkerContent>, prompt: Option<String>) {
    append_prompt(contents, prompt);
}

#[cfg(target_family = "unix")]
fn reconcile_python_polled_completion_prompt(
    contents: &mut Vec<WorkerContent>,
    prompt: Option<String>,
) {
    append_prompt_if_missing(contents, prompt);
}

#[cfg(not(target_family = "unix"))]
fn reconcile_python_completion_prompt(contents: &mut Vec<WorkerContent>, prompt: Option<String>) {
    if let Some(prompt_text) = prompt.as_deref() {
        strip_trailing_worker_stdout_prompt(contents, prompt_text);
    }
    append_prompt_if_missing(contents, prompt);
}

#[cfg(not(target_family = "unix"))]
fn reconcile_python_polled_completion_prompt(
    contents: &mut Vec<WorkerContent>,
    prompt: Option<String>,
) {
    if let Some(prompt_text) = prompt.as_deref() {
        strip_trailing_worker_stdout_prompt(contents, prompt_text);
    }
    append_prompt_if_missing(contents, prompt);
}

pub(crate) fn strip_trailing_prompt(contents: &mut Vec<WorkerContent>, prompt: &str) {
    if prompt.is_empty() {
        return;
    }
    let idx = contents
        .iter()
        .rposition(|content| matches!(content, WorkerContent::ContentText { .. }));
    let Some(idx) = idx else {
        return;
    };
    let WorkerContent::ContentText { text, stream, .. } = &contents[idx] else {
        return;
    };
    let Some(prefix) = text.strip_suffix(prompt) else {
        return;
    };
    if prefix.is_empty() {
        contents.remove(idx);
    } else {
        contents[idx] = WorkerContent::ContentText {
            text: prefix.to_string(),
            stream: *stream,
            origin: ContentOrigin::Worker,
        };
    }
}

#[cfg(not(target_family = "unix"))]
fn strip_trailing_worker_stdout_prompt(contents: &mut Vec<WorkerContent>, prompt: &str) {
    if prompt.is_empty() {
        return;
    }

    let mut idx = contents.len();
    while idx > 0 {
        idx = idx.saturating_sub(1);
        match &contents[idx] {
            WorkerContent::ContentText {
                origin: ContentOrigin::Server,
                ..
            } => continue,
            WorkerContent::ContentText {
                text,
                stream: TextStream::Stdout,
                origin: ContentOrigin::Worker,
            } => {
                let Some(prefix) = text.strip_suffix(prompt) else {
                    return;
                };
                if prefix.is_empty() {
                    contents.remove(idx);
                } else {
                    contents[idx] = WorkerContent::ContentText {
                        text: prefix.to_string(),
                        stream: TextStream::Stdout,
                        origin: ContentOrigin::Worker,
                    };
                }
                return;
            }
            WorkerContent::ContentText {
                origin: ContentOrigin::Worker,
                ..
            }
            | WorkerContent::ContentImage { .. } => return,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output_capture::OutputTextSource;

    fn echo_event(prompt: &str, line: &str) -> IpcEchoEvent {
        IpcEchoEvent {
            prompt: prompt.to_string(),
            line: line.to_string(),
            source: OutputTextSource::Ipc,
        }
    }

    fn raw_echo_event(prompt: &str, line: &str) -> IpcEchoEvent {
        IpcEchoEvent {
            prompt: prompt.to_string(),
            line: line.to_string(),
            source: OutputTextSource::Raw,
        }
    }

    fn contents_text(contents: &[WorkerContent]) -> String {
        contents
            .iter()
            .filter_map(|content| match content {
                WorkerContent::ContentText { text, .. } => Some(text.as_str()),
                WorkerContent::ContentImage { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    #[test]
    fn trims_echo_prefix_across_text_chunks() {
        let mut contents = vec![
            WorkerContent::stdout("> x <- 1\n"),
            WorkerContent::stdout("> y <- 2\n[1] 2\n"),
        ];
        maybe_trim_echo_prefix(&mut contents, Some("> x <- 1\n> y <- 2\n"), true);
        let text = match &contents[0] {
            WorkerContent::ContentText { text, .. } => text.as_str(),
            WorkerContent::ContentImage { .. } => "",
        };
        assert_eq!(text, "[1] 2\n");
    }

    #[test]
    fn does_not_trim_on_mismatch() {
        let mut contents = vec![WorkerContent::stdout("> x <- 1\n[1] 1\n")];
        maybe_trim_echo_prefix(&mut contents, Some("> y <- 2\n"), true);
        let text = match &contents[0] {
            WorkerContent::ContentText { text, .. } => text.as_str(),
            WorkerContent::ContentImage { .. } => "",
        };
        assert_eq!(text, "> x <- 1\n[1] 1\n");
    }

    #[test]
    fn does_not_trim_when_leading_stderr() {
        let mut contents = vec![
            WorkerContent::stderr("stderr: boom\n"),
            WorkerContent::stdout("> x <- 1\n[1] 1\n"),
        ];
        maybe_trim_echo_prefix(&mut contents, Some("> x <- 1\n"), true);
        let text = match &contents[0] {
            WorkerContent::ContentText { text, .. } => text.as_str(),
            WorkerContent::ContentImage { .. } => "",
        };
        assert_eq!(text, "stderr: boom\n");
    }

    #[test]
    fn trim_echo_then_append_protocol_warnings_drops_echo_only_multiline_input() {
        let warning = "late readline result".to_string();
        let echo = "> x <- 1\n> y <- 2\n";
        let mut contents = vec![WorkerContent::stdout(echo)];

        trim_echo_then_append_protocol_warnings(
            &mut contents,
            Some(echo),
            false,
            true,
            std::slice::from_ref(&warning),
        );

        assert_eq!(
            contents,
            vec![WorkerContent::server_stderr(format!("[repl] {warning}"))]
        );
    }

    #[test]
    fn trim_echo_then_append_protocol_warnings_keeps_output_before_warning() {
        let warning = "late readline result".to_string();
        let mut contents = vec![WorkerContent::stdout("> x <- 1\n[1] 1\n")];

        trim_echo_then_append_protocol_warnings(
            &mut contents,
            Some("> x <- 1\n"),
            true,
            true,
            std::slice::from_ref(&warning),
        );

        assert_eq!(
            contents,
            vec![
                WorkerContent::stdout("[1] 1\n"),
                WorkerContent::server_stderr(format!("[repl] {warning}")),
            ]
        );
    }

    #[test]
    fn trim_echo_prefix_after_leading_nonstdout_contents_removes_prompt_fallback_echo() {
        let mut contents = vec![
            WorkerContent::stderr("stderr: Error: object 'x' not found\n"),
            WorkerContent::stdout("> x\n"),
            WorkerContent::stdout("> "),
        ];

        let trimmed =
            trim_echo_prefix_after_leading_nonstdout_contents(&mut contents, Some("> x\n"));

        assert!(trimmed, "expected prompt fallback echo to be trimmed");
        assert_eq!(
            contents,
            vec![
                WorkerContent::stderr("stderr: Error: object 'x' not found\n"),
                WorkerContent::stdout("> "),
            ]
        );
    }

    #[test]
    fn trim_decision_applies_to_any_visible_echo() {
        let structural = vec![echo_event("> ", "1+1\n")];
        assert!(!should_trim_echo_prefix(&structural));

        let single = vec![raw_echo_event("> ", "1+1\n")];
        assert!(should_trim_echo_prefix(&single));

        let continuation = vec![raw_echo_event("> ", "1+\n"), raw_echo_event("+ ", "1\n")];
        assert!(should_trim_echo_prefix(&continuation));

        let multi = vec![raw_echo_event("> ", "1+1\n"), raw_echo_event("> ", "2+2\n")];
        assert!(should_trim_echo_prefix(&multi));

        let browser = vec![raw_echo_event("Browse[1]> ", "n\n")];
        assert!(should_trim_echo_prefix(&browser));

        let readline = vec![raw_echo_event("FIRST> ", "alpha\n")];
        assert!(should_trim_echo_prefix(&readline));
    }

    #[test]
    fn trim_matching_echo_event_suffix_from_contents_trims_late_top_level_echo() {
        let mut contents = vec![WorkerContent::worker_stdout(
            "> cat(\"TAIL_ONLY\\n\")\nTAIL_ONLY\n> ",
        )];

        let trimmed = trim_matching_echo_event_suffix_from_contents(
            &mut contents,
            &[
                raw_echo_event("> ", "cat(\"HEAD_ONLY\\n\")\n"),
                raw_echo_event("> ", "flush.console()\n"),
                raw_echo_event("> ", "cat(\"TAIL_ONLY\\n\")\n"),
            ],
        );

        assert!(trimmed, "expected late top-level echo to be trimmed");
        assert_eq!(contents_text(&contents), "TAIL_ONLY\n> ");
    }

    #[test]
    fn trim_matching_echo_event_suffix_from_contents_keeps_unmatched_prompt_tail() {
        let mut contents = vec![WorkerContent::worker_stdout("FIRST> alpha\nSECOND> ")];

        let trimmed = trim_matching_echo_event_suffix_from_contents(
            &mut contents,
            &[
                raw_echo_event("FIRST> ", "alpha\n"),
                raw_echo_event("SECOND> ", "beta\n"),
            ],
        );

        assert!(
            !trimmed,
            "did not expect partial prompt transcript to be trimmed without an exact match"
        );
        assert_eq!(contents_text(&contents), "FIRST> alpha\nSECOND> ");
    }
}
