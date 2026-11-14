//! One Console process's capture and derived documents. Before send, capture
//! lives in temporary storage; publication never changes event time or ownership.
//!
//! Logical session names, generations, and recording worker IDs are distinct.
//! Source admission can precede acquisition or finish without execution; only a
//! worker cell-entry receipt opens its numbered output interval. Delayed source,
//! note, image, and requirement records retain that original worker association.
//! A recorded result proves assembly, not local delivery or client observation.
//!
//! The queue's drain thread performs recording I/O: journal lines, output-file
//! appends and closes, images, publication, and projections. Handlers, worker
//! output, and replies queue operations for it and return. Three waits remain,
//! each bounded by `queue::WRITER_WAIT`: creating an output file, pacing a
//! producer of file data, and a reply whose omission marker names retained
//! files. A filesystem call that does not return therefore stalls recording,
//! not ordinary use.
//!
//! The integrator's `recording` request and finalization write under the same
//! state lock and wait for it without a bound. Finalization follows producer
//! retirement and transport settlement: fail-open recording handles reported
//! errors, not an indefinitely blocked OS write. Projection failure can leave
//! the journal and raw files usable; failed capture must not gain a completion
//! event.
use std::collections::BTreeMap;
use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{BufWriter, Write};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use chrono::{DateTime, SecondsFormat, Utc};
use event::{Envelope, Event, Outcome, RecordedContent, RecordedResult};
use rmcp::{
    ErrorData,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ImageContent,
        RequestId, RequestMetaObject,
    },
};
use serde::Serialize;
use serde_json::Value;

mod event;
mod files;
mod queue;
use files::{CaptureFile, CaptureStorage};
pub(crate) use queue::WriterState;
use queue::{ProtocolEvents, QueuedEvent};
mod markdown;
mod output;
pub(crate) use output::{CellOutput, OutputRecord};

/// Retained process diagnostics have no worker interval or raw text file.
pub(crate) struct ProcessOutput {
    pub public_directory: Arc<Mutex<PathBuf>>,
    pub retained_bytes: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct Owner {
    pub session_id: u64,
    pub cell_id: Option<u64>,
    pub interval: Interval,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) omit_from_projections: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Interval {
    Startup,
    Cell,
    Post,
}
impl Owner {
    fn stem(self) -> String {
        match self.interval {
            Interval::Startup => "startup".into(),
            Interval::Cell => format!("cell-{:03}", self.cell_id.expect("cell interval")),
            Interval::Post => format!("cell-{:03}-post", self.cell_id.expect("post interval")),
        }
    }
    fn path(self, suffix: &str) -> String {
        format!(
            "capture/sessions/{:03}/{}{suffix}",
            self.session_id,
            self.stem()
        )
    }
}

#[derive(Clone, Copy, Serialize)]
#[serde(untagged)]
enum ImageOwner {
    Interval(Owner),
    Call(CallImageOwner),
}

#[derive(Clone, Copy, Serialize)]
struct CallImageOwner {
    session_id: u64,
    call_id: u64,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    omit_from_projections: bool,
}

impl ImageOwner {
    fn omit_from_projections(self) -> bool {
        match self {
            Self::Interval(owner) => owner.omit_from_projections,
            Self::Call(owner) => owner.omit_from_projections,
        }
    }
}

/// Immediate images belong to their call without changing foreground capture.
pub(crate) struct CallImages {
    transcript: Transcript,
    owner: CallImageOwner,
    budget: queue::ImageBudget,
}
/// Byte ranges in the actual assembled text blocks, or one image block.
#[derive(Clone, Serialize)]
pub(crate) struct ResultPart {
    pub block: usize,
    pub start: usize,
    pub end: usize,
    pub owner: Option<Owner>,
}

#[derive(Clone)]
pub(crate) struct Transcript(
    Arc<Mutex<TranscriptState>>,
    Arc<Mutex<ProtocolEvents>>,
    Arc<AtomicU64>,
    Arc<CaptureStorage>,
    Arc<str>,
);
struct TranscriptState {
    started_at: DateTime<Utc>,
    working_directory: Result<PathBuf, String>,
    dynamic_resolution: bool,
    python_preparation: bool,
    r_available: bool,
    startup_requirements: crate::worker_client::Declaration,
    metadata_known: bool,
    storage: Arc<CaptureStorage>,
    active: Option<ActiveTranscript>,
    failure: Option<String>,
}
struct ActiveTranscript {
    storage: Arc<CaptureStorage>,
    published: bool,
    directory: PathBuf,
    public_directory: Arc<Mutex<PathBuf>>,
    writer: BufWriter<File>,
    projections: Option<markdown::Writers>,
    pending_projection_failure: Option<String>,
    run_id: String,
    sequence: u64,
    call_sessions: BTreeMap<u64, CallAssociation>,
    finished: bool,
}
#[derive(Default)]
struct CallAssociation {
    session_id: Option<u64>,
    source_pending: bool,
    result_finished: bool,
}
// One slot per assembled image; None preserves its position when raw capture
// did not retain it. No image payload is copied into the journal.
type ResultImages = Vec<Option<Artifact>>;

#[derive(Clone)]
pub(crate) struct Call {
    id: Option<u64>,
    result_images: Arc<Mutex<Option<ResultImages>>>,
    parts: Arc<Mutex<Vec<ResultPart>>>,
    /// Actual worker addressed by a call that submitted source.
    addressed_session: Option<u64>,
    omit_from_projections: bool,
}

/// Request identity and note ownership observed before any journal I/O.
pub(crate) struct CallReceipt {
    pub(crate) id: u64,
    at: DateTime<Utc>,
    note_target: Option<NoteTarget>,
}

#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum NoteTarget {
    Worker {
        session_id: u64,
    },
    NextWorker {
        session: String,
        after_session_id: u64,
    },
}

