//! The bounded drain owns journal I/O. Observed identities never need its writer
//! mutex, including during worker retirement and response assembly.
//! Reserve worker/cell/image ownership at observation time, before journal I/O;
//! late delivery and replacement must not reassign it. Queue limits include
//! in-flight batches. Overflow disables capture instead of blocking producers,
//! except for the two paced producers described below.
//!
//! Tool calls, results, activation, and configuration are queued operations
//! too: handlers enqueue them and return without waiting for the writer.
//!
//! Request intake is paced: before a transport reads its next request, it
//! waits while operations queued by earlier requests exceed a small bound.
//! Handlers never wait, so without this pacing, a burst of requests fills the
//! queue behind a writer that is slow but still writing. The bound counts only
//! operations without file data and leaves room for operations that requests
//! already read have yet to queue.
//!
//! Output-file and image bytes travel in queued operations as well, so the
//! drain performs every recording write. Their producers are paced too: a
//! producer waits while the writer makes progress, and a writer that applies
//! nothing for `WRITER_WAIT` disables capture. A reply that cites a retained
//! file waits at most `WRITER_WAIT` for the writer and says so if it is behind.
use super::{
    CallImageOwner, Event, ImageOwner, Interval, Owner, ProcessOutput, Transcript, output,
};
use chrono::{DateTime, Utc};
use rmcp::{
    ErrorData,
    model::{CallToolRequestParams, CallToolResult, RequestId},
};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Write;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// What a reply that cites retained files learned from its wait for the writer.
#[derive(Clone, Copy, Default, PartialEq)]
pub(crate) enum WriterState {
    /// Everything queued before the reply was written.
    #[default]
    Settled,
    /// The bounded wait ran out with queued operations still unwritten.
    Behind,
    /// Recording stopped; operations still queued then were discarded.
    Stopped,
}

#[derive(Default)]
pub(super) struct ProtocolEvents {
    pending: Vec<(QueuedEvent, DateTime<Utc>, usize)>,
    // Include batches held by flush_protocol while journal I/O is blocked.
    retained_events: usize,
    retained_bytes: usize,
    /// The share of that retention in operations that carry file data.
    retained_data_events: usize,
    retained_data_bytes: usize,
    pub(super) failure: Option<String>,
    draining: bool,
    next_session_id: u64,
    workers: BTreeMap<Arc<str>, WorkerEvents>,
    next_artifact_id: u64,
    process_output_bytes: u64,
    /// Only calls still awaiting cell entry; interval owners carry the choice later.
    pub(super) omitted_calls: std::collections::BTreeSet<u64>,
    /// Operations accepted and operations applied or dropped. Merged output
    /// ranges count once.
    queued: u64,
    applied: u64,
    /// The backlog point a reply last gave up waiting for.
    unsettled: Option<u64>,
    /// Notified whenever `applied` advances or capture stops.
    wake: Arc<Condvar>,
    /// Notified whenever the writer changes what a rendering says about
    /// retained files: a stop, a stall found or cleared, a truncated file.
    changed: tokio::sync::watch::Sender<()>,
    /// Discarded operations and released output files. Dropping one can close
    /// a file, so only a writer pass drops them, and never under this lock.
    retired: Vec<Box<dyn Send>>,
}

#[derive(Default)]
struct WorkerEvents {
    owner: Option<Owner>,
    cell_call: Option<u64>,
    starting_session: Option<u64>,
    images: ImageBudget,
}

