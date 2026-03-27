use crate::worker_protocol::{TextStream, WorkerContent};

#[derive(Clone, Copy)]
enum LineKind {
    Echo,
    Prompt,
    Output,
}

fn cleanup_echo_only_sequences(
    contents: Vec<WorkerContent>,
    prompt_variants: &[String],
) -> Vec<WorkerContent> {
    struct LineToken {
        content_index: usize,
        line_index: usize,
        kind: LineKind,
    }

    enum Token {
        Line(LineToken),
        Image,
    }

    let mut lines_per_content: Vec<Option<Vec<String>>> = Vec::with_capacity(contents.len());
    let mut keep_per_content: Vec<Option<Vec<bool>>> = Vec::with_capacity(contents.len());
    let mut tokens: Vec<Token> = Vec::new();

    for (idx, content) in contents.iter().enumerate() {
        match content {
            WorkerContent::ContentText { text, stream, .. } => {
                let lines = split_lines(text);
                let mut keep = vec![true; lines.len()];
                for (line_idx, line) in lines.iter().enumerate() {
                    let kind = classify_line(line, *stream, prompt_variants);
                    if matches!(kind, LineKind::Echo) {
                        keep[line_idx] = false;
                    }
                    tokens.push(Token::Line(LineToken {
                        content_index: idx,
                        line_index: line_idx,
                        kind,
                    }));
                }
                lines_per_content.push(Some(lines));
                keep_per_content.push(Some(keep));
            }
            WorkerContent::ContentImage { .. } => {
                lines_per_content.push(None);
                keep_per_content.push(None);
                tokens.push(Token::Image);
            }
        }
    }

    let mut pending_echo: Option<(usize, usize)> = None;
    for token in tokens {
        match token {
            Token::Line(LineToken {
                content_index,
                line_index,
                kind,
            }) => match kind {
                LineKind::Echo => {
                    pending_echo = Some((content_index, line_index));
                }
                LineKind::Prompt => {}
                LineKind::Output => {
                    if let Some((echo_content, echo_line)) = pending_echo.take()
                        && let Some(keep) = keep_per_content[echo_content].as_mut()
                    {
                        keep[echo_line] = true;
                    }
                }
            },
            Token::Image => {
                if let Some((echo_content, echo_line)) = pending_echo.take()
                    && let Some(keep) = keep_per_content[echo_content].as_mut()
                {
                    keep[echo_line] = true;
                }
            }
        }
    }

    let mut cleaned = Vec::with_capacity(contents.len());
    for (idx, content) in contents.into_iter().enumerate() {
        match content {
            WorkerContent::ContentText {
                text: _,
                stream,
                origin,
            } => {
                let Some(lines) = lines_per_content[idx].take() else {
                    continue;
                };
                let keep = keep_per_content[idx].take().unwrap_or_default();
                let mut new_text = String::new();
                for (line, keep) in lines.into_iter().zip(keep) {
                    if keep {
                        new_text.push_str(&line);
                    }
                }
                if !new_text.is_empty() {
                    cleaned.push(WorkerContent::ContentText {
                        text: new_text,
                        stream,
                        origin,
                    });
                }
            }
            other => cleaned.push(other),
        }
    }

    cleaned
}

#[test]
fn repl_tool_descriptions_are_backend_specific() {
    let r = super::repl_tool_description_for_backend(
        crate::backend::Backend::R,
        crate::oversized_output::OversizedOutputMode::Files,
    );
    let python = super::repl_tool_description_for_backend(
        crate::backend::Backend::Python,
        crate::oversized_output::OversizedOutputMode::Files,
    );

    assert_ne!(r, python, "expected backend-specific repl descriptions");
    assert!(r.contains("R code"));
    assert!(python.contains("Python REPL"));
}

#[test]
fn repl_tool_descriptions_include_language_specific_affordances() {
    let r = super::repl_tool_description_for_backend(
        crate::backend::Backend::R,
        crate::oversized_output::OversizedOutputMode::Files,
    );
    let python = super::repl_tool_description_for_backend(
        crate::backend::Backend::Python,
        crate::oversized_output::OversizedOutputMode::Files,
    );

    for description in [r, python] {
        let lower = description.to_lowercase();
        assert!(lower.contains("poll"));
        assert!(lower.contains("large output"));
        assert!(lower.contains("images"));
        assert!(lower.contains("debug"));
    }
    assert!(r.contains("help()"));
    assert!(python.contains("help()"));
}