enum CallSource {
    Mcp,
    #[cfg(unix)]
    Integrator {
        include_in_projections: bool,
    },
}
#[derive(Clone)]
pub(crate) struct Artifact {
    id: u64,
    pub(crate) path: String,
    mime_type: String,
    pub(crate) owner: Option<Owner>,
    pub(crate) public_directory: Arc<Mutex<PathBuf>>,
}
impl Transcript {
    #[cfg(unix)]
    /// Describe the projections after writing every queued operation, so the
    /// reply covers the request that asked for it.
    pub(crate) fn integrator_recording(&self) -> Result<serde_json::Value, String> {
        self.update(|state| {
            let Some(active) = &state.active else {
                return Ok(Err("recording has not started".to_string()));
            };
            Ok(active
                .projections
                .as_ref()
                .map(|projections| projections.integrator_recording(&self.4))
                .ok_or_else(|| "recording projections are unavailable".to_string()))
        })
        .unwrap_or_else(|| {
            let (state, _) = self.lock();
            let error = state
                .failure
                .as_deref()
                .unwrap_or("transcript lock poisoned");
            Err(format!("recording is unavailable: {error}"))
        })
    }

    pub(crate) fn call_images(&self, call: &Call) -> Option<CallImages> {
        let call_id = call.id?;
        let session_id = self.owner()?.session_id;
        Some(CallImages {
            transcript: self.clone(),
            owner: CallImageOwner {
                session_id,
                call_id,
                omit_from_projections: call.omit_from_projections,
            },
            budget: queue::ImageBudget::default(),
        })
    }