#[derive(Serialize)]
pub(super) enum QueuedEvent {
    Event(Event<'static>),
    /// Publish the recording directory on first use.
    #[cfg(unix)]
    Activate,
    /// Publishes like Activate, then records the call.
    ToolCall {
        call_id: u64,
        note_target: Option<super::NoteTarget>,
        request_id: RequestId,
        request: CallToolRequestParams,
        origin: Option<&'static str>,
        omit_from_projections: bool,
    },
    ToolResult {
        call_id: u64,
        #[serde(skip)]
        images: super::ResultImages,
        parts: Vec<super::ResultPart>,
        response: Result<CallToolResult, ErrorData>,
        /// The addressed worker, if it still owned output when the call finished.
        associate: Option<u64>,
        omit_from_projections: bool,
    },
    Configure {
        session: String,
        dynamic_resolution: bool,
        python_preparation: bool,
        r_available: bool,
        startup_requirements: crate::worker_client::Declaration,
    },
    WorkerStarting,
    AssociateWaitingCall {
        call_id: u64,
        session_id: u64,
    },
    WorkerStarted {
        session: String,
        session_id: u64,
        pid: u32,
        requirements: Value,
    },
    WorkerReady {
        session_id: u64,
    },
    WorkerEnded {
        session_id: u64,
        outcome: String,
    },
    CellStarted {
        call_id: u64,
        owner: Owner,
    },
    CellEnded {
        call_id: u64,
        owner: Owner,
        completion_confirmed: bool,
    },
    /// Append to an output file, then journal the retained range.
    OutputWrite {
        owner: Owner,
        path: String,
        offset: u64,
        #[serde(skip)]
        data: Vec<u8>,
        discarded_bytes: usize,
        #[serde(skip)]
        file: Arc<output::OutputFile>,
    },
    OutputClose {
        #[serde(skip)]
        file: Arc<output::OutputFile>,
    },
    ImageOmitted {
        owner: Option<ImageOwner>,
        encoded_bytes: usize,
        mime_type: String,
    },
    ImageRetained {
        owner: ImageOwner,
        artifact_id: u64,
        call_id: Option<u64>,
        path: String,
        mime_type: String,
        bytes: usize,
        #[serde(skip)]
        data: Vec<u8>,
    },
    OutputSummary {
        call_id: Option<u64>,
        owner: Owner,
        path: String,
        summary: output::CellOutputSummary,
        #[serde(skip)]
        file: Arc<output::OutputFile>,
    },
    ProcessOutput {
        data: String,
        discarded_bytes: usize,
    },
    StartupFailed {
        session_id: Option<u64>,
        message: String,
    },
    PythonEnvironmentAccepted {
        packages: Vec<String>,
    },
    RequirementsAccepted {
        session: String,
        requirements: Value,
    },
    RequirementsSelected {
        call_id: Option<u64>,
        action: String,
        snapshot: Value,
    },
}

impl QueuedEvent {
    /// The file data this operation carries, if any.
    fn data(&self) -> Option<&Vec<u8>> {
        match self {
            Self::OutputWrite { data, .. } | Self::ImageRetained { data, .. } => Some(data),
            _ => None,
        }
    }

