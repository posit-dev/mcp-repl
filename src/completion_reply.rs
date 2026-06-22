use std::time::Duration;

use crate::backend::Backend;
use crate::ipc::IpcEchoEvent;
use crate::reply_presentation::{
    echo_transcript_from_events, fallback_prompt_variants, normalize_prompt,
    reconcile_completion_prompt, should_drop_echo_only_contents, should_trim_echo_prefix,
    trim_echo_prefix_after_leading_nonstdout_contents, trim_echo_then_append_protocol_warnings,
    trim_leading_input_echo_from_contents, trim_matching_echo_event_suffix_from_contents,
};
use crate::worker_protocol::{WorkerContent, WorkerErrorCode, WorkerReply};

const TIMEOUT_STATUS_GRANULARITY_MS: u64 = 100;

pub(crate) struct InputContext {
    pub(crate) detached_prefix_contents: Vec<WorkerContent>,
    pub(crate) reply_prefix_contents: Vec<WorkerContent>,
    pub(crate) prefix_is_error: bool,
    pub(crate) start_offset: u64,
    pub(crate) prefix_bytes: u64,
    pub(crate) input_echo: Option<String>,
    pub(crate) input_transcript: Option<String>,
}

#[derive(Default)]
pub(crate) struct InputFallback {
    pub(crate) transcript: Option<String>,
    pub(crate) raw_input: Option<String>,
}

pub(crate) struct ReplyWithOffset {
    pub(crate) reply: WorkerReply,
    pub(crate) end_offset: u64,
}

pub(crate) struct CompletionInfo {
    pub(crate) prompt: Option<String>,
    pub(crate) prompt_variants: Option<Vec<String>>,
    pub(crate) echo_events: Vec<IpcEchoEvent>,
    pub(crate) protocol_warnings: Vec<String>,
    pub(crate) session_end_seen: bool,
}

impl CompletionInfo {
    pub(crate) fn empty() -> Self {
        Self {
            prompt: None,
            prompt_variants: None,
            echo_events: Vec::new(),
            protocol_warnings: Vec::new(),
            session_end_seen: false,
        }
    }
}

pub(crate) enum CompletionReplyMode {
    Files {
        fallback_input: InputFallback,
        idle_status_if_empty: bool,
    },
    Pager {
        pager_active: bool,
        fallback_input_transcript: Option<String>,
    },
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
            fallback_input,
            idle_status_if_empty,
        } => {
            finalize_files_contents(&mut contents, completion, fallback_input);
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
        CompletionReplyMode::Pager {
            pager_active,
            fallback_input_transcript,
        } => {
            finalize_pager_contents(&mut contents, completion, fallback_input_transcript);
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

fn finalize_files_contents(
    contents: &mut Vec<WorkerContent>,
    completion: &CompletionInfo,
    fallback_input: InputFallback,
) {
    let fallback_input_transcript = fallback_input.transcript.clone();
    let has_fallback_input_transcript = fallback_input_transcript.is_some();
    let trim_enabled = if completion.echo_events.is_empty() {
        has_fallback_input_transcript
    } else {
        should_trim_echo_prefix(&completion.echo_events)
    };
    let echo_transcript =
        echo_transcript_from_events(&completion.echo_events).or(fallback_input_transcript.clone());
    trim_echo_then_append_protocol_warnings(
        contents,
        echo_transcript.as_deref(),
        trim_enabled,
        if completion.echo_events.is_empty() {
            has_fallback_input_transcript
        } else {
            should_drop_echo_only_contents(&completion.echo_events)
        },
        &completion.protocol_warnings,
    );
    if !trim_enabled {
        let _ = trim_matching_echo_event_suffix_from_contents(contents, &completion.echo_events);
    }
    if completion.echo_events.is_empty() && fallback_input_transcript.is_none() {
        let prompt_variants = fallback_prompt_variants(
            completion.prompt.as_deref(),
            completion.prompt_variants.as_deref(),
        );
        let _ = trim_leading_input_echo_from_contents(
            contents,
            fallback_input.raw_input.as_deref(),
            &prompt_variants,
        );
    }
}

fn finalize_pager_contents(
    contents: &mut Vec<WorkerContent>,
    completion: &CompletionInfo,
    fallback_input_transcript: Option<String>,
) {
    let has_fallback_input_transcript = fallback_input_transcript.is_some();
    let trim_enabled = if completion.echo_events.is_empty() {
        has_fallback_input_transcript
    } else {
        should_trim_echo_prefix(&completion.echo_events)
    };
    let echo_transcript =
        echo_transcript_from_events(&completion.echo_events).or(fallback_input_transcript.clone());
    trim_echo_then_append_protocol_warnings(
        contents,
        echo_transcript.as_deref(),
        trim_enabled,
        if completion.echo_events.is_empty() {
            has_fallback_input_transcript
        } else {
            should_drop_echo_only_contents(&completion.echo_events)
        },
        &completion.protocol_warnings,
    );
    if completion.echo_events.is_empty() {
        let _ = trim_echo_prefix_after_leading_nonstdout_contents(
            contents,
            fallback_input_transcript.as_deref(),
        );
    }
}

fn duration_to_millis(duration: Duration) -> u64 {
    let millis = duration.as_millis();
    if millis > u64::MAX as u128 {
        u64::MAX
    } else {
        millis as u64
    }
}
