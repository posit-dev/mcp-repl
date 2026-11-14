//! Optional local integrator transport. MCP still owns the process lifetime.
//! Named requests share the MCP session registry and its lifecycle owners.

use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::time::Duration;

use rmcp::model::{CallToolResponse, CallToolResult, ContentBlock, RequestId};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, watch};
use tokio::task::{JoinHandle, JoinSet};

use super::ConsoleServer;
use crate::worker_client::{
    Content, IntegratorError, IntegratorObservation, IntegratorRestart, Response,
};

const MAX_FRAME: usize = 1024 * 1024;
const MAX_CONNECTIONS: usize = 64;

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Sessions {
        id: String,
    },
    Create {
        id: String,
        session: String,
    },
    Close {
        id: String,
        session: String,
    },
    Status {
        id: String,
        #[serde(default = "default_session")]
        session: String,
    },
    Preview {
        id: String,
        #[serde(default = "default_session")]
        session: String,
    },
    Output {
        id: String,
        #[serde(default = "default_session")]
        session: String,
    },
    Subscribe {
        id: String,
        #[serde(default = "default_session")]
        session: String,
        #[serde(default)]
        format: SubscriptionFormat,
    },
    Stdin {
        id: String,
        #[serde(default = "default_session")]
        session: String,
        evaluation_id: u64,
        input: String,
    },
    Interrupt {
        id: String,
        #[serde(default = "default_session")]
        session: String,
        evaluation_id: u64,
    },
    Restart {
        id: String,
        #[serde(default = "default_session")]
        session: String,
    },
    Prepare {
        id: String,
        #[serde(default = "default_session")]
        session: String,
        requirements: super::arguments::Requirements,
    },
    Requirements {
        id: String,
        #[serde(default = "default_session")]
        session: String,
    },
    Manifest {
        id: String,
        #[serde(default = "default_session")]
        session: String,
    },
    Recording {
        id: String,
        #[serde(default = "default_session")]
        session: String,
    },
    Evaluate {
        id: String,
        #[serde(default = "default_session")]
        session: String,
        language: crate::cell::Language,
        code: String,
        #[serde(default)]
        execution: crate::worker_client::IntegratorExecution,
        #[serde(default)]
        busy: Busy,
        wait_ms: Option<u32>,
        #[serde(default = "record_by_default")]
        record: bool,
        #[serde(default)]
        include_output: bool,
    },
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SubscriptionFormat {
    #[default]
    Preview,
    Events,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Busy {
    #[default]
    Reject,
    Wait,
}

fn default_session() -> String {
    "default".into()
}

fn record_by_default() -> bool {
    true
}

impl Request {
    fn id(&self) -> &str {
        match self {
            Self::Sessions { id }
            | Self::Create { id, .. }
            | Self::Close { id, .. }
            | Self::Status { id, .. }
            | Self::Preview { id, .. }
            | Self::Output { id, .. }
            | Self::Subscribe { id, .. }
            | Self::Stdin { id, .. }
            | Self::Interrupt { id, .. }
            | Self::Restart { id, .. }
            | Self::Prepare { id, .. }
            | Self::Requirements { id, .. }
            | Self::Manifest { id, .. }
            | Self::Recording { id, .. }
            | Self::Evaluate { id, .. } => id,
        }
    }

    fn session(&self) -> &str {
        match self {
            Self::Sessions { .. } => "default",
            Self::Create { session, .. }
            | Self::Close { session, .. }
            | Self::Status { session, .. }
            | Self::Preview { session, .. }
            | Self::Output { session, .. }
            | Self::Subscribe { session, .. }
            | Self::Stdin { session, .. }
            | Self::Interrupt { session, .. }
            | Self::Restart { session, .. }
            | Self::Prepare { session, .. }
            | Self::Requirements { session, .. }
            | Self::Manifest { session, .. }
            | Self::Recording { session, .. }
            | Self::Evaluate { session, .. } => session,
        }
    }
}

pub(super) struct Listener {
    listener: UnixListener,
    path: PathBuf,
    identity: (u64, u64),
}

impl Listener {
    pub fn bind(path: PathBuf) -> io::Result<Self> {
        let path = std::path::absolute(path)?;
        let parent = std::fs::metadata(path.parent().expect("absolute socket path"))?;
        // macOS does not enforce socket mode bits on connect. Require a private
        // parent before binding so no permissive creation window is exposed.
        if !parent.is_dir()
            || parent.uid() != unsafe { libc::geteuid() }
            || parent.mode() & 0o077 != 0
        {
            return Err(io::Error::other(
                "control socket requires a private parent directory owned by the current user (mode 0700)",
            ));
        }
        // Never unlink an existing path, including a stale socket.
        let listener = UnixListener::bind(&path).map_err(|error| {
            if error.kind() == io::ErrorKind::AddrInUse {
                io::Error::new(error.kind(), "control socket path already exists")
            } else {
                error
            }
        })?;
        let metadata = std::fs::symlink_metadata(&path)?;
        let bound = Self {
            listener,
            path,
            identity: (metadata.dev(), metadata.ino()),
        };
        std::fs::set_permissions(&bound.path, std::fs::Permissions::from_mode(0o600))?;
        Ok(bound)
    }

    pub fn start(self, server: ConsoleServer) -> Server {
        server.startup.runtime().worker.enable_integrator_output();
        let (closed, mut closing) = watch::channel(false);
        let failed = closed.clone();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    biased;
                    _ = closing.changed() => break,
                    Some(result) = connections.join_next() => {
                        if let Err(error) = result {
                            eprintln!("control connection task failed: {error}");
                        }
                    }
                    accepted = self.listener.accept() => match accepted {
                        Ok((stream, _)) if connections.len() < MAX_CONNECTIONS => {
                            connections.spawn(connection(stream, server.clone(), closing.clone()));
                        }
                        Ok(_) => {},
                        Err(error) => {
                            eprintln!("control socket accept failed: {error}");
                            let _ = failed.send(true);
                            break;
                        }
                    }
                }
            }
            while connections.join_next().await.is_some() {}
            // Keep the path owner alive until all handlers have recorded results.
            drop(self);
        });
        Server { closed, task }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        if std::fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| (metadata.dev(), metadata.ino()) == self.identity)
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub(super) struct Server {
    closed: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl Server {
    pub fn close(&self) {
        let _ = self.closed.send(true);
    }

    pub async fn finish(self) {
        if let Err(error) = self.task.await {
            eprintln!("control socket task failed: {error}");
        }
    }
}

async fn connection(stream: UnixStream, server: ConsoleServer, mut closing: watch::Receiver<bool>) {
    let (read, mut write) = stream.into_split();
    let (frames, mut received) = mpsc::channel(16);
    let (disconnected, mut disconnect) = watch::channel(false);
    let mut reader_closing = closing.clone();
    let incoming = server.startup.runtime().transcript.clone();
    let reader = tokio::spawn(async move {
        let mut read = BufReader::new(read);
        let mut activated = false;
        loop {
            // As at MCP receipt, hold intake to the recording writer's pace.
            tokio::select! {
                biased;
                _ = reader_closing.changed() => break,
                _ = frames.closed() => break,
                () = incoming.paced() => {}
            }
            let mut frame = Vec::new();
            let mut bounded = (&mut read).take((MAX_FRAME + 1) as u64);
            let result = tokio::select! {
                biased;
                _ = reader_closing.changed() => break,
                _ = frames.closed() => break,
                result = bounded.read_until(b'\n', &mut frame) => result,
            };
            if !frame.is_empty() {
                if !activated {
                    incoming.activate();
                    activated = true;
                }
                incoming.protocol(
                    "integrator_to_server",
                    serde_json::from_slice(&frame).unwrap_or_else(|_| {
                        Value::String(String::from_utf8_lossy(&frame).into_owned())
                    }),
                );
            }
            match result {
                Ok(0) | Err(_) => break,
                Ok(_) if frame.len() > MAX_FRAME || !frame.ends_with(b"\n") => break,
                Ok(_) => {
                    if frames.try_send(frame).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = disconnected.send(true);
    });
    loop {
        let frame = tokio::select! {
            biased;
            _ = closing.changed() => break,
            frame = received.recv() => match frame {
                Some(frame) => frame,
                None => break,
            },
        };
        let parsed = serde_json::from_slice::<Request>(&frame);
        let transcript = server
            .startup
            .runtime()
            .transcript
            .for_session(parsed.as_ref().map_or("default", Request::session));
        let subscription = match &parsed {
            Ok(Request::Subscribe {
                id,
                format,
                session,
            }) => server.sessions.get(session).map(|startup| {
                let worker = startup.runtime().worker.clone();
                Subscription {
                    id: id.clone(),
                    session: session.clone(),
                    format: *format,
                    changes: worker.integrator_subscribe(),
                    worker,
                    recording: transcript.clone(),
                }
            }),
            _ => None,
        };
        let (response, call_id) = match parsed {
            Ok(request) => handle(&server, request, &mut closing, &mut disconnect).await,
            Err(error) => (
                failure(Value::Null, "invalid_request", error.to_string()),
                None,
            ),
        };
        // The canonical result owns image artifacts. Keep transport evidence
        // correlated without duplicating potentially large base64 payloads.
        transcript.protocol("server_to_integrator", match call_id {
            Some(call_id) => json!({"id": response["id"], "tool_result_call_id": call_id, "error": response.get("error"), "execution_error": response["result"].get("execution_error")}),
            None => response.clone(),
        });
        let mut bytes = serde_json::to_vec(&response).expect("control response serialization");
        bytes.push(b'\n');
        let delivered = if *closing.borrow() || *disconnect.borrow() {
            false
        } else {
            tokio::select! {
                biased;
                _ = closing.changed() => false,
                _ = disconnect.changed() => false,
                result = write.write_all(&bytes) => result.is_ok(),
            }
        };
        transcript.delivery(&response["id"], call_id, delivered);
        if !delivered {
            break;
        }
        if let Some(subscription) = subscription.filter(|_| response.get("error").is_none()) {
            stream_snapshots(
                subscription,
                &mut write,
                &mut closing,
                &mut disconnect,
                &mut received,
            )
            .await;
            break;
        }
    }
    drop(received);
    let _ = reader.await;
}

struct Subscription {
    id: String,
    session: String,
    worker: crate::worker_client::Client,
    format: SubscriptionFormat,
    changes: watch::Receiver<()>,
    recording: crate::transcript::Transcript,
}

async fn stream_snapshots(
    subscription: Subscription,
    write: &mut tokio::net::unix::OwnedWriteHalf,
    closing: &mut watch::Receiver<bool>,
    disconnect: &mut watch::Receiver<bool>,
    received: &mut mpsc::Receiver<Vec<u8>>,
) {
    let Subscription {
        id,
        session,
        worker,
        format,
        mut changes,
        recording,
    } = subscription;
    let mut previous = None;
    // The recording writer changes what a preview says about retained files
    // without a worker change.
    let mut recorded = recording.subscribe_writer();
    loop {
        changes.borrow_and_update();
        recorded.borrow_and_update();
        let mut event = match format {
            SubscriptionFormat::Preview => {
                let Ok(content) = worker.integrator_preview(recording.writer_state()) else {
                    break;
                };
                let result = CallToolResult::success(content_blocks(content));
                json!({"id": id, "event": "preview", "result": result})
            }
            SubscriptionFormat::Events => json!({
                "id": id, "event": "output",
                "result": worker.integrator_output(),
            }),
        };
        if session != "default" {
            event["session"] = json!(session);
        }
        if previous.as_ref() != Some(&event) {
            let mut bytes = serde_json::to_vec(&event).expect("preview serialization");
            bytes.push(b'\n');
            if *closing.borrow() || *disconnect.borrow() {
                break;
            }
            let delivered = tokio::select! {
                biased;
                _ = closing.changed() => false,
                _ = disconnect.changed() => false,
                _ = received.recv() => false,
                result = write.write_all(&bytes) => result.is_ok(),
            };
            if !delivered {
                break;
            }
            previous = Some(event);
        }
        tokio::select! {
            biased;
            _ = closing.changed() => break,
            _ = disconnect.changed() => break,
            _ = received.recv() => break,
            result = changes.changed() => if result.is_err() { break; },
            result = recorded.changed() => if result.is_err() { break; },
        }
    }
}

fn failure(id: Value, code: &str, message: impl Into<String>) -> Value {
    json!({"id": id, "error": {"code": code, "message": message.into()}})
}

async fn handle(
    server: &ConsoleServer,
    request: Request,
    closing: &mut watch::Receiver<bool>,
    disconnect: &mut watch::Receiver<bool>,
) -> (Value, Option<u64>) {
    let id = Value::String(request.id().to_string());
    let session = request.session().to_owned();
    // Reject invalid admission policy before a new name can reserve capacity.
    let wait = match &request {
        Request::Evaluate { busy, wait_ms, .. } => match (busy, wait_ms) {
            (Busy::Reject, None) => None,
            (Busy::Wait, Some(milliseconds)) => Some(Duration::from_millis((*milliseconds).into())),
            _ => {
                return (
                    failure(
                        id,
                        "invalid_request",
                        "wait_ms is required with busy=wait and forbidden with busy=reject",
                    ),
                    None,
                );
            }
        },
        _ => None,
    };
    let creates = matches!(
        request,
        Request::Create { .. }
            | Request::Evaluate {
                execution: crate::worker_client::IntegratorExecution::Serial,
                ..
            }
    );
    if creates && server.inheritance.enabled() && !server.inheritance.captured() {
        return (
            failure(id, "not_ready", "waiting for MCP client permissions"),
            None,
        );
    }
    let startup = match server.sessions.select(&session, creates) {
        Ok(startup) => startup,
        Err(error) => {
            return (
                failure(
                    id,
                    if creates {
                        "execution_failed"
                    } else {
                        "unknown_session"
                    },
                    error,
                ),
                None,
            );
        }
    };
    if creates {
        // Like MCP admission, install the independent startup owner before
        // yielding to recording or observing a control-socket disconnect.
        let started = (|| {
            if session != "default" || matches!(request, Request::Create { .. }) {
                startup.retry_failed()?;
            }
            startup.start()
        })();
        if let Err(error) = started {
            return (failure(id, "execution_failed", error), None);
        }
    }
    let runtime = startup.runtime();
    let evaluation = matches!(&request, Request::Evaluate { .. });
    let output_snapshot = matches!(&request, Request::Output { .. });
    let mut execution_error = Value::Null;
    let mut typed_output = None;
    let arguments = match &request {
        Request::Evaluate { language, code, .. } => serde_json::Map::from_iter([(
            crate::cell::Languages::field(*language).into(),
            json!(code),
        )]),
        _ => serde_json::Map::new(),
    };
    let record = matches!(&request, Request::Evaluate { record: true, .. });
    let recording = runtime.transcript.clone();
    let request_id = RequestId::String(request.id().to_string().into());
    let call = recording.begin_integrator(&request_id, &arguments, record);
    let call_id = call.id();
    let operation_call = call.clone();
    let operation = async {
        match request {
            Request::Sessions { .. } => Ok(CallToolResult::structured(json!({
                "sessions": server.sessions.snapshot().into_iter().map(|(name, startup)| {
                    let worker = startup.runtime().worker.clone();
                    json!({"name": name, "state": worker.session_status(), "worker_id": worker.integrator_worker_id()})
                }).collect::<Vec<_>>()
            }))),
            Request::Create { .. } => {
                startup
                    .ready()
                    .await
                    .map_err(|error| ("execution_failed", error))?;
                Ok(CallToolResult::structured(
                    json!({"session": session, "state": runtime.worker.session_status(), "worker_id": runtime.worker.integrator_worker_id()}),
                ))
            }
            Request::Status { .. } => Ok(CallToolResult::structured(
                runtime.worker.integrator_status(),
            )),
            Request::Output { .. } => Ok(CallToolResult::structured(
                runtime.worker.integrator_output(),
            )),
            Request::Subscribe { .. } => {
                Ok(CallToolResult::structured(json!({"subscribed": true})))
            }
            Request::Stdin {
                evaluation_id,
                input,
                ..
            } => {
                runtime
                    .worker
                    .integrator_stdin(evaluation_id, input)
                    .map_err(control_error)?;
                Ok(CallToolResult::structured(json!({"accepted": true})))
            }
            Request::Interrupt { evaluation_id, .. } => {
                let target = runtime
                    .worker
                    .select_integrator_interrupt(evaluation_id)
                    .map_err(control_error)?;
                tokio::task::spawn_blocking(move || target.interrupt())
                    .await
                    .expect("integrator interrupt task")
                    .map_err(control_error)?;
                Ok(CallToolResult::structured(json!({"accepted": true})))
            }
            Request::Restart { .. } | Request::Close { .. } => {
                let closing_session = matches!(request, Request::Close { .. });
                if closing_session && session == "default" {
                    return Err((
                        "invalid_request",
                        "the default session cannot be closed; use restart".into(),
                    ));
                }
                if !runtime.worker.is_configured() {
                    return Err(("not_ready", "runtime configuration is not ready".into()));
                }
                if server.inheritance.enabled() && !server.inheritance.captured() {
                    return Err(("not_ready", "waiting for MCP client permissions".into()));
                }
                runtime
                    .worker
                    .integrator_restart(if closing_session {
                        IntegratorRestart::Stop
                    } else {
                        IntegratorRestart::Replace
                    })
                    .await
                    .map(CallToolResult::structured)
                    .map_err(control_error)
            }
            Request::Prepare { requirements, .. } => {
                if requirements.action != crate::worker_client::RequirementsAction::Add {
                    return Err((
                        "invalid_request",
                        "prepare supports only requirements.action=add".into(),
                    ));
                }
                if !runtime.worker.is_configured() {
                    return Err(("not_ready", "runtime configuration is not ready".into()));
                }
                if server.inheritance.enabled() && !server.inheritance.captured() {
                    return Err(("not_ready", "waiting for MCP client permissions".into()));
                }
                Ok(CallToolResult::structured(
                    runtime
                        .worker
                        .integrator_prepare(requirements.into_worker(operation_call.id()))
                        .await,
                ))
            }
            Request::Recording { .. } => {
                // This reply describes the recording itself, so it alone
                // waits for the recording writer.
                let recording = runtime.transcript.clone();
                tokio::task::spawn_blocking(move || recording.integrator_recording())
                    .await
                    .expect("integrator recording task")
                    .map(CallToolResult::structured)
                    .map_err(|error| ("projection_unavailable", error))
            }
            Request::Preview { .. } => {
                let writer = runtime.transcript.writer_state();
                let content = runtime.worker.integrator_preview(writer).map_err(|_| {
                    (
                        "busy",
                        "runner state is changing; retry preview".to_string(),
                    )
                })?;
                record_images(&content, &operation_call, &runtime.transcript);
                Ok(CallToolResult::success(content_blocks(content)))
            }
            Request::Requirements { .. } | Request::Manifest { .. }
                if !runtime.worker.is_configured() =>
            {
                Err((
                    "not_ready",
                    "runtime configuration is not ready".to_string(),
                ))
            }
            Request::Requirements { .. } => Ok(CallToolResult::structured(
                runtime.worker.inspect_requirements(),
            )),
            Request::Manifest { .. } => Ok(CallToolResult::structured(
                runtime.worker.inspect_requirements()["requirements"].clone(),
            )),
            Request::Evaluate {
                language,
                code,
                execution,
                include_output,
                record,
                id: request_id,
                ..
            } => {
                if server.inheritance.enabled() && !server.inheritance.captured() {
                    return Err(("not_ready", "waiting for MCP client permissions".into()));
                }
                let cell = crate::cell::Cell {
                    language,
                    source: code,
                };
                let (response, evaluation_id, output) = match execution {
                    crate::worker_client::IntegratorExecution::Serial => {
                        runtime
                            .worker
                            .integrator_evaluate(
                                cell,
                                wait,
                                runtime.transcript.clone(),
                                operation_call.id(),
                                disconnect.clone(),
                                IntegratorObservation {
                                    request_id,
                                    hidden: !record,
                                },
                            )
                            .await
                    }
                    crate::worker_client::IntegratorExecution::Immediate => {
                        if wait.is_some() {
                            return Err((
                                "invalid_request",
                                "immediate execution requires busy=reject and no wait_ms".into(),
                            ));
                        }
                        runtime
                            .worker
                            .integrator_priority_evaluate(
                                cell,
                                runtime.transcript.clone(),
                                &operation_call,
                                disconnect.clone(),
                                request_id,
                            )
                            .await
                    }
                }
                .map_err(control_error)?;
                if include_output {
                    typed_output = Some((evaluation_id, output));
                }
                execution_error = serde_json::to_value(response.execution_error())
                    .expect("execution error serialization");
                Ok(evaluation_result(response, &operation_call, &runtime.transcript).await)
            }
        }
    };
    let result = if *closing.borrow() || *disconnect.borrow() {
        drop(operation);
        Err(("connection_closed", "control connection closed".to_string()))
    } else {
        operation.await
    };
    let response = match &result {
        Ok(result) => {
            let mut result = result.structured_content.clone().unwrap_or_else(|| {
                serde_json::to_value(result).expect("tool result serialization")
            });
            if evaluation {
                result
                    .as_object_mut()
                    .expect("evaluation result object")
                    .insert("execution_error".into(), execution_error);
            }
            if let Some((evaluation_id, output)) = typed_output {
                result["evaluation_id"] = json!(evaluation_id);
                result["output"] = output;
            }
            json!({"id": id, "result": result})
        }
        Err((code, message)) => failure(id, code, message),
    };
    let recorded: Result<CallToolResponse, rmcp::ErrorData> = result
        .map(|result| {
            if output_snapshot {
                // Snapshot payloads are observations of already captured source
                // and artifacts. Record a receipt, not another inline image/text
                // copy that bypasses their existing retention allowances.
                let snapshot = result.structured_content.as_ref().expect("output snapshot");
                CallToolResult::structured(json!({"output_snapshot": {
                    "worker_id": snapshot["worker_id"],
                    "first_sequence": snapshot["first_sequence"],
                    "next_sequence": snapshot["next_sequence"],
                    "omitted_events": snapshot["omitted_events"],
                    "omitted_bytes": snapshot["omitted_bytes"],
                }}))
                .into()
            } else {
                result.into()
            }
        })
        .map_err(|(_, message)| rmcp::ErrorData::internal_error(message, None));
    runtime.transcript.finish(call, &recorded);
    (response, call_id)
}

fn control_error(error: IntegratorError) -> (&'static str, String) {
    match error {
        IntegratorError::Busy => ("busy", "runner is busy; code was not run".into()),
        IntegratorError::Timeout => (
            "timeout",
            "runner did not become idle before the deadline; code was not run".into(),
        ),
        IntegratorError::Failed(message) => ("execution_failed", message),
        IntegratorError::Cancelled => (
            "connection_closed",
            "control connection closed; code was not run".into(),
        ),
        IntegratorError::StaleEvaluation => {
            ("stale_evaluation", "evaluation is no longer active".into())
        }
    }
}

fn content_blocks(content: Vec<Content>) -> Vec<ContentBlock> {
    content
        .into_iter()
        .map(|content| match content {
            Content::Text(text) => ContentBlock::text(text),
            Content::Image {
                data, mime_type, ..
            } => ContentBlock::image(data, mime_type),
        })
        .collect()
}

async fn evaluation_result(
    mut response: Response,
    call: &crate::transcript::Call,
    transcript: &crate::transcript::Transcript,
) -> CallToolResult {
    response.settle_recording(transcript, &mut []).await;
    let (content, is_error, delivery, parts) = response.into_parts();
    call.record_result_parts(parts);
    record_images(&content, call, transcript);
    // A private response is never recovered into MCP, even when its socket closes.
    if let Some(delivery) = delivery {
        delivery.delivered();
    }
    if is_error {
        CallToolResult::error(content_blocks(content))
    } else {
        CallToolResult::success(content_blocks(content))
    }
}

fn record_images(
    content: &[Content],
    call: &crate::transcript::Call,
    transcript: &crate::transcript::Transcript,
) {
    let images = content
        .iter()
        .filter_map(|content| match content {
            Content::Image { artifact, .. } => Some(artifact.clone()),
            Content::Text(_) => None,
        })
        .collect();
    if let Err(error) = call.record_result_images(images) {
        transcript.disable(error);
    }
}