    fn append(
        self,
        state: &mut super::TranscriptState,
        recorder: &Transcript,
        at: DateTime<Utc>,
    ) -> Result<(), String> {
        #[cfg(unix)]
        let publishes = matches!(self, Self::Activate | Self::ToolCall { .. });
        #[cfg(not(unix))]
        let publishes = matches!(self, Self::ToolCall { .. });
        if publishes {
            let working_directory = state.working_directory.clone()?;
            let capture = state.capture()?;
            let published = capture.published;
            capture.publish(&working_directory)?;
            if !published {
                // Receipts issued before publication can now name their files.
                recorder.writer_changed();
            }
        }
        let active = state.capture()?;
        let event = match &self {
            Self::Event(_) => {
                let Self::Event(event) = self else {
                    unreachable!()
                };
                return active.append(event, at);
            }
            #[cfg(unix)]
            Self::Activate => return Ok(()),
            Self::ToolCall {
                call_id,
                note_target,
                request_id,
                request,
                origin,
                omit_from_projections,
            } => Event::ToolCall {
                call_id: *call_id,
                note_target: note_target.clone(),
                request_id,
                request,
                origin: *origin,
                omit_from_projections: *omit_from_projections,
            },
            Self::ToolResult {
                call_id,
                images,
                parts,
                response,
                associate,
                omit_from_projections,
            } => {
                // Validation can reject a restart-shaped request before
                // retirement. Associate otherwise unclaimed source only with
                // the worker it addressed; actual replacement changes that owner.
                if let Some(session_id) = associate
                    && active
                        .call_sessions
                        .get(call_id)
                        .and_then(|call| call.session_id)
                        .is_none()
                {
                    active.append(
                        Event::SourceAssociated {
                            call_id: *call_id,
                            session_id: *session_id,
                        },
                        at,
                    )?;
                }
                active.finish(
                    *call_id,
                    images.clone(),
                    parts,
                    response,
                    *omit_from_projections,
                    at,
                )?;
                if !active.call_sessions.contains_key(call_id) {
                    recorder
                        .1
                        .lock()
                        .expect("protocol capture lock")
                        .omitted_calls
                        .remove(call_id);
                }
                return Ok(());
            }
            Self::Configure {
                session,
                dynamic_resolution,
                python_preparation,
                r_available,
                startup_requirements,
            } => {
                state.dynamic_resolution = *dynamic_resolution;
                state.python_preparation = *python_preparation;
                state.r_available = *r_available;
                state.startup_requirements = startup_requirements.clone();
                state.metadata_known = true;
                return state.capture()?.append(
                    Event::EnvironmentDiscovered {
                        session,
                        dynamic_resolution: *dynamic_resolution,
                        python_preparation: *python_preparation,
                        r_available: *r_available,
                        startup_requirements,
                    },
                    at,
                );
            }
            Self::WorkerStarting => return Ok(()),
            Self::AssociateWaitingCall {
                call_id,
                session_id,
            } => {
                // Replacement readiness can arrive after a failed source and
                // its result have both settled. Never associate that retired
                // call with the replacement worker.
                let Some(call) = active.call_sessions.get(call_id) else {
                    return Ok(());
                };
                if call.session_id.is_some() {
                    return Ok(());
                }
                Event::SourceAssociated {
                    call_id: *call_id,
                    session_id: *session_id,
                }
            }
            Self::WorkerStarted {
                session,
                session_id,
                pid,
                requirements,
            } => Event::WorkerStarted {
                session,
                session_id: *session_id,
                pid: *pid,
                requirements,
            },
            Self::WorkerReady { session_id } => Event::WorkerReady {
                session_id: *session_id,
            },
            Self::WorkerEnded {
                session_id,
                outcome,
            } => Event::WorkerEnded {
                session_id: *session_id,
                outcome,
            },
            Self::CellStarted { call_id, owner } => Event::CellStarted {
                call_id: *call_id,
                owner: *owner,
            },
            Self::CellEnded {
                call_id,
                owner,
                completion_confirmed,
            } => Event::CellEnded {
                call_id: *call_id,
                owner: *owner,
                completion_confirmed: *completion_confirmed,
            },
            Self::OutputWrite {
                owner,
                path,
                offset,
                data,
                discarded_bytes,
                file,
            } => {
                // A failed file journals its truncated range once; its
                // summary accounts for every later range.
                let Some(retained) = file.write(*offset, data) else {
                    return Ok(());
                };
                if retained < data.len() {
                    recorder.writer_changed();
                }
                Event::OutputText {
                    owner: *owner,
                    path,
                    offset: *offset,
                    bytes: retained,
                    discarded_bytes: data.len() - retained + *discarded_bytes,
                }
            }
            Self::OutputClose { file } => {
                file.close();
                return Ok(());
            }
            Self::ImageOmitted {
                owner,
                encoded_bytes,
                mime_type,
            } => Event::ImageOmitted {
                owner: *owner,
                encoded_bytes: *encoded_bytes,
                mime_type,
            },
            Self::ImageRetained {
                owner,
                artifact_id,
                call_id,
                path,
                mime_type,
                bytes,
                data,
            } => {
                recorder.3.write_image(path, data)?;
                Event::ImageRetained {
                    owner: *owner,
                    artifact_id: *artifact_id,
                    call_id: *call_id,
                    path,
                    mime_type,
                    bytes: *bytes,
                }
            }
            Self::OutputSummary {
                call_id,
                owner,
                path,
                summary,
                file,
            } => {
                // The producer counted queued bytes. Every write queued
                // before this summary has run, so correct them to the file.
                let retained_bytes = file.written().min(summary.retained_bytes);
                let discarded_bytes =
                    summary.discarded_bytes + (summary.retained_bytes - retained_bytes);
                match call_id {
                    Some(call_id) => Event::CellOutput {
                        owner: *owner,
                        call_id: *call_id,
                        path,
                        retained_bytes,
                        inline_omitted_bytes: summary.inline_omitted_bytes,
                        discarded_bytes,
                        retention_limit_bytes: output::MAX_CELL_OUTPUT_BYTES,
                    },
                    None => Event::SessionOutput {
                        owner: *owner,
                        path,
                        retained_bytes,
                        inline_omitted_bytes: summary.inline_omitted_bytes,
                        discarded_bytes,
                        retention_limit_bytes: output::MAX_CELL_OUTPUT_BYTES,
                    },
                }
            }
            Self::ProcessOutput {
                data,
                discarded_bytes,
            } => Event::ProcessOutput {
                data,
                discarded_bytes: *discarded_bytes,
            },
            Self::StartupFailed {
                session_id,
                message,
            } => Event::StartupFailed {
                session_id: *session_id,
                message,
            },
            Self::PythonEnvironmentAccepted { packages } => {
                Event::PythonEnvironmentAccepted { packages }
            }
            Self::RequirementsAccepted {
                session,
                requirements,
            } => Event::RequirementsAccepted {
                session,
                requirements,
            },
            Self::RequirementsSelected {
                call_id,
                action,
                snapshot,
            } => Event::RequirementsSelected {
                call_id: *call_id,
                action,
                snapshot,
            },
        };
        active.append(event, at)
    }
}

const MAX_PROTOCOL_EVENTS: usize = 1024;
/// The overflow bound on queued operations that carry no file data.
const MAX_PROTOCOL_BYTES: usize = 64 * 1024 * 1024;
/// Producers of file data wait for the writer beyond this backlog.
const PACED_BYTES: usize = 16 * 1024 * 1024;
const PACED_EVENTS: usize = MAX_PROTOCOL_EVENTS / 2;
/// Request intake waits for the writer beyond this many queued operations
/// that carry no file data, or `PACED_BYTES` of them. File data can hold half
/// of the queue, and a transport can read this many requests before any of
/// them is handled; the other half then leaves each of them eight operations
/// of its own. Pacing keeps request traffic alone from reaching the overflow
/// bounds, which remain the limit for whatever is not paced.
const PACED_REQUEST_EVENTS: usize = MAX_PROTOCOL_EVENTS / 16;
/// One merged output range. Smaller ranges keep writer progress observable,
/// and a range's buffer doubles as it grows: this bound keeps that buffer
/// under 128 KiB however far the writer falls behind.
const MAX_MERGED_BYTES: usize = 64 * 1024;
/// The largest write merged into a pending range. A larger write is already
/// one buffer; merging it would copy it again on the thread that produced it.
const MAX_MERGED_WRITE_BYTES: usize = 4 * 1024;
/// The longest a reply or a producer waits for the recording writer, and the
/// longest an output-file creation may take.
pub(super) const WRITER_WAIT: Duration = Duration::from_secs(5);
// Raw image files are bounded per interval or immediate call, independently of previews.
const MAX_INTERVAL_IMAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_INTERVAL_IMAGES: u64 = 4096;
const MAX_INTERVAL_IMAGE_METADATA_BYTES: usize = 64 * 1024;

#[derive(Default)]
pub(super) struct ImageBudget {
    number: u64,
    bytes: usize,
    metadata_bytes: usize,
}

impl ImageBudget {
    fn reserve(&mut self, bytes: usize, mime_type: &str) -> Option<u64> {
        if self.number == MAX_INTERVAL_IMAGES
            || bytes > MAX_INTERVAL_IMAGE_BYTES - self.bytes
            || mime_type.len() > MAX_INTERVAL_IMAGE_METADATA_BYTES - self.metadata_bytes
        {
            return None;
        }
        self.number += 1;
        self.bytes += bytes;
        self.metadata_bytes += mime_type.len();
        Some(self.number)
    }
}

impl ProtocolEvents {
    fn push(&mut self, event: QueuedEvent, at: DateTime<Utc>) -> bool {
        // The bounds stop a backlog from growing; they do not cap one event,
        // whose producer already holds it. Retention can exceed the byte bound
        // by the one event that crosses it. File data does not count toward
        // the byte bound: its producers wait at `PACED_BYTES`, and one image
        // they then queue can be as large as this bound. Counting it would
        // refuse the events that complete that image's cell.
        if self.retained_events == MAX_PROTOCOL_EVENTS
            || self.retained_bytes - self.retained_data_bytes >= MAX_PROTOCOL_BYTES
        {
            self.stop(format!("protocol capture backlog exceeded ({MAX_PROTOCOL_EVENTS} events or {MAX_PROTOCOL_BYTES} bytes)"));
            return false;
        }
        // Consecutive raw ranges have one observed order and no intervening
        // image or boundary. Merge only pending ranges, retaining their first
        // timestamp; tiny writes must not become a queue of per-write objects.
        if let Some((
            QueuedEvent::OutputWrite {
                owner,
                path,
                offset,
                data,
                discarded_bytes,
                ..
            },
            _,
            measured,
        )) = self.pending.last_mut()
            && let QueuedEvent::OutputWrite {
                owner: next_owner,
                path: next_path,
                offset: next_offset,
                data: next_data,
                discarded_bytes: next_discarded,
                ..
            } = &event
            && owner == next_owner
            && path == next_path
            && *offset + data.len() as u64 == *next_offset
            && next_data.len() <= MAX_MERGED_WRITE_BYTES
            && data.len() + next_data.len() <= MAX_MERGED_BYTES
        {
            data.extend_from_slice(next_data);
            *discarded_bytes += next_discarded;
            *measured += next_data.len();
            self.retained_bytes += next_data.len();
            self.retained_data_bytes += next_data.len();
            return true;
        }
        let mut size = ProtocolBytes::default();
        if let Err(error) = serde_json::to_writer(&mut size, &event) {
            self.stop(format!("protocol capture failed: {error}"));
            return false;
        }
        // File data is not serialized, but the queue retains it.
        let size = size.0 + event.data().map_or(0, Vec::len);
        if event.data().is_some() {
            self.retained_data_events += 1;
            self.retained_data_bytes += size;
        }
        self.queued += 1;
        self.retained_events += 1;
        self.retained_bytes += size;
        self.pending.push((event, at, size));
        true
    }