    #[cfg(test)]
    pub(crate) fn new(dynamic_resolution: bool) -> Self {
        Self::configured(
            std::env::current_dir(),
            dynamic_resolution,
            false,
            true,
            crate::worker_client::Declaration::default(),
        )
    }
    pub(crate) fn configured(
        working_directory: std::io::Result<PathBuf>,
        dynamic_resolution: bool,
        python_preparation: bool,
        r_available: bool,
        startup_requirements: crate::worker_client::Declaration,
    ) -> Self {
        let storage = Arc::new(CaptureStorage::default());
        Self(
            Arc::new(Mutex::new(TranscriptState {
                started_at: Utc::now(),
                working_directory: working_directory
                    .map_err(|e| format!("failed to find the current working directory: {e}")),
                dynamic_resolution,
                python_preparation,
                r_available,
                startup_requirements,
                metadata_known: true,
                storage: storage.clone(),
                active: None,
                failure: None,
            })),
            Arc::default(),
            Arc::default(),
            storage,
            Arc::from("default"),
        )
    }
    /// Share recording identity and storage while retaining this worker's owner.
    pub(crate) fn for_session(&self, session: &str) -> Self {
        Self(
            self.0.clone(),
            self.1.clone(),
            self.2.clone(),
            self.3.clone(),
            Arc::from(session),
        )
    }
    pub(crate) fn pending(working_directory: std::io::Result<PathBuf>) -> Self {
        let value = Self::configured(working_directory, false, false, false, Default::default());
        value.0.lock().expect("transcript lock").metadata_known = false;
        value
    }
    pub(crate) fn configure(&self, configured: Self) {
        let (configuration, _) = configured.lock();
        self.queue_event(QueuedEvent::Configure {
            session: self.4.to_string(),
            dynamic_resolution: configuration.dynamic_resolution,
            python_preparation: configuration.python_preparation,
            r_available: configuration.r_available,
            startup_requirements: configuration.startup_requirements.clone(),
        });
    }
    #[cfg(unix)]
    pub(crate) fn activate(&self) {
        self.queue_event(QueuedEvent::Activate);
    }
    pub(crate) fn protocol(&self, direction: &'static str, message: Value) {
        self.queue_event(QueuedEvent::Event(Event::Protocol { direction, message }));
    }

    pub(crate) fn delivery(&self, request_id: &Value, call_id: Option<u64>, delivered: bool) {
        self.queue_event(QueuedEvent::Event(Event::Delivery {
            request_id: request_id.clone(),
            call_id,
            delivered,
        }));
    }

    /// Capture receipt without waiting for the journal writer. Source-bearing
    /// notes follow source association; standalone notes keep this worker target.
    pub(crate) fn receive_call(&self, request: &CallToolRequestParams) -> CallReceipt {
        let id = self
            .2
            .fetch_add(1, Ordering::Relaxed)
            .checked_add(1)
            .expect("recording call ID space exhausted");
        let at = Utc::now();
        let note_target = request.arguments.as_ref().and_then(|arguments| {
            (arguments.get("note").is_some_and(Value::is_string)
                && !["r", "python", "sql"]
                    .iter()
                    .any(|language| arguments.get(*language).is_some_and(Value::is_string)))
            .then(|| self.note_target())
        });
        CallReceipt {
            id,
            at,
            note_target,
        }
    }
    pub(crate) fn begin(
        &self,
        receipt: CallReceipt,
        request_id: &RequestId,
        request_meta: &RequestMetaObject,
        request: &CallToolRequestParams,
    ) -> Call {
        self.begin_call(receipt, request_id, request_meta, request, CallSource::Mcp)
    }

    #[cfg(unix)]
    pub(crate) fn begin_integrator(
        &self,
        request_id: &RequestId,
        arguments: &serde_json::Map<String, Value>,
        include_in_projections: bool,
    ) -> Call {
        let request = CallToolRequestParams::new("send").with_arguments(arguments.clone());
        self.begin_call(
            self.receive_call(&request),
            request_id,
            &RequestMetaObject::default(),
            &request,
            CallSource::Integrator {
                include_in_projections,
            },
        )
    }

