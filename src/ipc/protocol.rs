use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::worker_protocol::TextStream;

pub const WORKER_PROTOCOL_VERSION: u32 = 7;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServerToWorkerIpcMessage {
    InputBatch { input: String },
    DiscardPendingInput { discard_id: u64 },
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
    InputLine {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
        text: String,
    },
    InputWait {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
    },
    OutputImage {
        mime_type: String,
        data_b64: String,
        is_update: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    SessionEnd {
        #[serde(default)]
        reason: Option<String>,
        #[serde(default)]
        message: Option<String>,
    },
    DiscardPendingInputAck {
        discard_id: u64,
        discarded_input: bool,
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
pub struct IpcInputLineEvent {
    pub prompt: Option<String>,
    pub line: String,
}

#[derive(Clone)]
pub struct IpcOutputText {
    pub stream: TextStream,
    pub bytes: Vec<u8>,
    pub is_continuation: bool,
}

#[derive(Clone)]
pub struct IpcOutputImage {
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
    pub on_output_image: Option<Arc<dyn Fn(IpcOutputImage) + Send + Sync>>,
    pub on_input_wait: Option<Arc<dyn Fn(Option<String>) + Send + Sync>>,
    pub on_input_line: Option<Arc<dyn Fn(IpcInputLineEvent) + Send + Sync>>,
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
    fn output_image_protocol_uses_update_flag_without_worker_id() {
        let parsed = serde_json::from_value::<WorkerToServerIpcMessage>(json!({
            "type": "output_image",
            "mime_type": "image/png",
            "data_b64": "YWJj",
            "is_update": true,
            "source": "plot-1"
        }));

        let Ok(WorkerToServerIpcMessage::OutputImage {
            mime_type,
            data_b64,
            is_update,
            source,
        }) = parsed
        else {
            panic!("output_image should deserialize");
        };
        assert_eq!(mime_type, "image/png");
        assert_eq!(data_b64, "YWJj");
        assert!(is_update);
        assert_eq!(source.as_deref(), Some("plot-1"));
    }
    #[test]
    fn output_image_protocol_rejects_old_worker_id_and_update_fields() {
        let parsed = serde_json::from_value::<WorkerToServerIpcMessage>(json!({
            "type": "output_image",
            "image_id": "plot-1",
            "mime_type": "image/png",
            "data_b64": "YWJj",
            "update": false
        }));

        assert!(
            parsed.is_err(),
            "output_image should reject old worker-owned image fields"
        );
    }
    #[test]
    fn plot_image_is_not_protocol() {
        let parsed = serde_json::from_value::<WorkerToServerIpcMessage>(json!({
            "type": "plot_image",
            "mime_type": "image/png",
            "data": "abc",
            "is_update": false
        }));

        assert!(parsed.is_err(), "plot_image should not deserialize");
    }
    #[test]
    fn output_image_protocol_rejects_plain_data_payload() {
        let parsed = serde_json::from_value::<WorkerToServerIpcMessage>(json!({
            "type": "output_image",
            "mime_type": "image/png",
            "data": "abc",
            "is_update": false
        }));

        assert!(parsed.is_err(), "output_image should require data_b64");
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
    fn discard_pending_input_ack_protocol_reports_discarded_input() {
        let parsed = serde_json::from_value::<WorkerToServerIpcMessage>(json!({
            "type": "discard_pending_input_ack",
            "discard_id": 7,
            "discarded_input": true
        }));

        let Ok(WorkerToServerIpcMessage::DiscardPendingInputAck {
            discard_id,
            discarded_input,
        }) = parsed
        else {
            panic!("discard_pending_input_ack should deserialize");
        };
        assert_eq!(discard_id, 7);
        assert!(discarded_input);
    }
    #[test]
    fn readline_start_is_not_protocol() {
        let parsed = serde_json::from_value::<WorkerToServerIpcMessage>(json!({
            "type": "readline_start",
            "prompt": "zod> "
        }));

        assert!(parsed.is_err(), "readline_start should not deserialize");
    }
    #[test]
    fn plot_image_protocol_rejects_sequence_ack_handshake() {
        let worker_to_server = serde_json::from_value::<WorkerToServerIpcMessage>(json!({
            "type": "output_image",
            "mime_type": "image/png",
            "data_b64": "YWJj",
            "is_update": false,
            "sequence": 1
        }));
        assert!(
            worker_to_server.is_err(),
            "output_image should not expose worker-side ack sequencing"
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
            "prompt": "debug> "
        }));
        assert!(
            matches!(
                wait,
                Ok(WorkerToServerIpcMessage::InputWait {
                    ref prompt
                }) if prompt.as_deref() == Some("debug> ")
            ),
            "input_wait should deserialize as a worker-to-server message"
        );

        assert!(
            serde_json::from_value::<ServerToWorkerIpcMessage>(json!({
                "type": "input_wait",
                "prompt": "debug> "
            }))
            .is_err(),
            "input_wait should not deserialize as a server-to-worker message"
        );
        assert!(
            serde_json::from_value::<WorkerToServerIpcMessage>(json!({
                "type": "input_wait",
                "input_id": 7,
                "prompt": "debug> "
            }))
            .is_err(),
            "input_wait should reject input_id under v5"
        );
    }

    #[test]
    fn input_wait_null_prompt_is_prompt_free_readiness() {
        let ready = serde_json::from_value::<WorkerToServerIpcMessage>(json!({
            "type": "input_wait",
            "prompt": null
        }));
        assert!(
            matches!(
                ready,
                Ok(WorkerToServerIpcMessage::InputWait { prompt: None })
            ),
            "input_wait with null prompt should deserialize as prompt-free readiness"
        );

        assert!(
            serde_json::from_value::<ServerToWorkerIpcMessage>(json!({
                "type": "ready"
            }))
            .is_err(),
            "ready should not deserialize as a server-to-worker message"
        );
        assert!(
            serde_json::from_value::<WorkerToServerIpcMessage>(json!({
                "type": "ready"
            }))
            .is_err(),
            "ready should not deserialize as a worker-to-server message"
        );
    }

    #[test]
    fn stale_interrupt_messages_are_not_protocol() {
        assert!(
            serde_json::from_value::<ServerToWorkerIpcMessage>(json!({
                "type": "interrupt",
                "interrupt_id": 7
            }))
            .is_err(),
            "interrupt should not deserialize after discard_pending_input"
        );
        assert!(
            serde_json::from_value::<WorkerToServerIpcMessage>(json!({
                "type": "interrupt_ack",
                "interrupt_id": 7
            }))
            .is_err(),
            "interrupt_ack should not deserialize after discard_pending_input_ack"
        );
    }

    #[test]
    fn stale_turn_completion_messages_are_not_protocol() {
        assert!(
            serde_json::from_value::<ServerToWorkerIpcMessage>(json!({
                "type": "stdin_wait",
                "prompt": "debug> "
            }))
            .is_err(),
            "stdin_wait should not deserialize as a server-to-worker message"
        );
        assert!(
            serde_json::from_value::<WorkerToServerIpcMessage>(json!({
                "type": "stdin_wait",
                "prompt": "debug> "
            }))
            .is_err(),
            "stdin_wait should not deserialize as a worker-to-server message"
        );
        assert!(
            serde_json::from_value::<WorkerToServerIpcMessage>(json!({
                "type": "idle",
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
            "input": "x <- 1\n"
        }));
        assert!(
            matches!(
                batch,
                Ok(ServerToWorkerIpcMessage::InputBatch {
                    ref input
                }) if input == "x <- 1\n"
            ),
            "input_batch should deserialize as a server-to-worker message"
        );

        assert!(
            serde_json::from_value::<WorkerToServerIpcMessage>(json!({
                "type": "input_batch",
                "input": "x <- 1\n"
            }))
            .is_err(),
            "input_batch should not deserialize as a worker-to-server message"
        );
        assert!(
            serde_json::from_value::<ServerToWorkerIpcMessage>(json!({
                "type": "input_batch",
                "input_id": 7,
                "input": "x <- 1\n"
            }))
            .is_err(),
            "input_batch should reject input_id under v5"
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

    #[test]
    fn discard_pending_input_carries_server_discard_id() {
        let discard = serde_json::from_value::<ServerToWorkerIpcMessage>(json!({
            "type": "discard_pending_input",
            "discard_id": 7
        }));
        assert!(
            matches!(
                discard,
                Ok(ServerToWorkerIpcMessage::DiscardPendingInput { discard_id: 7 })
            ),
            "discard_pending_input should deserialize with discard_id"
        );

        assert!(
            serde_json::from_value::<ServerToWorkerIpcMessage>(json!({
                "type": "discard_pending_input",
                "input_id": 7
            }))
            .is_err(),
            "discard_pending_input should reject input_id"
        );
        assert!(
            serde_json::from_value::<WorkerToServerIpcMessage>(json!({
                "type": "discard_pending_input"
            }))
            .is_err(),
            "discard_pending_input should not deserialize as a worker-to-server message"
        );
    }
}