    /// Whether a producer of file data must wait for the writer.
    fn data_paced(&self) -> bool {
        self.failure.is_none()
            && (self.retained_bytes >= PACED_BYTES || self.retained_events >= PACED_EVENTS)
    }

    /// Whether request intake must wait for the writer. File data does not
    /// count: a worker that floods output must not hold back the request
    /// that interrupts it.
    fn intake_paced(&self) -> bool {
        self.failure.is_none()
            && (self.retained_events - self.retained_data_events >= PACED_REQUEST_EVENTS
                || self.retained_bytes - self.retained_data_bytes >= PACED_BYTES)
    }

    pub(super) fn stop(&mut self, error: String) {
        self.failure.get_or_insert(error);
        self.changed.send_replace(());
        self.omitted_calls.clear();
        self.applied += self.pending.len() as u64;
        self.wake.notify_all();
        self.retained_events -= self.pending.len();
        self.retained_bytes -= self
            .pending
            .iter()
            .map(|(_, _, bytes)| bytes)
            .sum::<usize>();
        if !self.pending.is_empty() {
            self.retired
                .push(Box::new(std::mem::take(&mut self.pending)));
        }
    }
}

/// Wait while the backlog is `paced` and the writer makes progress. A writer
/// that applies nothing for `WRITER_WAIT` stops capture instead.
fn pace(
    mut events: std::sync::MutexGuard<'_, ProtocolEvents>,
    paced: fn(&ProtocolEvents) -> bool,
) -> std::sync::MutexGuard<'_, ProtocolEvents> {
    while paced(&events) {
        let applied = events.applied;
        let wake = events.wake.clone();
        let (guard, wait) = wake
            .wait_timeout(events, WRITER_WAIT)
            .expect("protocol capture lock");
        events = guard;
        if wait.timed_out() && events.applied == applied {
            events.stop(format!(
                "recording writer made no progress for {} seconds",
                WRITER_WAIT.as_secs()
            ));
        }
    }
    events
}