    fn begin_call(
        &self,
        receipt: CallReceipt,
        request_id: &RequestId,
        request_meta: &RequestMetaObject,
        request: &CallToolRequestParams,
        source: CallSource,
    ) -> Call {
        let CallReceipt {
            id: call_id,
            at,
            note_target,
        } = receipt;
        let (origin, omit_from_projections) = match source {
            CallSource::Mcp => (None, false),
            #[cfg(unix)]
            CallSource::Integrator {
                include_in_projections,
            } => (Some("integrator"), !include_in_projections),
        };
        let mut request = request.clone();
        if !request_meta.is_empty() {
            request.meta = Some(request_meta.clone());
        }
        // Validation can reject a restart-shaped request before retirement.
        // finish() associates otherwise unclaimed source only if the worker
        // addressed here still owns output; actual replacement changes that owner.
        let has_source = request.arguments.as_ref().is_some_and(|arguments| {
            ["r", "python", "sql"]
                .iter()
                .any(|language| arguments.get(*language).is_some_and(Value::is_string))
        });
        let event = QueuedEvent::ToolCall {
            call_id,
            note_target,
            request_id: request_id.clone(),
            request,
            origin,
            omit_from_projections,
        };
        match self.tool_call(event, call_id, at, has_source, omit_from_projections) {
            Some(addressed_session) => Call {
                id: Some(call_id),
                result_images: Arc::new(Mutex::new(None)),
                parts: Arc::new(Mutex::new(Vec::new())),
                addressed_session,
                omit_from_projections,
            },
            None => Call::unrecorded(),
        }
    }
    /// Queue the assembled result. The journal names retained image files, so
    /// the queued copy carries no image payload.
    pub(crate) fn finish(&self, call: Call, response: &Result<CallToolResponse, ErrorData>) {
        let Some(call_id) = call.id else {
            return;
        };
        let event = (|| {
            let images = call.take_result_images()?;
            let parts = std::mem::take(
                &mut *call
                    .parts
                    .lock()
                    .map_err(|_| "tool result association lock poisoned")?,
            );
            let response = match response {
                Ok(CallToolResponse::Complete(result)) => {
                    let mut recorded = CallToolResult::default();
                    recorded.result_type = result.result_type.clone();
                    recorded.content = result
                        .content
                        .iter()
                        .map(|block| match block {
                            ContentBlock::Image(image) => {
                                let mut recorded =
                                    ImageContent::new(String::new(), image.mime_type.clone());
                                recorded.meta = image.meta.clone();
                                recorded.annotations = image.annotations.clone();
                                ContentBlock::Image(recorded)
                            }
                            other => other.clone(),
                        })
                        .collect();
                    recorded.structured_content = result.structured_content.clone();
                    recorded.is_error = result.is_error;
                    recorded.meta = result.meta.clone();
                    Ok(recorded)
                }
                Ok(_) => {
                    return Err(
                        "console tool unexpectedly returned a non-final MCP response".to_string(),
                    );
                }
                Err(error) => Err(error.clone()),
            };
            Ok(QueuedEvent::ToolResult {
                call_id,
                images,
                parts,
                response,
                associate: call.addressed_session.filter(|session_id| {
                    self.owner()
                        .is_some_and(|owner| owner.session_id == *session_id)
                }),
                omit_from_projections: call.omit_from_projections,
            })
        })();
        match event {
            Ok(event) => self.queue_event(event),
            Err(error) => self.disable(error),
        }
    }
    pub(crate) fn finalize(&self) {
        self.update(|state| {
            if let Some(active) = state.active.as_mut()
                && !active.finished
            {
                active.append(Event::RecordingEnded, Utc::now())?;
            }
            Ok(())
        });
        // Failed capture must not gain a completion event, but its producers
        // have still retired. Complete the permissions on retained files too.
        let (mut state, _) = self.lock();
        let result = state
            .active
            .as_mut()
            .map(ActiveTranscript::finalize)
            .unwrap_or_else(|| self.3.finalize());
        drop(state);
        if let Err(error) = result {
            report_failure(&error);
        }
    }
    fn lock(&self) -> (std::sync::MutexGuard<'_, TranscriptState>, bool) {
        match self.0.lock() {
            Ok(state) => (state, false),
            Err(poisoned) => (poisoned.into_inner(), true),
        }
    }

    fn update<T>(
        &self,
        operation: impl FnOnce(&mut TranscriptState) -> Result<T, String>,
    ) -> Option<T> {
        let (mut state, poisoned) = self.lock();
        if let Some(error) = &state.failure {
            self.stop(error.clone());
            return None;
        }
        if poisoned {
            let error = "transcript lock poisoned".to_string();
            state.disable(error.clone());
            drop(state);
            self.stop(error.clone());
            report_failure(&error);
            return None;
        }
        let result = self
            .flush_protocol(&mut state)
            .and_then(|()| operation(&mut state))
            .and_then(|value| {
                self.flush_protocol(&mut state)?;
                Ok(value)
            });
        match result {
            Ok(value) => {
                let projection_failure = state
                    .active
                    .as_mut()
                    .and_then(|a| a.pending_projection_failure.take());
                drop(state);
                if let Some(error) = projection_failure {
                    report_projection_failure(&error);
                }
                Some(value)
            }
            Err(error) => {
                state.disable(error.clone());
                drop(state);
                self.stop(error.clone());
                report_failure(&error);
                None
            }
        }
    }
}

