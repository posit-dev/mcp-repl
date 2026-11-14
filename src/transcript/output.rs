use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::{CaptureFile, Owner, Transcript, queue::QueuedEvent};

pub(super) const MAX_CELL_OUTPUT_BYTES: u64 = 1024 * 1024 * 1024;

/// One output file's state, shared by its producer, the recording writer, and
/// reply rendering. Only the writer performs I/O on the file.
pub(super) struct OutputFile {
    file: Arc<Mutex<CaptureFile>>,
    path: String,
    /// Bytes the writer has appended.
    written: AtomicU64,
    failed: AtomicBool,
    /// The writer's failure report, taken by the first reply that cites this file.
    notice: Mutex<Option<String>>,
    /// Set once the writer has closed the file.
    closed: AtomicBool,
    transcript: Transcript,
}

impl Drop for OutputFile {
    /// Dropping an open file closes it, and the last reference can drop on
    /// any thread: a reply's, a producer's, or one that stops recording and
    /// discards the queued close. Leave an open file to the writer.
    fn drop(&mut self) {
        if !self.closed.load(Ordering::Acquire) {
            self.transcript.retire(self.file.clone());
        }
    }
}

impl OutputFile {
    /// Writer only. Returns the retained prefix length of `data`, or `None`
    /// when an earlier write to this file already failed.
    pub(super) fn write(&self, offset: u64, data: &[u8]) -> Option<usize> {
        if self.failed() {
            return None;
        }
        let mut file = self.file.lock().expect("capture file lock");
        let result = file
            .writer
            .as_mut()
            .expect("open output interval")
            .write_all(data);
        let Err(error) = result else {
            self.written.fetch_add(data.len() as u64, Ordering::Release);
            return Some(data.len());
        };
        let observed = std::fs::metadata(&file.path)
            .map(|m| m.len())
            .unwrap_or(offset);
        // Publish the final count before the flag that selects it.
        self.written.store(observed, Ordering::Release);
        self.failed.store(true, Ordering::Release);
        *self.notice.lock().expect("output notice lock") = Some(format!(
            "cell output file {} (relative to recording) stopped after {observed} retained bytes: {error}; later text is not retained in this file",
            self.path
        ));
        Some(observed.saturating_sub(offset) as usize)
    }

    /// Writer only. Runs after the file's last queued write.
    pub(super) fn close(&self) {
        if let Err(error) = self.file.lock().expect("capture file lock").finish() {
            self.notice
                .lock()
                .expect("output notice lock")
                .get_or_insert(error);
        }
        // `finish` releases the handle even when it reports an error.
        self.closed.store(true, Ordering::Release);
    }

    pub(super) fn written(&self) -> u64 {
        self.written.load(Ordering::Acquire)
    }

    fn failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }
}

/// One private append-only file containing cell or session output.
///
/// The pending-output tape remains independently bounded for MCP projection.
/// This file retains console text and direct stdout/stderr bytes before that
/// projection discards overflow. Images remain separate transcript artifacts.
///
/// The producer only queues bytes; the recording writer appends them. Byte
/// counts therefore describe queued bytes until the writer reports a failure
/// or recording stops, and written bytes afterwards.
pub(crate) struct CellOutput {
    file: Arc<OutputFile>,
    public_directory: Arc<Mutex<std::path::PathBuf>>,
    owner: Owner,
    transcript: Transcript,
    /// Bytes handed to the writer.
    queued_bytes: u64,
    record: Arc<Mutex<OutputRecordState>>,
    discarded_bytes: u64,
    retention_limit_reported: bool,
}

#[derive(Clone, Copy, Default, PartialEq, serde::Serialize)]
pub(super) struct CellOutputSummary {
    pub(super) retained_bytes: u64,
    pub(super) inline_omitted_bytes: u64,
    pub(super) discarded_bytes: u64,
}

/// One response interval's accounting receipt, shared with delivery recovery.
#[derive(Clone)]
pub(crate) struct OutputRecord {
    state: Arc<Mutex<OutputRecordState>>,
    pub(crate) public_directory: Arc<Mutex<std::path::PathBuf>>,
    pub(crate) path: Arc<str>,
    accounted: Arc<AtomicU64>,
    inspection_only: bool,
    file: Arc<OutputFile>,
}

struct OutputRecordState {
    file: Arc<OutputFile>,
    transcript: Transcript,
    call_id: Option<u64>,
    owner: Owner,
    path: String,
    summary: CellOutputSummary,
    finished: bool,
    published: Option<CellOutputSummary>,
}

impl OutputRecordState {
    fn publish(&mut self) {
        if self.finished && self.published != Some(self.summary) {
            self.transcript.queue_event(QueuedEvent::OutputSummary {
                call_id: self.call_id,
                owner: self.owner,
                path: self.path.clone(),
                summary: self.summary,
                file: self.file.clone(),
            });
            self.published = Some(self.summary);
        }
    }
}

impl Drop for OutputRecordState {
    fn drop(&mut self) {
        self.publish();
    }
}

impl OutputRecord {
    /// Keep paths and byte counts available to an inspector without letting
    /// preview compaction publish delivery accounting through the shared receipt.
    pub(crate) fn inspection_copy(&self) -> Self {
        Self {
            inspection_only: true,
            ..self.clone()
        }
    }