#[derive(Default)]
// Measure serialized retention without allocating another copy of the event.
struct ProtocolBytes(usize);
impl Write for ProtocolBytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Transcript {
    pub(super) fn queue_event(&self, event: QueuedEvent) {
        self.observe(|_| Some((event, ())));
    }

    /// Queue an operation that carries file data, pacing its producer to the
    /// writer. The byte bound alone would otherwise turn a fast producer into
    /// a capture failure; the synchronous write this replaces paced it too.
    pub(super) fn queue_data(&self, event: QueuedEvent) {
        let events = self.1.lock().expect("protocol capture lock");
        if events.failure.is_some() {
            // The pass that stopped capture reports the failure. Output after that
            // must not start a writer thread for each write.
            return;
        }
        let mut events = pace(events, ProtocolEvents::data_paced);
        if events.failure.is_none() {
            events.push(event, Utc::now());
        }
        self.start_drain(events);
    }

    /// Hold the next request back while the writer works through the
    /// operations that earlier requests queued. Requests queue them without
    /// waiting, so a burst would otherwise fill the queue and disable capture
    /// behind a writer that is slow but still writing. Call this before
    /// reading a request.
    pub(crate) async fn paced(&self) {
        if !self.1.lock().expect("protocol capture lock").intake_paced() {
            return;
        }
        let recording = self.clone();
        tokio::task::spawn_blocking(move || {
            recording.drain_retired(pace(
                recording.1.lock().expect("protocol capture lock"),
                ProtocolEvents::intake_paced,
            ));
        })
        .await
        .expect("recording pacing task");
    }

    pub(super) fn stopped(&self) -> bool {
        self.1
            .lock()
            .expect("protocol capture lock")
            .failure
            .is_some()
    }

    /// Wait up to `WRITER_WAIT` for the writer to apply everything queued so
    /// far, and report where it stands. While an earlier wait's backlog
    /// remains unapplied, report that at once: one stall delays one reply, not
    /// every reply that cites a file.
    pub(crate) fn settle(&self) -> WriterState {
        let deadline = Instant::now() + WRITER_WAIT;
        let mut events = self.1.lock().expect("protocol capture lock");
        let target = events.queued;
        if let Some(point) = events.unsettled {
            if events.failure.is_none() && events.applied < point {
                return WriterState::Behind;
            }
            events.unsettled = None;
        }
        let wake = events.wake.clone();
        loop {
            // A stop discards queued writes, so check it before the count.
            if events.failure.is_some() {
                return WriterState::Stopped;
            }
            if events.applied >= target {
                return WriterState::Settled;
            }
            let now = Instant::now();
            if now >= deadline {
                events.unsettled = Some(target);
                events.changed.send_replace(());
                return WriterState::Behind;
            }
            events = wake
                .wait_timeout(events, deadline - now)
                .expect("protocol capture lock")
                .0;
        }
    }

    /// Where the writer stands, without waiting. A preview repeats while
    /// output is pending, so it never waits for the writer: it reports a
    /// stall only after a reply's bounded wait in `settle` has found one.
    #[cfg(unix)]
    pub(crate) fn writer_state(&self) -> WriterState {
        let events = self.1.lock().expect("protocol capture lock");
        if events.failure.is_some() {
            WriterState::Stopped
        } else if events.unsettled.is_some_and(|point| events.applied < point) {
            WriterState::Behind
        } else {
            WriterState::Settled
        }
    }