pub(crate) fn validate_image_data(data: &str) -> Result<(), String> {
    decode_image_data(data).map(drop)
}
pub(crate) fn decode_image_data(data: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|e| format!("worker returned invalid base64 image data: {e}"))
}
impl TranscriptState {
    fn capture(&mut self) -> Result<&mut ActiveTranscript, String> {
        if self.active.is_none() {
            self.active = Some(ActiveTranscript::create(self)?);
        }
        self.active()
    }

    fn active(&mut self) -> Result<&mut ActiveTranscript, String> {
        self.active
            .as_mut()
            .ok_or_else(|| "transcript was not materialized before recording output".to_string())
    }

    fn disable(&mut self, error: String) -> bool {
        if self.failure.is_some() {
            return false;
        }
        self.failure = Some(error);
        true
    }
}

impl ActiveTranscript {
    fn create(configuration: &TranscriptState) -> Result<Self, String> {
        let working_directory = configuration
            .working_directory
            .as_ref()
            .map_err(Clone::clone)?;
        let started_at = configuration.started_at;
        let storage = configuration.storage.clone();
        let working_directory_text = working_directory.to_string_lossy();
        // Keep incidental process-ID widths from shifting bounded output previews.
        let run_id = format!(
            "{}-{:010}",
            started_at.format("%Y%m%dT%H%M%S%.9fZ"),
            std::process::id()
        );
        let directory = storage.directory()?;
        let journal = directory.join("capture/events.jsonl");
        let writer = create_private_file(&journal)
            .map(BufWriter::new)
            .map_err(|error| format!("failed to create {}: {error}", journal.display()))?;
        let projections = create_private_directory(&directory.join("sessions"), false)
            .map_err(|e| e.to_string())
            .and_then(|()| {
                markdown::Writers::new(
                    &directory,
                    working_directory,
                    configuration.dynamic_resolution,
                    configuration.r_available,
                    &configuration.startup_requirements,
                    configuration.metadata_known,
                )
            });
        let (projections, pending_projection_failure) = match projections {
            Ok(projections) => (Some(projections), None),
            Err(error) => (None, Some(error)),
        };

        let mut transcript = Self {
            public_directory: storage.public_directory.clone(),
            storage,
            published: false,
            directory,
            writer,
            projections,
            pending_projection_failure,
            run_id,
            sequence: 0,
            call_sessions: BTreeMap::new(),
            finished: false,
        };
        transcript.append(
            Event::RecordingStarted {
                console_version: env!("MCP_CONSOLE_VERSION"),
                working_directory: &working_directory_text,
                dynamic_resolution: configuration
                    .metadata_known
                    .then_some(configuration.dynamic_resolution),
                python_preparation: configuration
                    .metadata_known
                    .then_some(configuration.python_preparation),
                startup_requirements: configuration
                    .metadata_known
                    .then_some(&configuration.startup_requirements),
            },
            started_at,
        )?;
        Ok(transcript)
    }
}

