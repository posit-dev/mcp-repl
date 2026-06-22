use std::time::Duration;

use crate::backend::Backend;
use crate::reply_presentation::{
    append_protocol_warnings, normalize_prompt, reconcile_completion_prompt,
};
use crate::worker_protocol::{WorkerContent, WorkerErrorCode, WorkerReply};

const TIMEOUT_STATUS_GRANULARITY_MS: u64 = 100;

pub(crate) struct InputContext {
    pub(crate) detached_prefix_contents: Vec<WorkerContent>,
    pub(crate) reply_prefix_contents: Vec<WorkerContent>,
    pub(crate) prefix_is_error: bool,
    pub(crate) start_offset: u64,
    pub(crate) prefix_bytes: u64,
}

pub(crate) struct ReplyWithOffset {
    pub(crate) reply: WorkerReply,
    pub(crate) end_offset: u64,
}

pub(crate) struct CompletionInfo {
    pub(crate) prompt: Option<String>,
    pub(crate) prompt_variants: Option<Vec<String>>,
    pub(crate) protocol_warnings: Vec<String>,
    pub(crate) session_end_seen: bool,
}

impl CompletionInfo {
    pub(crate) fn empty() -> Self {
        Self {
            prompt: None,
            prompt_variants: None,
            protocol_warnings: Vec::new(),
            session_end_seen: false,
        }
    }
}

pub(crate) enum CompletionReplyMode {
    Files { idle_status_if_empty: bool },
    Pager { pager_active: bool },
}

pub(crate) struct BuiltCompletionReply {
    pub(crate) reply: ReplyWithOffset,
    pub(crate) prompt_to_remember: Option<String>,
    pub(crate) pager_prompt: Option<PagerCompletionPrompt>,
}

pub(crate) enum PagerCompletionPrompt {
    PromptFree,
    Prompt(String),
}

impl PagerCompletionPrompt {
    pub(crate) fn from_prompt(prompt: Option<String>) -> Self {
        match prompt {
            Some(prompt) => Self::Prompt(prompt),
            None => Self::PromptFree,
        }
    }

    pub(crate) fn into_prompt(self) -> Option<String> {
        match self {
            Self::PromptFree => None,
            Self::Prompt(prompt) => Some(prompt),
        }
    }
}

pub(crate) fn build_completed_reply(
    mut contents: Vec<WorkerContent>,
    is_error: bool,
    end_offset: u64,
    completion: &CompletionInfo,
    session_end: bool,
    mode: CompletionReplyMode,
    backend: Backend,
) -> BuiltCompletionReply {
    let raw_prompt = if session_end {
        None
    } else {
        completion.prompt.clone()
    };
    if raw_prompt.as_deref() == Some("") {
        contents.push(input_wait_status_content());
    }
    let resolved_prompt = normalize_prompt(raw_prompt.clone());

    let (reply_prompt, pager_prompt) = match mode {
        CompletionReplyMode::Files {
            idle_status_if_empty,
        } => {
            finalize_contents(&mut contents, completion);
            if !session_end && idle_status_if_empty && contents.is_empty() {
                contents.push(idle_status_content());
            }
            if !session_end {
                reconcile_completion_prompt(&mut contents, resolved_prompt.clone(), backend);
            }
            (
                (!session_end).then_some(()).and(resolved_prompt.clone()),
                None,
            )
        }
        CompletionReplyMode::Pager { pager_active } => {
            finalize_contents(&mut contents, completion);
            if !session_end && !pager_active {
                reconcile_completion_prompt(&mut contents, resolved_prompt.clone(), backend);
            }
            (
                (!pager_active && !session_end)
                    .then_some(())
                    .and(resolved_prompt.clone()),
                (pager_active && !session_end)
                    .then_some(())
                    .map(|()| PagerCompletionPrompt::from_prompt(resolved_prompt.clone())),
            )
        }
    };

    BuiltCompletionReply {
        reply: ReplyWithOffset {
            reply: WorkerReply::Output {
                contents,
                is_error,
                error_code: None,
                prompt: reply_prompt,
                prompt_variants: completion.prompt_variants.clone(),
            },
            end_offset,
        },
        prompt_to_remember: raw_prompt,
        pager_prompt,
    }
}

pub(crate) fn build_timeout_reply(
    contents: Vec<WorkerContent>,
    is_error: bool,
    end_offset: u64,
) -> ReplyWithOffset {
    ReplyWithOffset {
        reply: WorkerReply::Output {
            contents,
            is_error,
            error_code: Some(WorkerErrorCode::Timeout),
            prompt: None,
            prompt_variants: None,
        },
        end_offset,
    }
}

pub(crate) fn timeout_status_content(timeout: Duration) -> WorkerContent {
    let elapsed_ms = duration_to_millis(timeout);
    let elapsed_ms = (elapsed_ms / TIMEOUT_STATUS_GRANULARITY_MS) * TIMEOUT_STATUS_GRANULARITY_MS;
    WorkerContent::server_stdout(format!(
        "<<repl status: busy, write_stdin timeout reached; elapsed_ms={elapsed_ms}>>"
    ))
}

pub(crate) fn idle_status_content() -> WorkerContent {
    WorkerContent::server_stdout("<<repl status: idle>>")
}

pub(crate) fn input_wait_status_content() -> WorkerContent {
    WorkerContent::server_stdout("<<repl status: waiting for input>>")
}

fn finalize_contents(contents: &mut Vec<WorkerContent>, completion: &CompletionInfo) {
    append_protocol_warnings(contents, &completion.protocol_warnings);
}

fn duration_to_millis(duration: Duration) -> u64 {
    let millis = duration.as_millis();
    if millis > u64::MAX as u128 {
        u64::MAX
    } else {
        millis as u64
    }
}