    pub(crate) fn note_inline_omission(&self, bytes: u64) {
        if self.inspection_only {
            return;
        }
        let previous = self.accounted.fetch_max(bytes, Ordering::Relaxed);
        if bytes > previous {
            self.state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .summary
                .inline_omitted_bytes += bytes - previous;
        }
    }

    pub(crate) fn publish(&self) {
        if self.inspection_only {
            return;
        }
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .publish();
    }

    /// Whether the writer stopped retaining this file before its producer did.
    pub(crate) fn truncated(&self) -> bool {
        self.file.failed()
    }

    pub(crate) fn same_file(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.file, &other.file)
    }

    /// Take the writer's failure report for a reply that cites this file.
    pub(crate) fn take_notice(&self) -> Option<String> {
        if self.inspection_only {
            return None;
        }
        self.file.notice.lock().expect("output notice lock").take()
    }
}

impl Transcript {
    pub(crate) fn create_session_output(&self) -> Result<Option<CellOutput>, String> {
        self.create_output(None)
    }
    pub(crate) fn create_cell_output(
        &self,
        call_id: Option<u64>,
        omit_from_projections: bool,
    ) -> Result<Option<CellOutput>, String> {
        if self.start_cell(call_id, omit_from_projections).is_none() {
            return Ok(None);
        }
        self.create_output(call_id)
    }
    fn create_output(&self, call_id: Option<u64>) -> Result<Option<CellOutput>, String> {
        let Some(owner) = self.owner() else {
            return Ok(None);
        };
        let path = owner.path(".txt");
        let file = Arc::new(OutputFile {
            file: self.3.create_output_file(&path)?,
            path: path.clone(),
            written: AtomicU64::new(0),
            failed: AtomicBool::new(false),
            notice: Mutex::new(None),
            closed: AtomicBool::new(false),
            transcript: self.clone(),
        });
        Ok(Some(CellOutput {
            file: file.clone(),
            public_directory: self.3.public_directory.clone(),
            owner,
            transcript: self.clone(),
            record: Arc::new(Mutex::new(OutputRecordState {
                file,
                transcript: self.clone(),
                call_id,
                owner,
                path,
                summary: CellOutputSummary::default(),
                finished: false,
                published: None,
            })),
            queued_bytes: 0,
            discarded_bytes: 0,
            retention_limit_reported: false,
        }))
    }
}

impl CellOutput {
    pub(crate) fn record(&self) -> OutputRecord {
        OutputRecord {
            state: self.record.clone(),
            public_directory: self.public_directory.clone(),
            path: Arc::from(self.file.path.as_str()),
            accounted: Arc::new(AtomicU64::new(0)),
            inspection_only: false,
            file: self.file.clone(),
        }
    }

    pub(crate) fn retained_bytes(&self) -> u64 {
        if self.file.failed() || self.transcript.stopped() {
            self.file.written().min(self.queued_bytes)
        } else {
            self.queued_bytes
        }
    }
    pub(crate) fn discarded_bytes(&self) -> u64 {
        self.discarded_bytes + self.queued_bytes - self.retained_bytes()
    }
    /// Queues bytes for the writer while the retention limit permits it.
    ///
    /// Returns a server-owned notice to publish after the corresponding
    /// ordinary output event. Further writes are still drained from the worker
    /// even after persistence stops. A write failure is not known here: the
    /// writer leaves its report for the first reply that cites this file.
    pub(crate) fn append(&mut self, bytes: &[u8]) -> Option<String> {
        if bytes.is_empty() {
            return None;
        }
        if self.file.failed() {
            self.discarded_bytes = self.discarded_bytes.saturating_add(bytes.len() as u64);
            return None;
        }
        let remaining = MAX_CELL_OUTPUT_BYTES.saturating_sub(self.queued_bytes);
        let retained = bytes
            .len()
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        let discarded = bytes.len() - retained;
        self.transcript.queue_data(QueuedEvent::OutputWrite {
            owner: self.owner,
            path: self.file.path.clone(),
            offset: self.queued_bytes,
            data: bytes[..retained].to_vec(),
            discarded_bytes: discarded,
            file: self.file.clone(),
        });
        self.queued_bytes = self.queued_bytes.saturating_add(retained as u64);
        if discarded == 0 {
            return None;
        }
        self.discarded_bytes = self.discarded_bytes.saturating_add(discarded as u64);
        if self.retention_limit_reported {
            return None;
        }
        self.retention_limit_reported = true;
        Some(format!(
            "cell output retention limit reached at {MAX_CELL_OUTPUT_BYTES} bytes for {} (relative to recording); later text is not retained in this file",
            self.file.path
        ))
    }

    /// Queues the file's close and releases its summary. The writer corrects
    /// the summary's counts to what it wrote before journaling them.
    pub(crate) fn finish(self) {
        self.transcript.queue_event(QueuedEvent::OutputClose {
            file: self.file.clone(),
        });
        let mut record = self
            .record
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        record.summary.retained_bytes = self.queued_bytes;
        record.summary.discarded_bytes = self.discarded_bytes;
        record.finished = true;
    }
}