impl ActiveTranscript {
    fn publish(&mut self, working_directory: &Path) -> Result<(), String> {
        if self.published {
            return Ok(());
        }
        let project_console = working_directory.join(".agents/console");
        let in_project = match std::fs::metadata(&project_console) {
            Ok(metadata) => metadata.is_dir(),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                false
            }
            Err(e) => return Err(format!("cannot inspect {}: {e}", project_console.display())),
        };
        let console = if in_project {
            project_console
        } else {
            crate::console_paths::home_console_directory()?.ok_or("HOME is not set")?
        };
        let sessions = console.join("sessions");
        let directory = sessions.join(&self.run_id);
        let public = if in_project {
            PathBuf::from(".agents/console/sessions").join(&self.run_id)
        } else {
            directory.clone()
        };
        if public.to_str().is_none() {
            return Err("recording path must be UTF-8".into());
        }
        create_private_directory(&sessions, true)
            .map_err(|e| format!("failed to create {}: {e}", sessions.display()))?;
        create_private_directory(&directory, false)
            .map_err(|e| format!("failed to create {}: {e}", directory.display()))?;
        self.writer.flush().map_err(|e| e.to_string())?;
        self.storage.relocate(&directory)?;
        self.writer = BufWriter::new(
            OpenOptions::new()
                .append(true)
                .open(directory.join("capture/events.jsonl"))
                .map_err(|e| e.to_string())?,
        );
        if let Some(projections) = &mut self.projections
            && let Err(error) = projections.relocate(&directory)
        {
            self.projections = None;
            self.pending_projection_failure = Some(error);
        }
        self.directory = directory;
        *self
            .public_directory
            .lock()
            .map_err(|_| "recording path lock poisoned")? = public;
        self.published = true;
        self.storage.finish_publication()?;
        self.append(Event::RecordingActivated, Utc::now())
    }
    fn append(&mut self, event: Event<'_>, at: DateTime<Utc>) -> Result<(), String> {
        if self.finished {
            return Ok(());
        }
        match &event {
            Event::ToolCall { call_id, .. } => {
                self.call_sessions
                    .insert(*call_id, CallAssociation::default());
            }
            Event::SourceAdmitted { call_id } => {
                self.call_sessions
                    .get_mut(call_id)
                    .ok_or("source admission has no recorded call")?
                    .source_pending = true;
            }
            Event::SourceFinished { call_id } => {
                let call = self
                    .call_sessions
                    .get_mut(call_id)
                    .ok_or("source settlement has no recorded call")?;
                call.source_pending = false;
                if call.result_finished {
                    self.call_sessions.remove(call_id);
                }
            }
            Event::ToolResult { call_id, .. } => {
                let call = self
                    .call_sessions
                    .get_mut(call_id)
                    .ok_or("result settlement has no recorded call")?;
                call.result_finished = true;
                if !call.source_pending {
                    self.call_sessions.remove(call_id);
                }
            }
            Event::SourceAssociated {
                call_id,
                session_id,
            } => {
                let call = self
                    .call_sessions
                    .get_mut(call_id)
                    .ok_or("source association has no recorded call")?;
                if call.session_id == Some(*session_id) {
                    return Ok(());
                }
                call.session_id = Some(*session_id);
            }
            Event::RecordingStarted { .. }
            | Event::RecordingActivated
            | Event::RecordingEnded
            | Event::Protocol { .. }
            | Event::Delivery { .. }
            | Event::WorkerStarted { .. }
            | Event::WorkerReady { .. }
            | Event::WorkerEnded { .. }
            | Event::CellStarted { .. }
            | Event::CellEnded { .. }
            | Event::OutputText { .. }
            | Event::ImageOmitted { .. }
            | Event::ProcessOutput { .. }
            | Event::EnvironmentDiscovered { .. }
            | Event::StartupFailed { .. }
            | Event::PythonEnvironmentAccepted { .. }
            | Event::RequirementsAccepted { .. }
            | Event::RequirementsSelected { .. }
            | Event::ImageRetained { .. }
            | Event::CellOutput { .. }
            | Event::SessionOutput { .. } => {}
        }
        self.sequence += 1;
        let envelope = Envelope {
            event,
            run_id: &self.run_id,
            sequence: self.sequence,
            at: timestamp(at),
        };
        let mut bytes = serde_json::to_vec(&envelope)
            .map_err(|e| format!("failed to serialize transcript event: {e}"))?;
        bytes.push(b'\n');
        self.writer
            .write_all(&bytes)
            .and_then(|()| self.writer.flush())
            .map_err(|e| format!("failed to append transcript event: {e}"))?;
        if let Some(projections) = &mut self.projections
            && let Err(error) = projections.append(&envelope)
        {
            self.projections = None;
            self.pending_projection_failure = Some(error);
        }
        Ok(())
    }
    fn finish(
        &mut self,
        call_id: u64,
        result_images: ResultImages,
        parts: &[ResultPart],
        response: &Result<CallToolResult, ErrorData>,
        omit_from_projections: bool,
        at: DateTime<Utc>,
    ) -> Result<(), String> {
        let outcome = match response {
            Ok(result) => Outcome::Result {
                result: Self::project_result(call_id, result_images, result)?,
            },
            Err(error) => Outcome::Error { error },
        };
        self.append(
            Event::ToolResult {
                call_id,
                parts,
                outcome,
                omit_from_projections,
            },
            at,
        )
    }
    fn finalize(&mut self) -> Result<(), String> {
        if self.finished {
            return Ok(());
        }
        let mut errors = Vec::new();
        self.finished = true;
        // Retirement, output producers, and queued journal writes have settled.
        // QMD associations can settle after worker termination. Finalize their
        // permissions with the recording, after all related updates finish.
        self.projections.take();
        if let Err(error) = self.storage.finalize() {
            errors.push(error);
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
    fn project_result(
        call_id: u64,
        result_images: ResultImages,
        result: &CallToolResult,
    ) -> Result<RecordedResult<'_>, String> {
        let mut result_images = result_images.into_iter();
        let mut content = Vec::with_capacity(result.content.len());
        for item in &result.content {
            content.push(match item {
                ContentBlock::Text(text) => RecordedContent::Text(text),
                ContentBlock::Image(image) => {
                    let artifact = result_images.next().ok_or_else(|| {
                        format!("tool call {call_id} returned an image without a capture association")
                    })?;
                    if let Some(artifact) = &artifact
                        && image.mime_type != artifact.mime_type
                    {
                        return Err(format!(
                            "tool call {call_id} returned an image with a different MIME type than its artifact"
                        ));
                    }
                    RecordedContent::Image {
                        mime_type: &image.mime_type,
                        meta: image.meta.as_ref(),
                        annotations: image.annotations.as_ref(),
                        artifact_id: artifact.as_ref().map(|a| a.id),
                        retained: artifact.is_none().then_some(false),
                        path: artifact.map(|a| a.path),
                    }
                }
                other => RecordedContent::Other(other),
            });
        }
        if result_images.next().is_some() {
            return Err(format!(
                "tool call {call_id} retained more artifacts than it returned"
            ));
        }
        Ok(RecordedResult {
            content,
            structured_content: result.structured_content.as_ref(),
            is_error: result.is_error,
            meta: result.meta.as_ref(),
        })
    }
}

