use crate::backend::Backend;
#[cfg(not(target_family = "unix"))]
use crate::worker_protocol::TextStream;
use crate::worker_protocol::{ContentOrigin, WorkerContent};

pub(crate) fn input_echo_text(prompt: &str, line: &str) -> Option<String> {
    if prompt.is_empty() && line.is_empty() {
        return None;
    }
    let mut text = String::with_capacity(prompt.len().saturating_add(line.len()));
    text.push_str(prompt);
    text.push_str(line);
    Some(text)
}

pub(crate) fn normalize_prompt(prompt: Option<String>) -> Option<String> {
    prompt.filter(|value| !value.is_empty())
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

    #[test]
    fn input_echo_text_combines_prompt_and_consumed_line() {
        assert_eq!(input_echo_text("> ", "1+1\n").as_deref(), Some("> 1+1\n"));
    }

    #[test]
    fn input_echo_text_skips_empty_events() {
        assert_eq!(input_echo_text("", ""), None);
    }
}