    /// Wake when `writer_state` or a file's retention may have changed, so a
    /// preview subscription can replace a snapshot the worker did not change.
    #[cfg(unix)]
    pub(crate) fn subscribe_writer(&self) -> tokio::sync::watch::Receiver<()> {
        self.1
            .lock()
            .expect("protocol capture lock")
            .changed
            .subscribe()
    }

    fn writer_changed(&self) {
        self.1
            .lock()
            .expect("protocol capture lock")
            .changed
            .send_replace(());
    }

    fn observe<T>(
        &self,
        operation: impl FnOnce(&mut ProtocolEvents) -> Option<(QueuedEvent, T)>,
    ) -> Option<T> {
        self.observe_at(None, operation)
    }

    /// Queue one operation, timestamped now unless it was observed earlier.
    fn observe_at<T>(
        &self,
        at: Option<DateTime<Utc>>,
        operation: impl FnOnce(&mut ProtocolEvents) -> Option<(QueuedEvent, T)>,
    ) -> Option<T> {
        let mut events = self.1.lock().expect("protocol capture lock");
        if events.failure.is_some() {
            return None;
        }
        let (event, value) = operation(&mut events)?;
        let accepted = events.push(event, at.unwrap_or_else(Utc::now));
        self.start_drain(events);
        accepted.then_some(value)
    }

    fn start_drain(&self, mut events: std::sync::MutexGuard<'_, ProtocolEvents>) {
        if events.draining {
            return;
        }
        events.draining = true;
        drop(events);
        let recorder = self.clone();
        std::thread::spawn(move || {
            loop {
                recorder.update(|_| Ok(()));
                let mut events = recorder.1.lock().expect("protocol capture lock");
                let retired = std::mem::take(&mut events.retired);
                if events.pending.is_empty() && retired.is_empty() {
                    events.draining = false;
                    return;
                }
                drop(events);
                // This can close files, and one of those drops can retire
                // its file in turn.
                drop(retired);
            }
        });
    }

    /// Start a writer pass for whatever a stop discarded.
    fn drain_retired(&self, events: std::sync::MutexGuard<'_, ProtocolEvents>) {
        if !events.retired.is_empty() {
            self.start_drain(events);
        }
    }

    /// Stop capture, leaving what that discards to a writer pass.
    pub(super) fn stop(&self, error: String) {
        let mut events = self.1.lock().expect("protocol capture lock");
        events.stop(error);
        self.drain_retired(events);
    }

    /// Leave an open output file for the writer to close.
    pub(super) fn retire(&self, file: Arc<Mutex<super::CaptureFile>>) {
        let mut events = self.1.lock().expect("protocol capture lock");
        events.retired.push(Box::new(file));
        self.start_drain(events);
    }

    pub(crate) fn disable(&self, error: String) {
        let mut events = self.1.lock().expect("protocol capture lock");
        events.stop(error);
        self.start_drain(events);
    }

    pub(super) fn flush_protocol(&self, state: &mut super::TranscriptState) -> Result<(), String> {
        let pending = {
            let mut events = self.1.lock().expect("protocol capture lock");
            if let Some(error) = &events.failure {
                return Err(error.clone());
            }
            std::mem::take(&mut events.pending)
        };
        // Release retention per operation. A failed batch releases the rest
        // of its retention; the caller then disables recording.
        let mut pending = pending.into_iter();
        while let Some((event, at, bytes)) = pending.next() {
            let data = event.data().is_some();
            let result = event.append(state, self, at);
            let (count, bytes) = match result {
                Ok(()) => (1, bytes),
                Err(_) => pending
                    .by_ref()
                    .fold((1, bytes), |(count, total), (_, _, bytes)| {
                        (count + 1, total + bytes)
                    }),
            };
            let mut events = self.1.lock().expect("protocol capture lock");
            events.retained_events -= count;
            events.retained_bytes -= bytes;
            // A failed batch stops capture, which ends pacing.
            if data && result.is_ok() {
                events.retained_data_events -= 1;
                events.retained_data_bytes -= bytes;
            }
            events.applied += count as u64;
            if events
                .unsettled
                .is_some_and(|point| events.applied >= point)
            {
                events.unsettled = None;
                events.changed.send_replace(());
            }
            match &result {
                Ok(()) => events.wake.notify_all(),
                // Publish the stop with the count that covers the discarded
                // operations, so no waiter reads them as written.
                Err(error) => events.stop(error.clone()),
            }
            drop(events);
            result?;
        }
        Ok(())
    }