impl Call {
    pub(crate) fn unrecorded() -> Self {
        Self {
            id: None,
            result_images: Arc::new(Mutex::new(None)),
            parts: Arc::new(Mutex::new(Vec::new())),
            addressed_session: None,
            omit_from_projections: false,
        }
    }

    pub(crate) fn id(&self) -> Option<u64> {
        self.id
    }

    pub(crate) fn record_result_images(&self, images: ResultImages) -> Result<(), String> {
        let mut result_images = self
            .result_images
            .lock()
            .map_err(|_| "tool call artifact lock poisoned".to_string())?;
        if result_images.is_some() {
            return Err(format!(
                "tool call {} already retained result images",
                self.id.unwrap_or_default()
            ));
        }
        *result_images = Some(images);
        Ok(())
    }

    fn take_result_images(&self) -> Result<ResultImages, String> {
        Ok(self
            .result_images
            .lock()
            .map_err(|_| "tool call artifact lock poisoned".to_string())?
            .take()
            .unwrap_or_default())
    }
}

impl Call {
    pub(crate) fn record_result_parts(&self, parts: Vec<ResultPart>) {
        *self.parts.lock().expect("tool result association lock") = parts;
    }
}
fn timestamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn report_failure(error: &str) {
    let _ = writeln!(
        std::io::stderr().lock(),
        "mcp-console: transcript recording disabled: {error}"
    );
}

fn report_projection_failure(error: &str) {
    let _ = writeln!(
        std::io::stderr().lock(),
        "mcp-console: transcript projections disabled: {error}"
    );
}

fn create_private_directory(path: &Path, recursive: bool) -> std::io::Result<()> {
    let mut builder = DirBuilder::new();
    builder.recursive(recursive);
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(path)
}

fn create_private_file(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    options.open(path)
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = create_private_file(path)
        .map_err(|error| format!("failed to create {}: {error}", path.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.flush())
        .map_err(|error| format!("failed to write {}: {error}", path.display()))
}

fn replace_private_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let temporary = path.with_file_name(".transcript.qmd.tmp");
    if let Err(error) = write_new(&temporary, bytes) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!(
            "failed to replace {} from {}: {error}",
            path.display(),
            temporary.display()
        ));
    }
    Ok(())
}
