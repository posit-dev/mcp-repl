use serde::{Deserialize, Serialize};

pub const WORKER_MODE_ARG: &str = "worker";

fn is_false(value: &bool) -> bool {
    !*value
}

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

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerContent {
    ContentText {
        text: String,
        stream: TextStream,
    },
    ContentImage {
        data: String,
        mime_type: String,
        id: String,
        is_new: bool,
    },
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy)]
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
        #[serde(
            rename = "olderOutputDropped",
            default,
            skip_serializing_if = "is_false"
        )]
        older_output_dropped: bool,
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
    pub fn stdout(text: impl Into<String>) -> Self {
        WorkerContent::ContentText {
            text: text.into(),
            stream: TextStream::Stdout,
        }
    }

    pub fn stderr(text: impl Into<String>) -> Self {
        WorkerContent::ContentText {
            text: text.into(),
            stream: TextStream::Stderr,
        }
    }
}