    /// Queue a call and report the worker it addressed, if it carries source.
    /// Returns None when recording has stopped.
    pub(super) fn tool_call(
        &self,
        event: QueuedEvent,
        call_id: u64,
        at: DateTime<Utc>,
        has_source: bool,
        omit_from_projections: bool,
    ) -> Option<Option<u64>> {
        self.observe_at(Some(at), |events| {
            if omit_from_projections {
                events.omitted_calls.insert(call_id);
            }
            let addressed = events
                .workers
                .get(&self.4)
                .and_then(|worker| worker.owner)
                .filter(|_| has_source)
                .map(|owner| owner.session_id);
            Some((event, addressed))
        })
    }

    pub(crate) fn source_admitted(&self, call_id: Option<u64>) {
        if let Some(call_id) = call_id {
            self.queue_event(QueuedEvent::Event(Event::SourceAdmitted { call_id }));
        }
    }
    /// The evaluation retains this choice through retirement, including a late
    /// cell-entry receipt after source admission has already settled.
    pub(crate) fn take_projection_omission(&self, call_id: Option<u64>) -> bool {
        call_id.is_some_and(|call_id| {
            self.1
                .lock()
                .expect("protocol capture lock")
                .omitted_calls
                .remove(&call_id)
        })
    }
    pub(crate) fn source_finished(&self, call_id: Option<u64>) {
        if let Some(call_id) = call_id {
            self.observe(|events| {
                events.omitted_calls.remove(&call_id);
                Some((QueuedEvent::Event(Event::SourceFinished { call_id }), ()))
            });
        }
    }
    /// Bind waiting source to its first worker without replaying it onto replacement.
    pub(crate) fn associate_waiting_call(&self, call_id: Option<u64>) {
        self.observe(|events| {
            Some((
                QueuedEvent::AssociateWaitingCall {
                    call_id: call_id?,
                    session_id: events.workers.get(&self.4)?.owner?.session_id,
                },
                (),
            ))
        });
    }
    pub(crate) fn associate_current_call(&self, call_id: Option<u64>) {
        self.observe(|events| {
            Some((
                QueuedEvent::Event(Event::SourceAssociated {
                    call_id: call_id?,
                    session_id: events.workers.get(&self.4)?.owner?.session_id,
                }),
                (),
            ))
        });
    }
    pub(crate) fn owner(&self) -> Option<Owner> {
        let events = self.1.lock().expect("protocol capture lock");
        events
            .failure
            .is_none()
            .then_some(events.workers.get(&self.4)?.owner)
            .flatten()
    }
    pub(super) fn note_target(&self) -> super::NoteTarget {
        let events = self.1.lock().expect("protocol capture lock");
        match events.workers.get(&self.4).and_then(|worker| worker.owner) {
            Some(owner) => super::NoteTarget::Worker {
                session_id: owner.session_id,
            },
            // Other logical sessions can acquire workers first. Record a
            // boundary, without reserving a global ID or creating a worker.
            None => super::NoteTarget::NextWorker {
                session: self.4.to_string(),
                after_session_id: events.next_session_id,
            },
        }
    }
    pub(crate) fn worker_starting(&self) {
        self.observe(|events| {
            events
                .workers
                .entry(self.4.clone())
                .or_default()
                .starting_session = None;
            Some((QueuedEvent::WorkerStarting, ()))
        });
    }
    pub(crate) fn worker_started(&self, pid: u32, requirements: &Value) {
        self.observe(|events| {
            events.next_session_id += 1;
            let session_id = events.next_session_id;
            let worker = events.workers.entry(self.4.clone()).or_default();
            worker.owner = Some(Owner {
                session_id,
                cell_id: None,
                interval: Interval::Startup,
                omit_from_projections: false,
            });
            worker.images = ImageBudget::default();
            worker.starting_session = Some(session_id);
            Some((
                QueuedEvent::WorkerStarted {
                    session: self.4.to_string(),
                    session_id,
                    pid,
                    requirements: requirements.clone(),
                },
                (),
            ))
        });
    }
    pub(crate) fn worker_ready(&self) {
        self.observe(|events| {
            Some((
                QueuedEvent::WorkerReady {
                    session_id: events.workers.get_mut(&self.4)?.starting_session.take()?,
                },
                (),
            ))
        });
    }
    pub(crate) fn worker_ended(&self, outcome: &str) {
        self.observe(|events| {
            Some((
                QueuedEvent::WorkerEnded {
                    session_id: events.workers.get_mut(&self.4)?.owner.take()?.session_id,
                    outcome: outcome.to_owned(),
                },
                (),
            ))
        });
    }
    pub(crate) fn worker_retired(&self) {
        let mut events = self.1.lock().expect("protocol capture lock");
        if let Some(worker) = events.workers.get_mut(&self.4) {
            // Startup failure can be reported after retirement and still owns
            // the acquired worker's starting_session identity.
            worker.owner = None;
            worker.cell_call = None;
        }
    }
    pub(super) fn start_cell(
        &self,
        call_id: Option<u64>,
        omit_from_projections: bool,
    ) -> Option<Owner> {
        let call_id = call_id?;
        self.observe(|events| {
            let worker = events.workers.get_mut(&self.4)?;
            let previous = worker.owner?;
            let owner = Owner {
                session_id: previous.session_id,
                cell_id: Some(previous.cell_id.unwrap_or(0) + 1),
                interval: Interval::Cell,
                omit_from_projections,
            };
            worker.cell_call = Some(call_id);
            worker.owner = Some(owner);
            worker.images = ImageBudget::default();
            Some((QueuedEvent::CellStarted { call_id, owner }, owner))
        })
    }
    pub(crate) fn end_cell(&self, completion_confirmed: bool) {
        self.observe(|events| {
            let worker = events.workers.get_mut(&self.4)?;
            let owner = worker.owner.filter(|o| o.interval == Interval::Cell)?;
            let call_id = worker.cell_call.take()?;
            worker.owner = Some(Owner {
                interval: Interval::Post,
                ..owner
            });
            worker.images = ImageBudget::default();
            Some((
                QueuedEvent::CellEnded {
                    call_id,
                    owner,
                    completion_confirmed,
                },
                (),
            ))
        });
    }
    pub(super) fn reserve_image(
        &self,
        bytes: usize,
        encoded_bytes: usize,
        mime_type: &str,
    ) -> Option<(u64, Owner, String)> {
        let mut events = self.1.lock().expect("protocol capture lock");
        if events.failure.is_some() {
            return None;
        }
        let worker = events.workers.get_mut(&self.4)?;
        let owner = worker.owner?;
        let Some(number) = worker.images.reserve(bytes, mime_type) else {
            events.push(
                QueuedEvent::ImageOmitted {
                    owner: Some(ImageOwner::Interval(owner)),
                    encoded_bytes,
                    mime_type: mime_type.to_owned(),
                },
                Utc::now(),
            );
            self.start_drain(events);
            return None;
        };
        events.next_artifact_id += 1;
        Some((
            events.next_artifact_id,
            owner,
            owner.path(&format!("-{number:03}")),
        ))
    }