#[test]
fn repl_tool_descriptions_are_mode_specific() {
    let files = super::repl_tool_description_for_backend(
        crate::backend::Backend::R,
        crate::oversized_output::OversizedOutputMode::Files,
    );
    let pager = super::repl_tool_description_for_backend(
        crate::backend::Backend::R,
        crate::oversized_output::OversizedOutputMode::Pager,
    );

    assert_ne!(files, pager, "expected mode-specific repl descriptions");
    assert!(files.contains("output bundle"));
    assert!(pager.contains("modal pager"));
    assert!(pager.contains(":q"));
}

#[test]
fn timeout_bundle_reuse_treats_blank_lines_as_fresh_input() {
    assert!(matches!(
        super::response::timeout_bundle_reuse_for_input(""),
        super::response::TimeoutBundleReuse::FullReply
    ));
    assert!(matches!(
        super::response::timeout_bundle_reuse_for_input("\n"),
        super::response::TimeoutBundleReuse::FollowUpInput
    ));
    assert!(matches!(
        super::response::timeout_bundle_reuse_for_input("\r\n"),
        super::response::TimeoutBundleReuse::FollowUpInput
    ));
}

fn split_lines(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    text.split_inclusive('\n')
        .map(|line| line.to_string())
        .collect()
}

fn classify_line(line: &str, stream: TextStream, prompt_variants: &[String]) -> LineKind {
    if matches!(stream, TextStream::Stderr) {
        return LineKind::Output;
    }
    let trimmed = line.trim_end_matches(['\n', '\r']);
    if trimmed.is_empty() {
        return LineKind::Output;
    }
    if let Some(prefix_len) = prompt_prefix_len_any(trimmed, prompt_variants) {
        let remainder = trimmed[prefix_len..].trim();
        if remainder.is_empty() {
            LineKind::Prompt
        } else {
            LineKind::Echo
        }
    } else {
        LineKind::Output
    }
}

fn prompt_prefix_len(line: &str, prompt: &str) -> Option<usize> {
    let trimmed = prompt.trim_end_matches(['\n', '\r']);
    if trimmed.is_empty() {
        return None;
    }
    line.starts_with(trimmed).then_some(trimmed.len())
}

fn prompt_prefix_len_any(line: &str, prompt_variants: &[String]) -> Option<usize> {
    for prompt in prompt_variants {
        if let Some(len) = prompt_prefix_len(line, prompt) {
            return Some(len);
        }
    }
    None
}

fn prompt_variants(prompt: &str) -> Vec<String> {
    let trimmed = prompt.trim_end_matches(['\n', '\r']);
    if trimmed.is_empty() {
        return Vec::new();
    }
    let mut variants = vec![trimmed.to_string()];
    if let Some(alt) = swap_prompt_variant(trimmed)
        && alt != trimmed
    {
        variants.push(alt);
    }
    variants
}

