use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::output_capture::OutputTextSource;
use crate::worker_protocol::TextStream;

pub const WORKER_PROTOCOL_VERSION: u32 = 4;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServerToWorkerIpcMessage {
    InputBatch {
        input_id: u64,
        input: String,
    },
    Interrupt {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_id: Option<u64>,
    },
    Shutdown {},
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerToServerIpcMessage {
    WorkerReady {
        protocol: WorkerProtocol,
        worker: WorkerIdentity,
        capabilities: WorkerCapabilities,
    },
    OutputText {
        stream: TextStream,
        data_b64: String,
        #[serde(default, skip_serializing_if = "is_false")]
        is_continuation: bool,
    },
    ReadlineStart {
        prompt: String,
    },
    InputLine {
        input_id: u64,
        prompt: String,
        text: String,
    },
    InputWait {
        input_id: u64,
        prompt: String,
    },
    PlotImage {
        mime_type: String,
        data: String,
        is_update: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    OutputImage {
        image_id: String,
        mime_type: String,
        data_b64: String,
        update: bool,
    },
    SessionEnd {
        #[serde(default)]
        reason: Option<String>,
        #[serde(default)]
        message_b64: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_id: Option<u64>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerProtocol {
    pub name: String,
    pub version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerIdentity {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerCapabilities {
    #[serde(default)]
    pub images: bool,
}

#[derive(Debug, Clone)]
pub struct IpcEchoEvent {
    pub prompt: String,
    pub line: String,
    pub source: OutputTextSource,
}

#[derive(Clone)]
pub struct IpcOutputText {
    pub stream: TextStream,
    pub bytes: Vec<u8>,
    pub is_continuation: bool,
}

#[derive(Clone)]
pub struct IpcPlotImage {
    pub id: String,
    pub mime_type: String,
    pub data: String,
    pub is_new: bool,
    pub updates_previous_image: bool,
    pub readline_results_seen: usize,
}

#[derive(Default, Clone)]
pub struct IpcHandlers {
    pub on_output_text: Option<Arc<dyn Fn(IpcOutputText) + Send + Sync>>,
    pub on_plot_image: Option<Arc<dyn Fn(IpcPlotImage) + Send + Sync>>,
    pub on_readline_start: Option<Arc<dyn Fn(String) + Send + Sync>>,
    pub on_readline_result: Option<Arc<dyn Fn(IpcEchoEvent) + Send + Sync>>,
    pub on_session_end: Option<Arc<dyn Fn() + Send + Sync>>,
}

pub(crate) fn worker_ready_message(
    worker_name: &str,
    supports_images: bool,
) -> WorkerToServerIpcMessage {
    WorkerToServerIpcMessage::WorkerReady {
        protocol: WorkerProtocol {
            name: "mcp-repl-worker".to_string(),
            version: WORKER_PROTOCOL_VERSION,
        },
        worker: WorkerIdentity {
            name: worker_name.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
        capabilities: WorkerCapabilities {
            images: supports_images,
        },
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[cfg(test)]
mod tests {
    use super::{ServerToWorkerIpcMessage, WorkerToServerIpcMessage};
    use crate::worker_protocol::TextStream;
    use base64::Engine as _;
    use serde_json::json;

    #[test]
    fn builtin_worker_ready_uses_current_protocol_version() {
        let WorkerToServerIpcMessage::WorkerReady { protocol, .. } =
            super::worker_ready_message("r", true)
        else {
            panic!("worker_ready_message should create worker_ready");
        };
        assert_eq!(protocol.version, super::WORKER_PROTOCOL_VERSION);
    }
    #[test]
    fn plot_image_protocol_uses_update_flag_without_worker_id() {
        let parsed = serde_json::from_value::<WorkerToServerIpcMessage>(json!({
            "type": "plot_image",
            "mime_type": "image/png",
            "data": "abc",
            "is_update": true
        }));

        assert!(
            parsed.is_ok(),
            "plot_image should not require worker image id"
        );
    }
    #[test]
    fn plot_image_protocol_rejects_worker_id_and_is_new() {
        let parsed = serde_json::from_value::<WorkerToServerIpcMessage>(json!({
            "type": "plot_image",
            "id": "plot-1",
            "mime_type": "image/png",
            "data": "abc",
            "is_new": true,
            "is_update": false
        }));

        assert!(
            parsed.is_err(),
            "plot_image should reject old worker-owned image fields"
        );
    }
    #[test]
    fn output_text_protocol_uses_stream_and_base64_payload() {
        let parsed = serde_json::from_value::<WorkerToServerIpcMessage>(json!({
            "type": "output_text",
            "stream": "stdout",
            "data_b64": "YWxwaGE="
        }));

        let Ok(WorkerToServerIpcMessage::OutputText {
            stream,
            data_b64,
            is_continuation,
        }) = parsed
        else {
            panic!("output_text should deserialize");
        };
        assert_eq!(stream, TextStream::Stdout);
        assert_eq!(data_b64, "YWxwaGE=");
        assert!(
            !is_continuation,
            "output_text continuation should default to false"
        );
    }
    #[test]
    fn output_text_protocol_rejects_plain_data_payload() {
        let parsed = serde_json::from_value::<WorkerToServerIpcMessage>(json!({
            "type": "output_text",
            "stream": "stdout",
            "data": "alpha"
        }));

        assert!(parsed.is_err(), "output_text should require data_b64");
    }
    #[test]
    fn readline_start_protocol_only_carries_prompt() {
        let value = serde_json::to_value(WorkerToServerIpcMessage::ReadlineStart {
            prompt: "zod> ".to_string(),
        })
        .expect("serialize readline_start");

        assert_eq!(
            value,
            json!({
                "type": "readline_start",
                "prompt": "zod> "
            })
        );
    }
    #[test]
    fn plot_image_protocol_rejects_sequence_ack_handshake() {
        let worker_to_server = serde_json::from_value::<WorkerToServerIpcMessage>(json!({
            "type": "plot_image",
            "mime_type": "image/png",
            "data": "abc",
            "is_update": false,
            "sequence": 1
        }));
        assert!(
            worker_to_server.is_err(),
            "plot_image should not expose worker-side ack sequencing"
        );

        let server_to_worker = serde_json::from_value::<ServerToWorkerIpcMessage>(json!({
            "type": "plot_image_ack",
            "sequence": 1
        }));
        assert!(
            server_to_worker.is_err(),
            "server-to-worker protocol should not include plot_image_ack"
        );
    }
    #[test]
    fn request_end_is_not_part_of_worker_to_server_protocol() {
        let parsed = serde_json::from_value::<WorkerToServerIpcMessage>(json!({
            "type": "request_end"
        }));

        assert!(parsed.is_err(), "request_end should not deserialize");
    }
    #[test]
    fn stale_stdin_write_control_messages_are_not_protocol() {
        assert!(
            serde_json::from_value::<ServerToWorkerIpcMessage>(json!({
                "type": "request_start"
            }))
            .is_err(),
            "request_start should not deserialize after v3 demand feeding"
        );
        assert!(
            serde_json::from_value::<ServerToWorkerIpcMessage>(json!({
                "type": "stdin_write",
                "byte_len": 1
            }))
            .is_err(),
            "stdin_write should not deserialize after v3 demand feeding"
        );
        assert!(
            serde_json::from_value::<ServerToWorkerIpcMessage>(json!({
                "type": "stdin_write_complete"
            }))
            .is_err(),
            "stdin_write_complete should not deserialize after v3 demand feeding"
        );
        assert!(
            serde_json::from_value::<WorkerToServerIpcMessage>(json!({
                "type": "stdin_write_ack"
            }))
            .is_err(),
            "stdin_write_ack should not deserialize after v3 demand feeding"
        );
    }
    #[test]
    fn builtin_python_request_generation_messages_are_not_protocol() {
        assert!(
            serde_json::from_value::<ServerToWorkerIpcMessage>(json!({
                "type": "python_request_start",
                "request_generation": 7,
                "stdin_b64": "aW5wdXQNCg=="
            }))
            .is_err(),
            "python_request_start should not deserialize after v3 demand feeding"
        );
        assert!(
            serde_json::from_value::<ServerToWorkerIpcMessage>(json!({
                "type": "python_interrupt",
                "request_generation": 7
            }))
            .is_err(),
            "python_interrupt should not deserialize after v3 interrupt unification"
        );
        assert!(
            serde_json::from_value::<WorkerToServerIpcMessage>(json!({
                "type": "python_interrupt_ack"
            }))
            .is_err(),
            "python_interrupt_ack should not deserialize after v3 interrupt unification"
        );
    }
    #[test]
    fn input_wait_message_is_worker_to_server_only() {
        let wait = serde_json::from_value::<WorkerToServerIpcMessage>(json!({
            "type": "input_wait",
            "input_id": 7,
            "prompt": "debug> "
        }));
        assert!(
            matches!(
                wait,
                Ok(WorkerToServerIpcMessage::InputWait {
                    input_id: 7,
                    ref prompt
                }) if prompt == "debug> "
            ),
            "input_wait should deserialize as a worker-to-server message"
        );

        assert!(
            serde_json::from_value::<ServerToWorkerIpcMessage>(json!({
                "type": "input_wait",
                "input_id": 7,
                "prompt": "debug> "
            }))
            .is_err(),
            "input_wait should not deserialize as a server-to-worker message"
        );
    }

    #[test]
    fn stale_turn_completion_messages_are_not_protocol() {
        assert!(
            serde_json::from_value::<ServerToWorkerIpcMessage>(json!({
                "type": "stdin_wait",
                "input_id": 7,
                "prompt": "debug> "
            }))
            .is_err(),
            "stdin_wait should not deserialize as a server-to-worker message"
        );
        assert!(
            serde_json::from_value::<WorkerToServerIpcMessage>(json!({
                "type": "stdin_wait",
                "input_id": 7,
                "prompt": "debug> "
            }))
            .is_err(),
            "stdin_wait should not deserialize as a worker-to-server message"
        );
        assert!(
            serde_json::from_value::<WorkerToServerIpcMessage>(json!({
                "type": "idle",
                "input_id": 7,
                "prompt": "debug> "
            }))
            .is_err(),
            "idle should not deserialize as a worker-to-server message"
        );
        assert!(
            serde_json::from_value::<WorkerToServerIpcMessage>(json!({
                "type": "turn_input",
                "input_id": 7,
                "input": "c\n"
            }))
            .is_err(),
            "turn_input should not deserialize as a worker-to-server message"
        );
        assert!(
            serde_json::from_value::<ServerToWorkerIpcMessage>(json!({
                "type": "turn_input",
                "input_id": 7,
                "input": "c\n"
            }))
            .is_err(),
            "turn_input should not deserialize as a server-to-worker message"
        );
    }

    #[test]
    fn input_batch_is_server_to_worker_only() {
        let batch = serde_json::from_value::<ServerToWorkerIpcMessage>(json!({
            "type": "input_batch",
            "input_id": 7,
            "input": "x <- 1\n"
        }));
        assert!(
            matches!(
                batch,
                Ok(ServerToWorkerIpcMessage::InputBatch {
                    input_id: 7,
                    ref input
                }) if input == "x <- 1\n"
            ),
            "input_batch should deserialize as a server-to-worker message"
        );

        assert!(
            serde_json::from_value::<WorkerToServerIpcMessage>(json!({
                "type": "input_batch",
                "input_id": 7,
                "input": "x <- 1\n"
            }))
            .is_err(),
            "input_batch should not deserialize as a worker-to-server message"
        );
        assert!(
            serde_json::from_value::<ServerToWorkerIpcMessage>(json!({
                "type": "turn_start",
                "turn_id": 7,
                "input": "x <- 1\n"
            }))
            .is_err(),
            "turn_start should not deserialize after v4 input_batch rename"
        );
    }

    #[test]
    fn shutdown_is_server_to_worker_lifecycle_control() {
        let shutdown = serde_json::from_value::<ServerToWorkerIpcMessage>(json!({
            "type": "shutdown"
        }));
        assert!(
            matches!(shutdown, Ok(ServerToWorkerIpcMessage::Shutdown {})),
            "shutdown should deserialize as a server-to-worker message"
        );

        assert!(
            serde_json::from_value::<WorkerToServerIpcMessage>(json!({
                "type": "shutdown"
            }))
            .is_err(),
            "shutdown should not deserialize as a worker-to-server message"
        );
        assert!(
            serde_json::from_value::<ServerToWorkerIpcMessage>(json!({
                "type": "shutdown",
                "input_id": 1
            }))
            .is_err(),
            "shutdown should not carry input payload"
        );
    }
}
