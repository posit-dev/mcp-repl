use serde::{Deserialize, Serialize};

pub const WORKER_MODE_ARG: &str = "worker";

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkerErrorCode {
    Busy,
    Timeout,
    InputRejectedPendingOutput,
    InputRejectedBackgroundOutput,
    SessionStartFailed,
    WorkerExecutionFailed,
    Interrupted,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ContentOrigin {
    /// Text that came from the worker REPL and is eligible for transcript files.
    #[default]
    Worker,
    /// Text synthesized by the server, such as timeout or busy-status notices.
    Server,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ContentVisibility {
    #[default]
    ReplyAndTranscript,
    TranscriptOnly,
}

impl ContentVisibility {
    fn is_default(value: &Self) -> bool {
        matches!(value, Self::ReplyAndTranscript)
    }

    pub(crate) fn is_reply_visible(self) -> bool {
        matches!(self, Self::ReplyAndTranscript)
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerContent {
    ContentText {
        text: String,
        stream: TextStream,
        #[serde(default)]
        origin: ContentOrigin,
        #[serde(default, skip_serializing_if = "ContentVisibility::is_default")]
        visibility: ContentVisibility,
    },
    ContentImage {
        data: String,
        mime_type: String,
        id: String,
        is_new: bool,
    },
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TextStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerReply {
    Output {
        contents: Vec<WorkerContent>,
        #[serde(rename = "isError")]
        is_error: bool,
        #[serde(rename = "errorCode", default, skip_serializing_if = "Option::is_none")]
        error_code: Option<WorkerErrorCode>,
        #[serde(rename = "prompt", default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
        #[serde(
            rename = "promptVariants",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        prompt_variants: Option<Vec<String>>,
    },
}

impl WorkerContent {
    #[allow(dead_code)]
    pub fn stdout(text: impl Into<String>) -> Self {
        Self::worker_stdout(text)
    }

    #[allow(dead_code)]
    pub fn stderr(text: impl Into<String>) -> Self {
        Self::worker_stderr(text)
    }

    pub fn worker_stdout(text: impl Into<String>) -> Self {
        WorkerContent::ContentText {
            text: text.into(),
            stream: TextStream::Stdout,
            origin: ContentOrigin::Worker,
            visibility: ContentVisibility::ReplyAndTranscript,
        }
    }

    pub fn worker_stdout_transcript_only(text: impl Into<String>) -> Self {
        WorkerContent::ContentText {
            text: text.into(),
            stream: TextStream::Stdout,
            origin: ContentOrigin::Worker,
            visibility: ContentVisibility::TranscriptOnly,
        }
    }

    #[allow(dead_code)]
    pub fn worker_stderr(text: impl Into<String>) -> Self {
        WorkerContent::ContentText {
            text: text.into(),
            stream: TextStream::Stderr,
            origin: ContentOrigin::Worker,
            visibility: ContentVisibility::ReplyAndTranscript,
        }
    }

    pub fn server_stdout(text: impl Into<String>) -> Self {
        WorkerContent::ContentText {
            text: text.into(),
            stream: TextStream::Stdout,
            origin: ContentOrigin::Server,
            visibility: ContentVisibility::ReplyAndTranscript,
        }
    }

    pub fn server_stderr(text: impl Into<String>) -> Self {
        WorkerContent::ContentText {
            text: text.into(),
            stream: TextStream::Stderr,
            origin: ContentOrigin::Server,
            visibility: ContentVisibility::ReplyAndTranscript,
        }
    }
}