fn swap_prompt_variant(prompt: &str) -> Option<String> {
    let core = prompt.trim_end_matches(|ch: char| ch.is_whitespace());
    let suffix = &prompt[core.len()..];
    let swapped_core = if core == ">" {
        Some("+".to_string())
    } else if core == "+" {
        Some(">".to_string())
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

#[test]
fn prompt_prefix_len_handles_variants() {
    assert_eq!(prompt_prefix_len("> x", "> "), Some(2));
    assert_eq!(prompt_prefix_len(">x", ">"), Some(1));
    assert_eq!(prompt_prefix_len("+ x", "+ "), Some(2));
    assert_eq!(prompt_prefix_len("+x", "+"), Some(1));
    assert_eq!(
        prompt_prefix_len("Browse[2]> x", "Browse[2]> "),
        Some("Browse[2]> ".len())
    );
    assert_eq!(
        prompt_prefix_len("Browse[2]+ x", "Browse[2]+ "),
        Some("Browse[2]+ ".len())
    );
    assert_eq!(
        prompt_prefix_len("Browse[12]>x", "Browse[12]>"),
        Some("Browse[12]>".len())
    );
    assert_eq!(prompt_prefix_len(">> not a prompt", "> "), None);
}

#[test]
fn prompt_prefix_len_respects_custom_prompt() {
    assert_eq!(
        prompt_prefix_len(">>>|>>> x", ">>>|>>> "),
        Some(">>>|>>> ".len())
    );
}

#[test]
fn classify_line_detects_echo_and_prompt() {
    let prompt = "> ";
    let prompts = prompt_variants(prompt);
    assert!(matches!(
        classify_line("> x <- 1\n", TextStream::Stdout, &prompts),
        LineKind::Echo
    ));
    assert!(matches!(
        classify_line("> \n", TextStream::Stdout, &prompts),
        LineKind::Prompt
    ));
    assert!(matches!(
        classify_line(
            "Browse[1]> x\n",
            TextStream::Stdout,
            &prompt_variants("Browse[1]> "),
        ),
        LineKind::Echo
    ));
    assert!(matches!(
        classify_line(
            "Browse[1]+ \n",
            TextStream::Stdout,
            &prompt_variants("Browse[1]+ "),
        ),
        LineKind::Prompt
    ));
    assert!(matches!(
        classify_line("> output line\n", TextStream::Stderr, &prompts),
        LineKind::Output
    ));
}

#[test]
fn cleanup_echo_only_sequences_keeps_output_context() {
    let prompt = "> ";
    let contents = vec![WorkerContent::stdout("> x <- 1\n> y <- 2\n[1] 2\n> ")];
    let cleaned = cleanup_echo_only_sequences(contents, &prompt_variants(prompt));
    let text = match &cleaned[0] {
        WorkerContent::ContentText { text, .. } => text.as_str(),
        _ => "",
    };
    assert_eq!(text, "> y <- 2\n[1] 2\n> ");
}

#[test]
fn cleanup_echo_only_sequences_drops_no_output_echoes() {
    let prompt = "> ";
    let contents = vec![WorkerContent::stdout("> x <- 1\n> y <- 2\n> ")];
    let cleaned = cleanup_echo_only_sequences(contents, &prompt_variants(prompt));
    let text = match &cleaned[0] {
        WorkerContent::ContentText { text, .. } => text.as_str(),
        _ => "",
    };
    assert_eq!(text, "> ");
}

#[test]
fn cleanup_echo_only_sequences_keeps_echo_before_image() {
    let prompt = "> ";
    let contents = vec![
        WorkerContent::stdout("> x <- 1\n"),
        WorkerContent::ContentImage {
            data: "abc".to_string(),
            mime_type: "image/png".to_string(),
            id: "plot-1".to_string(),
            is_new: true,
        },
        WorkerContent::stdout("> "),
    ];
    let cleaned = cleanup_echo_only_sequences(contents, &prompt_variants(prompt));
    let text = match &cleaned[0] {
        WorkerContent::ContentText { text, .. } => text.as_str(),
        _ => "",
    };
    assert_eq!(text, "> x <- 1\n");
}

#[test]
fn cleanup_echo_only_sequences_accepts_custom_prompt() {
    let prompt = ">>>|>>> ";
    let contents = vec![WorkerContent::stdout(">>>|>>> x <- 1\n>>>|>>> ")];
    let cleaned = cleanup_echo_only_sequences(contents, &prompt_variants(prompt));
    let text = match &cleaned[0] {
        WorkerContent::ContentText { text, .. } => text.as_str(),
        _ => "",
    };
    assert_eq!(text, ">>>|>>> ");
}

#[test]
fn classify_line_handles_continuation_prompts() {
    let prompts = prompt_variants("> ");
    assert!(matches!(
        classify_line("+ x <- 1\n", TextStream::Stdout, &prompts),
        LineKind::Echo
    ));
    assert!(matches!(
        classify_line("+ \n", TextStream::Stdout, &prompts),
        LineKind::Prompt
    ));
    let browse_prompts = prompt_variants("Browse[2]> ");
    assert!(matches!(
        classify_line("Browse[2]+ x\n", TextStream::Stdout, &browse_prompts),
        LineKind::Echo
    ));
    assert!(matches!(
        classify_line("Browse[2]+ \n", TextStream::Stdout, &browse_prompts),
        LineKind::Prompt
    ));
}