    pub(super) fn reserve_call_image(
        &self,
        owner: CallImageOwner,
        budget: &mut ImageBudget,
        bytes: usize,
        encoded_bytes: usize,
        mime_type: &str,
    ) -> Option<(u64, String)> {
        let mut events = self.1.lock().expect("protocol capture lock");
        if events.failure.is_some() {
            return None;
        }
        let Some(number) = budget.reserve(bytes, mime_type) else {
            events.push(
                QueuedEvent::ImageOmitted {
                    owner: Some(ImageOwner::Call(owner)),
                    encoded_bytes,
                    mime_type: mime_type.to_owned(),
                },
                Utc::now(),
            );
            self.start_drain(events);
            return None;
        };
        events.next_artifact_id += 1;
        Some((
            events.next_artifact_id,
            format!(
                "capture/sessions/{:03}/call-{:03}-{number:03}",
                owner.session_id, owner.call_id,
            ),
        ))
    }
    pub(crate) fn unowned_text(&self, bytes: &[u8]) -> Option<ProcessOutput> {
        use base64::Engine as _;
        self.observe(|events| {
            let retained = bytes.len().min(
                output::MAX_CELL_OUTPUT_BYTES.saturating_sub(events.process_output_bytes) as usize,
            );
            events.process_output_bytes += retained as u64;
            Some((
                QueuedEvent::ProcessOutput {
                    data: base64::engine::general_purpose::STANDARD.encode(&bytes[..retained]),
                    discarded_bytes: bytes.len() - retained,
                },
                ProcessOutput {
                    public_directory: self.3.public_directory.clone(),
                    retained_bytes: retained,
                },
            ))
        })
    }
    pub(crate) fn startup_failed(&self, message: &str) {
        self.observe(|events| {
            Some((
                QueuedEvent::StartupFailed {
                    session_id: events
                        .workers
                        .get_mut(&self.4)
                        .and_then(|worker| worker.starting_session.take()),
                    message: message.to_owned(),
                },
                (),
            ))
        });
    }
    pub(crate) fn requirements_selected(
        &self,
        call_id: Option<u64>,
        action: &str,
        snapshot: &Value,
    ) {
        self.queue_event(QueuedEvent::RequirementsSelected {
            call_id,
            action: action.to_owned(),
            snapshot: snapshot.clone(),
        });
    }
    pub(crate) fn requirements_accepted(&self, requirements: &Value) {
        self.queue_event(QueuedEvent::RequirementsAccepted {
            session: self.4.to_string(),
            requirements: requirements.clone(),
        });
    }
    pub(crate) fn python_environment_accepted(&self, packages: &[String]) {
        self.queue_event(QueuedEvent::PythonEnvironmentAccepted {
            packages: packages.to_vec(),
        });
    }
}
