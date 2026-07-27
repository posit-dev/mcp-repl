#[cfg(any(test, target_os = "windows"))]
use std::borrow::Cow;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread;
use std::time::Duration;
#[cfg(target_family = "windows")]
use std::time::Instant;

#[cfg(all(test, target_family = "unix"))]
use std::cell::RefCell;
#[cfg(target_family = "unix")]
use std::collections::{HashMap, HashSet};
#[cfg(any(target_family = "unix", target_family = "windows"))]
use std::fs::File;
#[cfg(target_family = "unix")]
use std::fs::OpenOptions;

use crate::backend::{
    Backend, CustomWorkerSpec, CustomWorkerWorkingDir, CustomWorkerWorkingDirPolicy, WorkerLaunch,
    WorkerStdinTransport,
};
#[cfg(target_family = "windows")]
use crate::ipc::{IPC_PIPE_FROM_WORKER_ENV, IPC_PIPE_TO_WORKER_ENV};
#[cfg(target_family = "unix")]
use crate::ipc::{IPC_READ_FD_ENV, IPC_WRITE_FD_ENV};
use crate::ipc::{
    IpcHandle, IpcInputLineEvent, IpcInputReadiness, IpcServer, IpcWaitError, ServerIpcConnection,
    ServerToWorkerIpcMessage, WorkerToServerIpcMessage,
};
#[cfg(any(target_family = "unix", target_family = "windows"))]
use crate::ipc::{IpcHandlers, IpcOutputImage};
use crate::output_capture::OutputTimeline;
use crate::oversized_output::OversizedOutputMode;
use crate::pending_output_tape::PendingSidebandKind;
use crate::sandbox::{
    R_SESSION_TMPDIR_ENV, SandboxState, prepare_worker_command_with_managed_network,
};
use crate::worker_process::{
    PREVIOUS_IMAGE_UPDATE_NOTICE, WorkerError, worker_context_event_payload,
};
use crate::worker_protocol::{ContentOrigin, TextStream, WORKER_MODE_ARG};

#[cfg(target_family = "unix")]
use portable_pty::{PtySize, native_pty_system};
#[cfg(target_family = "unix")]
use std::os::unix::io::{AsRawFd, FromRawFd};
#[cfg(target_family = "unix")]
use std::os::unix::process::CommandExt;
#[cfg(target_family = "windows")]
use std::os::windows::io::AsRawHandle;
#[cfg(target_family = "windows")]
use std::os::windows::process::CommandExt;
#[cfg(target_family = "windows")]
use std::os::windows::process::ExitStatusExt;
#[cfg(target_family = "unix")]
use sysinfo::{Pid, ProcessesToUpdate, System};
#[cfg(target_family = "windows")]
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_HANDLE_EOF, HANDLE, WAIT_FAILED, WAIT_TIMEOUT,
};
#[cfg(target_family = "windows")]
use windows_sys::Win32::System::Pipes::PeekNamedPipe;
#[cfg(target_family = "windows")]
use windows_sys::Win32::System::Threading::{
    CREATE_NEW_PROCESS_GROUP, GetExitCodeProcess, PROCESS_INFORMATION, TerminateProcess,
    WaitForSingleObject,
};

#[cfg(all(test, target_family = "unix"))]
thread_local! {
    static TEST_UNIX_KILL_RECORDER: RefCell<Option<Vec<(i32, i32)>>> = const { RefCell::new(None) };
}

#[cfg(target_family = "unix")]
fn raw_unix_kill(target: i32, signal: i32) -> i32 {
    #[cfg(test)]
    if let Ok(Some(result)) = TEST_UNIX_KILL_RECORDER.try_with(|recorder| {
        let mut recorder = recorder.borrow_mut();
        recorder.as_mut().map(|calls| {
            calls.push((target, signal));
            0
        })
    }) {
        return result;
    }

    unsafe { libc::kill(target, signal) }
}

#[cfg(all(test, target_family = "unix"))]
pub(crate) fn capture_recorded_unix_kills<F, R>(f: F) -> (R, Vec<(i32, i32)>)
where
    F: FnOnce() -> R,
{
    TEST_UNIX_KILL_RECORDER.with(|recorder| {
        assert!(
            recorder.borrow().is_none(),
            "did not expect nested unix kill recorder"
        );
        *recorder.borrow_mut() = Some(Vec::new());
    });
    let result = f();
    let kills = TEST_UNIX_KILL_RECORDER
        .with(|recorder| recorder.borrow_mut().take().expect("recorded kills"));
    (result, kills)
}

#[derive(Debug, Clone)]
pub(crate) struct GuardrailEvent {
    pub(crate) message: String,
    pub(crate) was_busy: bool,
    pub(crate) is_error: bool,
}

#[derive(Clone)]
pub(crate) struct GuardrailShared {
    pub(crate) event: Arc<Mutex<Option<GuardrailEvent>>>,
    pub(crate) busy: Arc<AtomicBool>,
}

#[derive(Clone)]
pub(crate) struct LiveOutputCapture {
    output_timeline: OutputTimeline,
    #[cfg(any(test, target_os = "windows"))]
    windows_conpty_startup_noise_filter: Option<Arc<Mutex<WindowsConptyStartupNoiseFilter>>>,
}

impl LiveOutputCapture {
    pub(crate) fn new(
        _oversized_output: OversizedOutputMode,
        output_timeline: OutputTimeline,
    ) -> Self {
        Self {
            output_timeline,
            #[cfg(any(test, target_os = "windows"))]
            windows_conpty_startup_noise_filter: None,
        }
    }

    #[cfg(any(test, target_os = "windows"))]
    fn with_windows_conpty_startup_noise_filter(mut self) -> Self {
        self.windows_conpty_startup_noise_filter = Some(Arc::new(Mutex::new(
            WindowsConptyStartupNoiseFilter::default(),
        )));
        self
    }

    pub(crate) fn append_output_text(
        &self,
        bytes: &[u8],
        stream: TextStream,
        is_continuation: bool,
    ) {
        #[cfg(any(test, target_os = "windows"))]
        if let Some(filter) = &self.windows_conpty_startup_noise_filter {
            let mut filter = filter.lock().unwrap();
            // IPC and raw ConPTY readers race for this lock. Resolve anything
            // that arrived earlier before deciding whether this IPC LF is the
            // missing prefix of a byte-exact shutdown frame.
            if matches!(stream, TextStream::Stdout)
                && filter.consume_raw_first_shutdown_pair(bytes, is_continuation)
            {
                drop(filter);
                return;
            }
            self.flush_windows_conpty_before_observable_event_locked(&mut filter);
            if matches!(stream, TextStream::Stdout)
                && filter.stage_ipc_shutdown_lf(bytes, is_continuation)
            {
                drop(filter);
                return;
            }
            filter.note_ipc_output(bytes, stream);
            self.append_text(bytes, stream, is_continuation, true);
            drop(filter);
            return;
        }
        self.append_text(bytes, stream, is_continuation, true);
    }

    fn append_raw_text(&self, bytes: &[u8], stream: TextStream) {
        #[cfg(any(test, target_os = "windows"))]
        if let Some(filter) = &self.windows_conpty_startup_noise_filter {
            let mut filter = filter.lock().unwrap();
            if matches!(stream, TextStream::Stdout) {
                let filtered = filter.filter(bytes);
                self.append_windows_conpty_stdout_parts(filtered.ipc_lf, filtered.raw.as_deref());
            } else {
                self.flush_windows_conpty_before_observable_event_locked(&mut filter);
                self.append_text(bytes, stream, false, false);
            }
            drop(filter);
            return;
        }
        self.append_text(bytes, stream, false, false);
    }

    fn note_windows_conpty_shutdown_starting(&self) {
        #[cfg(any(test, target_os = "windows"))]
        if let Some(filter) = &self.windows_conpty_startup_noise_filter {
            let mut filter = filter.lock().unwrap();
            let pending = filter.arm_shutdown_reset();
            if let Some(bytes) = pending {
                self.append_text(&bytes, TextStream::Stdout, false, false);
            }
            drop(filter);
        }
    }

    #[cfg(test)]
    fn flush_unarmed_windows_conpty_ambiguous_lf(&self) {
        #[cfg(any(test, target_os = "windows"))]
        if let Some(filter) = &self.windows_conpty_startup_noise_filter {
            let mut filter = filter.lock().unwrap();
            self.flush_unarmed_windows_conpty_ambiguous_lf_locked(&mut filter);
            drop(filter);
        }
    }

    #[cfg(test)]
    fn flush_unarmed_windows_conpty_ambiguous_lf_locked(
        &self,
        filter: &mut WindowsConptyStartupNoiseFilter,
    ) {
        if let Some(bytes) = filter.flush_unarmed_ambiguous_lf() {
            self.append_text(&bytes, TextStream::Stdout, false, false);
        }
    }

    #[cfg(any(test, target_os = "windows"))]
    fn append_windows_conpty_stdout_parts(
        &self,
        pending_lf: Option<PendingIpcShutdownLf>,
        raw: Option<&[u8]>,
    ) {
        let raw = raw.unwrap_or_default();
        let Some(pending_lf) = pending_lf else {
            if !raw.is_empty() {
                self.append_text(raw, TextStream::Stdout, false, false);
            }
            return;
        };

        let split = pending_lf.raw_prefix_len.min(raw.len());
        if split > 0 {
            self.append_text(&raw[..split], TextStream::Stdout, false, false);
        }
        self.append_text(b"\n", TextStream::Stdout, pending_lf.is_continuation, true);
        if split < raw.len() {
            self.append_text(&raw[split..], TextStream::Stdout, false, false);
        }
    }

    #[cfg(any(test, target_os = "windows"))]
    fn flush_windows_conpty_before_observable_event_locked(
        &self,
        filter: &mut WindowsConptyStartupNoiseFilter,
    ) {
        let finished = filter.flush_pending_raw_before_observable_event();
        self.append_windows_conpty_stdout_parts(finished.ipc_lf, finished.raw.as_deref());
    }

    fn finalize_windows_conpty_raw_text(&self) {
        #[cfg(any(test, target_os = "windows"))]
        if let Some(filter) = &self.windows_conpty_startup_noise_filter {
            let mut filter = filter.lock().unwrap();
            let finished = filter.finalize();
            self.append_windows_conpty_stdout_parts(finished.ipc_lf, finished.raw.as_deref());
            if filter.take_pending_session_end() {
                self.output_timeline.append_session_end();
            }
            drop(filter);
        }
    }

    fn finish_raw_text(&self, stream: TextStream) {
        #[cfg(any(test, target_os = "windows"))]
        if matches!(stream, TextStream::Stdout) {
            let Some(filter) = &self.windows_conpty_startup_noise_filter else {
                return;
            };
            let mut filter = filter.lock().unwrap();
            let finished = filter.finish_reader();
            self.append_windows_conpty_stdout_parts(finished.ipc_lf, finished.raw.as_deref());
            drop(filter);
        }
        #[cfg(not(any(test, target_os = "windows")))]
        let _ = stream;
    }

    fn note_accepted_input_starting(&self) {
        // ConPTY startup bytes can remain queued in the raw-output pipe after
        // IPC accepts the first request. Filtering is anchored to the beginning
        // of raw stdout, not request timing.
    }

    fn append_text(
        &self,
        bytes: &[u8],
        stream: TextStream,
        is_continuation: bool,
        is_output_text: bool,
    ) {
        match stream {
            TextStream::Stdout => {
                if is_output_text {
                    self.output_timeline.append_ipc_text_with_continuation(
                        bytes,
                        false,
                        ContentOrigin::Worker,
                        is_continuation,
                    );
                } else {
                    self.output_timeline
                        .append_text(bytes, false, ContentOrigin::Worker);
                }
            }
            TextStream::Stderr => {
                if is_output_text {
                    self.output_timeline.append_ipc_text_with_continuation(
                        bytes,
                        true,
                        ContentOrigin::Worker,
                        is_continuation,
                    );
                } else {
                    self.output_timeline.append_text_with_continuation(
                        bytes,
                        true,
                        ContentOrigin::Worker,
                        is_continuation,
                    );
                }
            }
        }
    }

    pub(crate) fn append_image(&self, image: IpcOutputImage) {
        #[cfg(any(test, target_os = "windows"))]
        if let Some(filter) = &self.windows_conpty_startup_noise_filter {
            let mut filter = filter.lock().unwrap();
            self.flush_windows_conpty_before_observable_event_locked(&mut filter);
            self.append_image_inner(&image);
            drop(filter);
            return;
        }
        self.append_image_inner(&image);
    }

    fn append_image_inner(&self, image: &IpcOutputImage) {
        if image.updates_previous_image {
            self.output_timeline.append_text_event(
                PREVIOUS_IMAGE_UPDATE_NOTICE.to_string(),
                false,
                ContentOrigin::Server,
                Some(image.readline_results_seen),
            );
        }
        self.output_timeline.append_image(
            image.id.clone(),
            image.mime_type.clone(),
            image.data.clone(),
            image.is_new,
            image.readline_results_seen,
        );
    }

    pub(crate) fn append_sideband(&self, kind: PendingSidebandKind) {
        #[cfg(any(test, target_os = "windows"))]
        if let Some(filter) = &self.windows_conpty_startup_noise_filter {
            let mut filter = filter.lock().unwrap();
            if matches!(&kind, PendingSidebandKind::SessionEnd) {
                if let Some(bytes) = filter.arm_shutdown_reset() {
                    self.append_text(&bytes, TextStream::Stdout, false, false);
                }
                if filter.defer_session_end() {
                    self.output_timeline.append_session_end();
                }
            } else {
                self.flush_windows_conpty_before_observable_event_locked(&mut filter);
                self.append_sideband_inner(kind);
            }
            drop(filter);
            return;
        }
        self.append_sideband_inner(kind);
    }

    fn append_sideband_inner(&self, kind: PendingSidebandKind) {
        match kind {
            PendingSidebandKind::InputWait { .. } => self.output_timeline.append_input_wait(),
            PendingSidebandKind::ReadlineResult { prompt, line } => {
                self.output_timeline.append_input_echo(&prompt, &line);
            }
            PendingSidebandKind::RequestBoundary => self.output_timeline.append_request_boundary(),
            PendingSidebandKind::SessionEnd => self.output_timeline.append_session_end(),
        }
    }
}

#[cfg(any(test, target_os = "windows"))]
struct WindowsConptyStartupNoiseFilter {
    prefix: Vec<u8>,
    matched: bool,
    finished: bool,
    startup_prefix_cross_route_blocked: bool,
    startup_ipc_lf_boundary: Option<usize>,
    shutdown_reset: Option<WindowsConptyShutdownResetFilter>,
    // Hosted ConPTY can split one reset frame across output_text (the LF)
    // and raw capture (the ANSI suffix), in either reader order.
    pending_ipc_shutdown_lf: Option<PendingIpcShutdownLf>,
    ipc_stdout_seen_since_arm: bool,
    session_end_pending: bool,
    raw_output_finalized: bool,
}

#[cfg(any(test, target_os = "windows"))]
impl Default for WindowsConptyStartupNoiseFilter {
    fn default() -> Self {
        Self {
            prefix: Vec::new(),
            matched: false,
            finished: false,
            startup_prefix_cross_route_blocked: false,
            startup_ipc_lf_boundary: None,
            shutdown_reset: Some(WindowsConptyShutdownResetFilter::default()),
            pending_ipc_shutdown_lf: None,
            ipc_stdout_seen_since_arm: false,
            session_end_pending: false,
            raw_output_finalized: false,
        }
    }
}

#[cfg(any(test, target_os = "windows"))]
#[derive(Clone, Copy)]
struct PendingIpcShutdownLf {
    is_continuation: bool,
    raw_prefix_len: usize,
}

#[cfg(any(test, target_os = "windows"))]
struct WindowsConptyFilteredRaw<'a> {
    ipc_lf: Option<PendingIpcShutdownLf>,
    raw: Option<Cow<'a, [u8]>>,
}

#[cfg(any(test, target_os = "windows"))]
struct WindowsConptyStartupFilteredRaw<'a> {
    raw: Option<Cow<'a, [u8]>>,
    ipc_lf_boundary: Option<usize>,
}

#[cfg(any(test, target_os = "windows"))]
struct WindowsConptyShutdownFilteredRaw<'a> {
    raw: Option<Cow<'a, [u8]>>,
    ipc_lf_raw_prefix_len: Option<usize>,
}

#[cfg(any(test, target_os = "windows"))]
struct WindowsConptyFinishedRaw {
    ipc_lf: Option<PendingIpcShutdownLf>,
    raw: Option<Vec<u8>>,
}

#[cfg(any(test, target_os = "windows"))]
impl WindowsConptyStartupNoiseFilter {
    const PREFIX: &'static [u8] = b"\x1b[?9001h\x1b[?1004h";

    fn filter<'a>(&mut self, bytes: &'a [u8]) -> WindowsConptyFilteredRaw<'a> {
        // ConPTY startup noise belongs exclusively to the raw stream. Remove
        // it before evaluating whether the remaining bytes are the raw half
        // of a shutdown frame split across raw and sideband stdout.
        let startup_filtered = self.filter_startup(bytes);
        let block_bare_pair =
            std::mem::take(&mut self.startup_prefix_cross_route_blocked) && !self.matched;
        let dropped_bare_before = self
            .shutdown_reset
            .as_ref()
            .map_or(0, |filter| filter.dropped_bare_reset_count);
        let allow_bare_reset_drop =
            self.pending_ipc_shutdown_lf.is_some() && !self.ipc_stdout_seen_since_arm;
        if block_bare_pair && let Some(filter) = self.shutdown_reset.as_mut() {
            filter.block_pending_bare_pair();
        }
        if let Some(boundary) = startup_filtered.ipc_lf_boundary
            && let Some(filter) = self.shutdown_reset.as_mut()
        {
            filter.mark_pending_ipc_lf_boundary(boundary);
        }
        let shutdown_filtered = match (self.shutdown_reset.as_mut(), startup_filtered.raw) {
            (Some(filter), Some(Cow::Borrowed(bytes))) => {
                filter.filter(bytes, allow_bare_reset_drop)
            }
            (Some(filter), Some(Cow::Owned(bytes))) => {
                let filtered = filter.filter(&bytes, allow_bare_reset_drop);
                WindowsConptyShutdownFilteredRaw {
                    raw: filtered.raw.map(|raw| Cow::Owned(raw.into_owned())),
                    ipc_lf_raw_prefix_len: filtered.ipc_lf_raw_prefix_len,
                }
            }
            (Some(_), None) => WindowsConptyShutdownFilteredRaw {
                raw: None,
                ipc_lf_raw_prefix_len: None,
            },
            (None, raw) => WindowsConptyShutdownFilteredRaw {
                ipc_lf_raw_prefix_len: startup_filtered.ipc_lf_boundary,
                raw,
            },
        };
        let dropped_bare_reset = self
            .shutdown_reset
            .as_ref()
            .is_some_and(|filter| filter.dropped_bare_reset_count != dropped_bare_before);
        let ipc_lf = if dropped_bare_reset {
            let paired_lf = self.pending_ipc_shutdown_lf.take();
            debug_assert!(paired_lf.is_some());
            None
        } else if let Some(raw_prefix_len) = shutdown_filtered.ipc_lf_raw_prefix_len {
            self.take_pending_ipc_shutdown_lf_for_output(raw_prefix_len)
        } else {
            None
        };
        WindowsConptyFilteredRaw {
            ipc_lf,
            raw: shutdown_filtered.raw,
        }
    }

    fn arm_shutdown_reset(&mut self) -> Option<Vec<u8>> {
        let filter = self.shutdown_reset.as_mut()?;
        if filter.armed {
            return None;
        }
        let pending = filter.arm(self.pending_ipc_shutdown_lf.is_some());
        self.ipc_stdout_seen_since_arm = false;
        pending
    }

    fn consume_raw_first_shutdown_pair(&mut self, bytes: &[u8], is_continuation: bool) -> bool {
        let can_pair = bytes == b"\n"
            && !is_continuation
            && !self.ipc_stdout_seen_since_arm
            && self.pending_ipc_shutdown_lf.is_none();
        can_pair
            && self
                .shutdown_reset
                .as_mut()
                .is_some_and(WindowsConptyShutdownResetFilter::consume_pending_bare_reset)
    }

    fn stage_ipc_shutdown_lf(&mut self, bytes: &[u8], is_continuation: bool) -> bool {
        // Before an explicit server shutdown arm, a standalone IPC LF is
        // runtime output and must not be reclassified as terminal noise.
        let can_pair = bytes == b"\n"
            && !is_continuation
            && !self.ipc_stdout_seen_since_arm
            && self
                .shutdown_reset
                .as_ref()
                .is_some_and(|filter| filter.armed && !filter.finished);
        if can_pair && self.pending_ipc_shutdown_lf.is_none() {
            self.pending_ipc_shutdown_lf = Some(PendingIpcShutdownLf {
                is_continuation,
                raw_prefix_len: 0,
            });
            if self.finished {
                if let Some(filter) = self.shutdown_reset.as_mut() {
                    filter.mark_pending_ipc_lf_boundary(0);
                }
            } else {
                debug_assert!(self.startup_ipc_lf_boundary.is_none());
                self.startup_ipc_lf_boundary = Some(self.prefix.len());
            }
            return true;
        }
        false
    }

    fn note_ipc_output(&mut self, bytes: &[u8], stream: TextStream) {
        if bytes.is_empty() {
            return;
        }
        if matches!(stream, TextStream::Stdout)
            && self
                .shutdown_reset
                .as_ref()
                .is_some_and(|filter| filter.armed && !filter.finished)
        {
            self.ipc_stdout_seen_since_arm = true;
        }
    }

    fn take_pending_ipc_shutdown_lf_for_output(
        &mut self,
        raw_prefix_len: usize,
    ) -> Option<PendingIpcShutdownLf> {
        let pending = self.pending_ipc_shutdown_lf.take().map(|mut pending| {
            pending.raw_prefix_len = raw_prefix_len;
            pending
        });
        if pending.is_some() {
            self.ipc_stdout_seen_since_arm = true;
        }
        pending
    }

    fn flush_pending_raw_before_observable_event(&mut self) -> WindowsConptyFinishedRaw {
        if !self.finished && !self.matched && !self.prefix.is_empty() {
            self.startup_prefix_cross_route_blocked = true;
        }
        let mut drained = self
            .shutdown_reset
            .as_mut()
            .map_or_else(WindowsConptyShutdownDrain::default, |filter| {
                filter.flush_pending_before_observable_event()
            });
        if self.startup_ipc_lf_boundary.take().is_some() {
            debug_assert!(drained.ipc_lf_raw_prefix_len.is_none());
            // Startup matching is deliberately raw-stream-local: sideband and
            // stderr may overtake an ambiguous raw prefix while another raw
            // read can still complete the exact startup sequence. Preserve
            // that candidate, but release the staged IPC LF before the new
            // observable event. If the candidate is later disproved, its raw
            // bytes retain the startup filter's established cross-route order.
            drained.ipc_lf_raw_prefix_len = Some(drained.visible.len());
        }
        if let Some(filter) = self.shutdown_reset.as_mut() {
            filter.force_pending_ipc_lf_boundary(&mut drained);
        }
        let ipc_lf = if self.pending_ipc_shutdown_lf.is_some() {
            self.take_pending_ipc_shutdown_lf_for_output(drained.ipc_lf_raw_prefix_len.unwrap_or(0))
        } else {
            None
        };
        WindowsConptyFinishedRaw {
            ipc_lf,
            raw: (!drained.visible.is_empty()).then_some(drained.visible),
        }
    }

    #[cfg(test)]
    fn flush_unarmed_ambiguous_lf(&mut self) -> Option<Vec<u8>> {
        let mut drained = WindowsConptyShutdownDrain::default();
        self.shutdown_reset
            .as_mut()?
            .flush_unarmed_ambiguous_lf(&mut drained);
        (!drained.visible.is_empty()).then_some(drained.visible)
    }

    fn finish_reader(&mut self) -> WindowsConptyFinishedRaw {
        let startup_filtered = self.finish_startup_at_eof();
        let dropped_bare_before = self
            .shutdown_reset
            .as_ref()
            .map_or(0, |filter| filter.dropped_bare_reset_count);
        let allow_bare_reset_drop =
            self.pending_ipc_shutdown_lf.is_some() && !self.ipc_stdout_seen_since_arm;
        let drained = self.shutdown_reset.as_mut().map_or_else(
            WindowsConptyShutdownDrain::default,
            |filter| {
                if let Some(boundary) = startup_filtered.ipc_lf_boundary {
                    filter.mark_pending_ipc_lf_boundary(boundary);
                }
                if let Some(raw) = startup_filtered.raw.as_deref() {
                    filter.append_pending(raw);
                }
                filter.finish_reader(allow_bare_reset_drop)
            },
        );
        let dropped_bare_reset = self
            .shutdown_reset
            .as_ref()
            .is_some_and(|filter| filter.dropped_bare_reset_count != dropped_bare_before);
        let ipc_lf = if dropped_bare_reset {
            let paired_lf = self.pending_ipc_shutdown_lf.take();
            debug_assert!(paired_lf.is_some());
            None
        } else if let Some(raw_prefix_len) = drained.ipc_lf_raw_prefix_len {
            self.take_pending_ipc_shutdown_lf_for_output(raw_prefix_len)
        } else {
            None
        };
        WindowsConptyFinishedRaw {
            ipc_lf,
            raw: (!drained.visible.is_empty()).then_some(drained.visible),
        }
    }

    fn defer_session_end(&mut self) -> bool {
        if self.raw_output_finalized {
            true
        } else {
            self.session_end_pending = true;
            false
        }
    }

    fn take_pending_session_end(&mut self) -> bool {
        std::mem::take(&mut self.session_end_pending)
    }

    fn finalize(&mut self) -> WindowsConptyFinishedRaw {
        let startup_filtered = self.finish_startup_at_eof();
        let mut drained = self.shutdown_reset.as_mut().map_or_else(
            WindowsConptyShutdownDrain::default,
            |filter| {
                if let Some(boundary) = startup_filtered.ipc_lf_boundary {
                    filter.mark_pending_ipc_lf_boundary(boundary);
                }
                if let Some(raw) = startup_filtered.raw.as_deref() {
                    filter.append_pending(raw);
                }
                filter.finalize()
            },
        );
        if self.startup_ipc_lf_boundary.take().is_some() && drained.ipc_lf_raw_prefix_len.is_none()
        {
            drained.ipc_lf_raw_prefix_len = Some(0);
        }
        let ipc_lf = if self.pending_ipc_shutdown_lf.is_some() {
            self.take_pending_ipc_shutdown_lf_for_output(drained.ipc_lf_raw_prefix_len.unwrap_or(0))
        } else {
            None
        };
        self.raw_output_finalized = true;
        WindowsConptyFinishedRaw {
            ipc_lf,
            raw: (!drained.visible.is_empty()).then_some(drained.visible),
        }
    }

    fn filter_startup<'a>(&mut self, bytes: &'a [u8]) -> WindowsConptyStartupFilteredRaw<'a> {
        if self.finished {
            return WindowsConptyStartupFilteredRaw {
                raw: Some(Cow::Borrowed(bytes)),
                ipc_lf_boundary: None,
            };
        }
        self.prefix.extend_from_slice(bytes);
        if !self.matched {
            if self.prefix.len() < Self::PREFIX.len() && Self::PREFIX.starts_with(&self.prefix) {
                return WindowsConptyStartupFilteredRaw {
                    raw: None,
                    ipc_lf_boundary: None,
                };
            }
            if !self.prefix.starts_with(Self::PREFIX) {
                return self.finish_startup_with_drop(0);
            }

            self.matched = true;
            self.drop_startup_pending_prefix(Self::PREFIX.len());
        }
        self.finish_after_matched_prefix()
    }

    fn finish_after_matched_prefix<'a>(&mut self) -> WindowsConptyStartupFilteredRaw<'a> {
        let Some(first_visible) = self
            .prefix
            .iter()
            .position(|byte| !matches!(byte, b'\r' | b'\n'))
        else {
            // All of these bytes are classified startup whitespace. Retain
            // only a final LF because it may begin the one shutdown sequence
            // whose reset matcher includes that LF. This keeps an arbitrary
            // blank-line stream from growing the startup buffer without bound.
            let retained = usize::from(self.prefix.last() == Some(&b'\n'));
            self.drop_startup_pending_prefix(self.prefix.len() - retained);
            return WindowsConptyStartupFilteredRaw {
                raw: None,
                ipc_lf_boundary: None,
            };
        };
        if first_visible > 0 && self.prefix[first_visible - 1] == b'\n' {
            let reset_start = first_visible - 1;
            let possible_reset = &self.prefix[reset_start..];
            if possible_reset
                .starts_with(WindowsConptyShutdownResetFilter::LF_PREFIXED_SIMPLE_SEQUENCE)
            {
                return self.finish_startup_with_drop(reset_start);
            }
            if WindowsConptyShutdownResetFilter::LF_PREFIXED_SIMPLE_SEQUENCE
                .starts_with(possible_reset)
            {
                // Earlier whitespace is already known startup noise. Keeping
                // only the possible reset prefix bounds this ambiguity by the
                // fixed reset-sequence length.
                self.drop_startup_pending_prefix(reset_start);
                return WindowsConptyStartupFilteredRaw {
                    raw: None,
                    ipc_lf_boundary: None,
                };
            }
        }
        self.finish_startup_with_drop(first_visible)
    }

    fn finish_startup_at_eof(&mut self) -> WindowsConptyStartupFilteredRaw<'static> {
        if self.finished {
            return WindowsConptyStartupFilteredRaw {
                raw: None,
                ipc_lf_boundary: None,
            };
        }
        if !self.matched {
            return self.finish_startup_with_drop(0);
        }
        let drop_len = self
            .prefix
            .iter()
            .position(|byte| !matches!(byte, b'\r' | b'\n'))
            .unwrap_or(self.prefix.len());
        self.finish_startup_with_drop(drop_len)
    }

    fn finish_startup_with_drop<'a>(
        &mut self,
        drop_len: usize,
    ) -> WindowsConptyStartupFilteredRaw<'a> {
        self.drop_startup_pending_prefix(drop_len);
        let ipc_lf_boundary = self.startup_ipc_lf_boundary.take();
        let raw = std::mem::take(&mut self.prefix);
        self.finished = true;
        WindowsConptyStartupFilteredRaw {
            raw: (!raw.is_empty()).then_some(Cow::Owned(raw)),
            ipc_lf_boundary,
        }
    }

    fn drop_startup_pending_prefix(&mut self, length: usize) {
        self.prefix.drain(..length);
        if let Some(boundary) = self.startup_ipc_lf_boundary.as_mut() {
            *boundary = boundary.saturating_sub(length);
        }
    }
}

#[cfg(any(test, target_os = "windows"))]
#[derive(Default)]
struct WindowsConptyShutdownResetFilter {
    pending: Vec<u8>,
    finished: bool,
    armed: bool,
    reader_finished: bool,
    dropped_bare_reset_count: usize,
    pending_bare_pair_blocked: bool,
    pending_ipc_lf_boundary: Option<usize>,
}

#[cfg(any(test, target_os = "windows"))]
#[derive(Default)]
struct WindowsConptyShutdownDrain {
    visible: Vec<u8>,
    ipc_lf_raw_prefix_len: Option<usize>,
}

#[cfg(any(test, target_os = "windows"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WindowsConptyShutdownResetMatch {
    Complete(usize),
    Prefix,
    NoMatch,
}

#[cfg(any(test, target_os = "windows"))]
impl WindowsConptyShutdownResetFilter {
    const START: &'static [u8] = b"\x1b[?25l";
    const BARE_SIMPLE_SEQUENCE: &'static [u8] = b"\x1b[2J\x1b[m\x1b[H\x1b[?25h";
    const LF_PREFIXED_SIMPLE_SEQUENCE: &'static [u8] = b"\n\x1b[2J\x1b[m\x1b[H\x1b[?25h";
    const SIMPLE_SEQUENCE: &'static [u8] = b"\x1b[?25l\x1b[2J\x1b[m\x1b[H\x1b[?25h";
    const SIMPLE_WITH_TRAILING_MODES: &'static [u8] =
        b"\x1b[?25l\x1b[2J\x1b[m\x1b[H\x1b[?25h\x1b[?9001l\x1b[?1004l";
    const MODES_FIRST_SEQUENCE: &'static [u8] =
        b"\x1b[?25l\x1b[?9001l\x1b[?1004l\x1b[2J\x1b[m\x1b[H\x1b[?25h";
    const TITLED_PREFIX: &'static [u8] =
        b"\x1b[?25l\x1b[?9001l\x1b[?1004l\x1b[2J\x1b[m\x1b[H\x1b]0;";
    const TITLED_SUFFIX: &'static [u8] = b"\x07\x1b[?25h";
    const MAX_TITLE_BYTES: usize = 32 * 1024;

    fn filter<'a>(
        &mut self,
        bytes: &'a [u8],
        allow_bare_reset_drop: bool,
    ) -> WindowsConptyShutdownFilteredRaw<'a> {
        if self.finished {
            return WindowsConptyShutdownFilteredRaw {
                raw: Some(Cow::Borrowed(bytes)),
                ipc_lf_raw_prefix_len: None,
            };
        }

        self.append_pending(bytes);
        let drained = self.drain_pending(false, allow_bare_reset_drop, true);
        WindowsConptyShutdownFilteredRaw {
            raw: (!drained.visible.is_empty()).then_some(Cow::Owned(drained.visible)),
            ipc_lf_raw_prefix_len: drained.ipc_lf_raw_prefix_len,
        }
    }

    fn arm(&mut self, allow_bare_reset_drop: bool) -> Option<Vec<u8>> {
        self.armed = true;
        let drained = self.drain_pending(self.reader_finished, allow_bare_reset_drop, true);
        debug_assert!(drained.ipc_lf_raw_prefix_len.is_none());
        (!drained.visible.is_empty()).then_some(drained.visible)
    }

    fn finish_reader(&mut self, allow_bare_reset_drop: bool) -> WindowsConptyShutdownDrain {
        self.reader_finished = true;
        self.drain_pending(true, allow_bare_reset_drop, true)
    }

    fn finalize(&mut self) -> WindowsConptyShutdownDrain {
        let mut drained = if self.armed {
            self.drain_pending(true, false, false)
        } else {
            self.pending_bare_pair_blocked = false;
            let mut drained = WindowsConptyShutdownDrain::default();
            self.emit_pending_prefix(self.pending.len(), &mut drained);
            drained
        };
        if !self.pending.is_empty() {
            self.emit_pending_prefix(self.pending.len(), &mut drained);
        }
        self.record_pending_ipc_lf_boundary(&mut drained);
        self.finished = true;
        drained
    }

    fn append_pending(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
    }

    fn mark_pending_ipc_lf_boundary(&mut self, append_offset: usize) {
        debug_assert!(self.pending_ipc_lf_boundary.is_none());
        self.pending_ipc_lf_boundary = Some(self.pending.len() + append_offset);
    }

    fn flush_unarmed_ambiguous_lf(&mut self, drained: &mut WindowsConptyShutdownDrain) {
        if !self.armed && !self.finished && self.pending == b"\n" {
            self.emit_pending_prefix(1, drained);
        }
    }

    fn block_pending_bare_pair(&mut self) {
        self.pending_bare_pair_blocked = true;
    }

    fn flush_pending_before_observable_event(&mut self) -> WindowsConptyShutdownDrain {
        let mut drained = WindowsConptyShutdownDrain::default();
        self.flush_unarmed_ambiguous_lf(&mut drained);
        self.flush_or_invalidate_pending_bare_candidate(&mut drained);
        drained
    }

    fn flush_or_invalidate_pending_bare_candidate(
        &mut self,
        drained: &mut WindowsConptyShutdownDrain,
    ) {
        if self.finished
            || self.pending.is_empty()
            || !(Self::BARE_SIMPLE_SEQUENCE.starts_with(&self.pending)
                || self.pending.starts_with(Self::BARE_SIMPLE_SEQUENCE))
        {
            return;
        }

        self.pending_bare_pair_blocked = true;
        // ESC and ESC[ are shared with legacy reset candidates. Keep those
        // ambiguous bytes buffered, but mark them ineligible for cross-route
        // pairing. Once the bytes uniquely identify the bare reset candidate,
        // emit them before the observable boundary to preserve timeline order.
        if Self::START.starts_with(&self.pending) {
            return;
        }

        self.pending_bare_pair_blocked = false;
        self.emit_pending_prefix(self.pending.len(), drained);
    }

    fn force_pending_ipc_lf_boundary(&mut self, drained: &mut WindowsConptyShutdownDrain) {
        let Some(boundary) = self.pending_ipc_lf_boundary.take() else {
            return;
        };
        debug_assert!(boundary <= self.pending.len());
        let boundary = boundary.min(self.pending.len());
        let after_boundary = self.pending.split_off(boundary);

        let mut before = self.drain_pending(true, false, false);
        if !self.pending.is_empty() {
            self.emit_pending_prefix(self.pending.len(), &mut before);
        }
        debug_assert!(before.ipc_lf_raw_prefix_len.is_none());
        drained.visible.extend(before.visible);
        debug_assert!(drained.ipc_lf_raw_prefix_len.is_none());
        drained.ipc_lf_raw_prefix_len = Some(drained.visible.len());

        self.pending = after_boundary;
        self.pending_bare_pair_blocked = false;
        let mut after = self.drain_pending(true, false, false);
        if !self.pending.is_empty() {
            self.emit_pending_prefix(self.pending.len(), &mut after);
        }
        debug_assert!(after.ipc_lf_raw_prefix_len.is_none());
        drained.visible.extend(after.visible);
    }

    fn consume_pending_bare_reset(&mut self) -> bool {
        if self.armed
            && !self.finished
            && !self.pending_bare_pair_blocked
            && self.pending == Self::BARE_SIMPLE_SEQUENCE
        {
            self.pending.clear();
            self.pending_ipc_lf_boundary = None;
            true
        } else {
            false
        }
    }

    fn drain_pending(
        &mut self,
        at_eof: bool,
        allow_bare_reset_drop: bool,
        retain_unpaired_bare_reset: bool,
    ) -> WindowsConptyShutdownDrain {
        let mut drained = WindowsConptyShutdownDrain::default();
        let mut bare_drop_available = allow_bare_reset_drop;
        loop {
            let wait_for_forward_bare_candidate = !at_eof
                && bare_drop_available
                && !self.pending.is_empty()
                && Self::BARE_SIMPLE_SEQUENCE.starts_with(&self.pending);
            if !wait_for_forward_bare_candidate {
                self.record_pending_ipc_lf_boundary(&mut drained);
            }
            if self.pending.is_empty() {
                self.pending_bare_pair_blocked = false;
                break;
            }
            if self.pending_bare_pair_blocked
                && !(Self::BARE_SIMPLE_SEQUENCE.starts_with(&self.pending)
                    || self.pending.starts_with(Self::BARE_SIMPLE_SEQUENCE))
            {
                self.pending_bare_pair_blocked = false;
            }

            if !Self::starts_with_candidate(&self.pending) {
                if let Some(start) = Self::first_candidate_start(&self.pending) {
                    self.emit_pending_prefix(start, &mut drained);
                    self.pending_bare_pair_blocked = false;
                    continue;
                }
                let start_retained = longest_suffix_matching_prefix(&self.pending, Self::START);
                let lf_retained = longest_suffix_matching_prefix(
                    &self.pending,
                    Self::LF_PREFIXED_SIMPLE_SEQUENCE,
                );
                let bare_retained =
                    longest_suffix_matching_prefix(&self.pending, Self::BARE_SIMPLE_SEQUENCE);
                let retained = start_retained.max(lf_retained).max(bare_retained);
                let visible_len = self.pending.len().saturating_sub(retained);
                self.emit_pending_prefix(visible_len, &mut drained);
                if visible_len > 0 {
                    self.pending_bare_pair_blocked = false;
                }
                break;
            }

            match Self::match_at_start(&self.pending, at_eof) {
                WindowsConptyShutdownResetMatch::Complete(length) => {
                    if !self.armed {
                        if at_eof || self.pending.len() == length {
                            break;
                        }
                        self.emit_pending_prefix(1, &mut drained);
                        self.pending_bare_pair_blocked = false;
                        continue;
                    }
                    let bare_reset = length == Self::BARE_SIMPLE_SEQUENCE.len()
                        && self.pending.starts_with(Self::BARE_SIMPLE_SEQUENCE);
                    if bare_reset
                        && (self.pending_bare_pair_blocked
                            || !bare_drop_available
                            || !drained.visible.is_empty())
                    {
                        if retain_unpaired_bare_reset
                            && !self.pending_bare_pair_blocked
                            && self.pending.len() == length
                        {
                            break;
                        }
                        self.emit_pending_prefix(1, &mut drained);
                        self.pending_bare_pair_blocked = false;
                        continue;
                    }
                    if bare_reset {
                        self.dropped_bare_reset_count += 1;
                        bare_drop_available = false;
                    }
                    self.drop_pending_prefix(length, &mut drained);
                    self.pending_bare_pair_blocked = false;
                    continue;
                }
                WindowsConptyShutdownResetMatch::Prefix if !at_eof => break,
                WindowsConptyShutdownResetMatch::Prefix if self.armed => {
                    self.emit_pending_prefix(self.pending.len(), &mut drained);
                    break;
                }
                WindowsConptyShutdownResetMatch::Prefix => break,
                WindowsConptyShutdownResetMatch::NoMatch => {
                    self.emit_pending_prefix(1, &mut drained);
                    self.pending_bare_pair_blocked = false;
                }
            }
        }
        drained
    }

    fn record_pending_ipc_lf_boundary(&mut self, drained: &mut WindowsConptyShutdownDrain) {
        if self.pending_ipc_lf_boundary == Some(0) {
            debug_assert!(drained.ipc_lf_raw_prefix_len.is_none());
            drained.ipc_lf_raw_prefix_len = Some(drained.visible.len());
            self.pending_ipc_lf_boundary = None;
        }
    }

    fn emit_pending_prefix(&mut self, length: usize, drained: &mut WindowsConptyShutdownDrain) {
        if length == 0 {
            return;
        }
        let visible_start = drained.visible.len();
        self.adjust_pending_ipc_lf_boundary(length, visible_start, true, drained);
        drained.visible.extend(self.pending.drain(..length));
    }

    fn drop_pending_prefix(&mut self, length: usize, drained: &mut WindowsConptyShutdownDrain) {
        if length == 0 {
            return;
        }
        let visible_start = drained.visible.len();
        self.adjust_pending_ipc_lf_boundary(length, visible_start, false, drained);
        self.pending.drain(..length);
    }

    fn adjust_pending_ipc_lf_boundary(
        &mut self,
        consumed: usize,
        visible_start: usize,
        emitted: bool,
        drained: &mut WindowsConptyShutdownDrain,
    ) {
        let Some(boundary) = self.pending_ipc_lf_boundary else {
            return;
        };
        if boundary <= consumed {
            debug_assert!(drained.ipc_lf_raw_prefix_len.is_none());
            drained.ipc_lf_raw_prefix_len =
                Some(visible_start + if emitted { boundary } else { 0 });
            self.pending_ipc_lf_boundary = None;
        } else {
            self.pending_ipc_lf_boundary = Some(boundary - consumed);
        }
    }

    fn match_at_start(bytes: &[u8], at_eof: bool) -> WindowsConptyShutdownResetMatch {
        let mut complete = None;
        let mut longer_prefix = false;
        for candidate in [
            Self::BARE_SIMPLE_SEQUENCE,
            Self::LF_PREFIXED_SIMPLE_SEQUENCE,
            Self::SIMPLE_WITH_TRAILING_MODES,
            Self::MODES_FIRST_SEQUENCE,
            Self::SIMPLE_SEQUENCE,
        ] {
            match fixed_shutdown_reset_match(bytes, candidate) {
                WindowsConptyShutdownResetMatch::Complete(length) => {
                    complete = Some(complete.map_or(length, |current: usize| current.max(length)));
                }
                WindowsConptyShutdownResetMatch::Prefix => longer_prefix = true,
                WindowsConptyShutdownResetMatch::NoMatch => {}
            }
        }
        match Self::match_titled_sequence(bytes) {
            WindowsConptyShutdownResetMatch::Complete(length) => {
                complete = Some(complete.map_or(length, |current| current.max(length)));
            }
            WindowsConptyShutdownResetMatch::Prefix => longer_prefix = true,
            WindowsConptyShutdownResetMatch::NoMatch => {}
        }

        match complete {
            Some(_) if longer_prefix && !at_eof => WindowsConptyShutdownResetMatch::Prefix,
            Some(length) => WindowsConptyShutdownResetMatch::Complete(length),
            None if longer_prefix => WindowsConptyShutdownResetMatch::Prefix,
            None => WindowsConptyShutdownResetMatch::NoMatch,
        }
    }

    fn starts_with_candidate(bytes: &[u8]) -> bool {
        bytes.starts_with(Self::START)
            || bytes.starts_with(Self::BARE_SIMPLE_SEQUENCE)
            || bytes.starts_with(Self::LF_PREFIXED_SIMPLE_SEQUENCE)
    }

    fn first_candidate_start(bytes: &[u8]) -> Option<usize> {
        [
            Self::START,
            Self::BARE_SIMPLE_SEQUENCE,
            Self::LF_PREFIXED_SIMPLE_SEQUENCE,
        ]
        .into_iter()
        .filter_map(|prefix| {
            bytes
                .windows(prefix.len())
                .position(|window| window == prefix)
        })
        .min()
    }

    fn match_titled_sequence(bytes: &[u8]) -> WindowsConptyShutdownResetMatch {
        if Self::TITLED_PREFIX.starts_with(bytes) {
            return WindowsConptyShutdownResetMatch::Prefix;
        }
        let Some(title_and_suffix) = bytes.strip_prefix(Self::TITLED_PREFIX) else {
            return WindowsConptyShutdownResetMatch::NoMatch;
        };
        let Some(title_end) = title_and_suffix.iter().position(|byte| *byte == b'\x07') else {
            return if title_and_suffix.len() <= Self::MAX_TITLE_BYTES {
                WindowsConptyShutdownResetMatch::Prefix
            } else {
                WindowsConptyShutdownResetMatch::NoMatch
            };
        };
        if title_end > Self::MAX_TITLE_BYTES {
            return WindowsConptyShutdownResetMatch::NoMatch;
        }
        let suffix = &title_and_suffix[title_end..];
        if suffix.starts_with(Self::TITLED_SUFFIX) {
            return WindowsConptyShutdownResetMatch::Complete(
                Self::TITLED_PREFIX.len() + title_end + Self::TITLED_SUFFIX.len(),
            );
        }
        if Self::TITLED_SUFFIX.starts_with(suffix) {
            return WindowsConptyShutdownResetMatch::Prefix;
        }
        WindowsConptyShutdownResetMatch::NoMatch
    }
}

#[cfg(any(test, target_os = "windows"))]
fn fixed_shutdown_reset_match(bytes: &[u8], candidate: &[u8]) -> WindowsConptyShutdownResetMatch {
    if bytes.starts_with(candidate) {
        WindowsConptyShutdownResetMatch::Complete(candidate.len())
    } else if candidate.starts_with(bytes) {
        WindowsConptyShutdownResetMatch::Prefix
    } else {
        WindowsConptyShutdownResetMatch::NoMatch
    }
}

#[cfg(any(test, target_os = "windows"))]
fn longest_suffix_matching_prefix(bytes: &[u8], prefix: &[u8]) -> usize {
    let max = bytes.len().min(prefix.len().saturating_sub(1));
    (1..=max)
        .rev()
        .find(|length| bytes[bytes.len() - length..] == prefix[..*length])
        .unwrap_or(0)
}

#[cfg(target_family = "unix")]
const WORKER_MEM_GUARDRAIL_RATIO: f64 = 0.75;
#[cfg(target_family = "unix")]
const WORKER_MEM_GUARDRAIL_ACTIVE_INTERVAL: Duration = Duration::from_secs(10);
#[cfg(target_family = "unix")]
const WORKER_MEM_GUARDRAIL_IDLE_INTERVAL: Duration = Duration::from_secs(60);

const WORKER_READY_TIMEOUT: Duration = Duration::from_secs(10);
const WORKER_RESTART_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(500);
const WORKER_SESSION_END_RESPAWN_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(target_family = "windows")]
const WINDOWS_INTERRUPT_STDIN_TIMEOUT: Duration = Duration::from_secs(1);
#[cfg(target_family = "windows")]
pub(crate) const WINDOWS_IPC_CONNECT_MAX_WAIT: Duration = Duration::from_secs(10);
pub(crate) const OUTPUT_READER_QUIESCE_GRACE: Duration = Duration::from_millis(120);
#[cfg(target_family = "unix")]
const OUTPUT_READER_STOP_DRAIN_GRACE: Duration = Duration::from_millis(50);

pub(crate) enum InitialWorkerPrompt {
    Immediate(String),
    Waited(String),
}

pub(crate) struct SupervisorSpawn {
    pub(crate) process: WorkerProcess,
    pub(crate) initial_prompt: Option<InitialWorkerPrompt>,
}

pub(crate) struct WorkerSupervisor;

impl WorkerSupervisor {
    pub(crate) fn spawn(
        worker_launch: WorkerLaunch,
        exe_path: &Path,
        backend: Backend,
        sandbox_state: &SandboxState,
        context: WorkerSpawnContext<'_>,
    ) -> Result<SupervisorSpawn, WorkerError> {
        // Start each worker with a clean server-owned session temp dir. The
        // current implementation reuses the same configured path across
        // respawns and wipes/recreates it in place before launch.
        crate::sandbox::prepare_session_temp_dir(&sandbox_state.session_temp_dir)
            .map_err(|err| WorkerError::Sandbox(err.to_string()))?;
        #[cfg(target_os = "windows")]
        if let Some(prepared_windows_launch) = context.prepared_windows_launch.as_ref() {
            crate::windows_sandbox::refresh_prepared_sandbox_launch_acl_state(
                prepared_windows_launch,
            )
            .map_err(WorkerError::Sandbox)?;
        }
        crate::event_log::log_lazy("worker_spawn_begin", || {
            worker_context_event_payload(&worker_launch, backend, sandbox_state)
        });
        let process = WorkerProcess::spawn(worker_launch, exe_path, sandbox_state, context)?;
        let ipc = process
            .ipc_connection()
            .ok_or_else(|| WorkerError::Protocol("worker ipc unavailable".to_string()))?;
        if let Err(err) = wait_for_worker_ready(ipc, WORKER_READY_TIMEOUT) {
            return Err(Self::terminate_spawn_error(process, backend, err));
        }
        let initial_prompt = match seed_initial_readiness_from_process(&process) {
            Ok(prompt) => prompt,
            Err(err) => return Err(Self::terminate_spawn_error(process, backend, err)),
        };
        Ok(SupervisorSpawn {
            process,
            initial_prompt,
        })
    }

    fn terminate_spawn_error(
        process: WorkerProcess,
        backend: Backend,
        err: WorkerError,
    ) -> WorkerError {
        let _ = process.kill();
        crate::event_log::log(
            "worker_spawn_error",
            serde_json::json!({
                "error": err.to_string(),
                "backend": format!("{:?}", backend),
            }),
        );
        err
    }
}

fn wait_for_worker_ready(ipc: ServerIpcConnection, timeout: Duration) -> Result<u32, WorkerError> {
    match ipc.wait_for_worker_ready(timeout) {
        Ok(WorkerToServerIpcMessage::WorkerReady { protocol, .. }) => {
            if protocol.name != "mcp-repl-worker"
                || protocol.version != crate::ipc::WORKER_PROTOCOL_VERSION
            {
                return Err(WorkerError::Protocol(format!(
                    "unsupported worker protocol {} version {}",
                    protocol.name, protocol.version
                )));
            }
            Ok(protocol.version)
        }
        Ok(_) => Err(WorkerError::Protocol(
            "expected worker_ready before user input".to_string(),
        )),
        Err(IpcWaitError::Timeout) => Err(WorkerError::Protocol(
            "timed out waiting for worker_ready".to_string(),
        )),
        Err(IpcWaitError::Disconnected) => Err(WorkerError::Protocol(
            "ipc disconnected while waiting for worker_ready".to_string(),
        )),
        Err(IpcWaitError::SessionEnd) => Err(WorkerError::Protocol(
            "worker session ended before worker_ready".to_string(),
        )),
        Err(IpcWaitError::Protocol(message)) => Err(WorkerError::Protocol(message)),
    }
}

fn seed_initial_readiness_from_process(
    process: &WorkerProcess,
) -> Result<Option<InitialWorkerPrompt>, WorkerError> {
    let Some(ipc) = process.ipc_connection() else {
        return Ok(None);
    };
    if let Some(raw_prompt) = ipc.try_take_prompt() {
        return Ok(Some(InitialWorkerPrompt::Immediate(raw_prompt)));
    }
    match ipc.wait_for_input_readiness(WORKER_READY_TIMEOUT) {
        Ok(IpcInputReadiness::InputWait(prompt)) => Ok(Some(InitialWorkerPrompt::Waited(prompt))),
        Ok(IpcInputReadiness::Ready) => Ok(None),
        Err(IpcWaitError::Protocol(message)) => Err(WorkerError::Protocol(message)),
        Err(IpcWaitError::Timeout) => Ok(None),
        Err(IpcWaitError::SessionEnd) => Err(WorkerError::Protocol(
            "worker session ended before startup readiness".to_string(),
        )),
        Err(IpcWaitError::Disconnected) => Err(WorkerError::Protocol(
            "ipc disconnected while waiting for worker startup readiness".to_string(),
        )),
    }
}

pub(crate) struct WorkerProcess {
    child: WorkerChild,
    stdin_tx: mpsc::Sender<StdinCommand>,
    #[cfg(target_family = "windows")]
    windows_interrupt_transport: WorkerStdinTransport,
    #[cfg(target_family = "windows")]
    windows_interrupt_delivery_started_at: Option<Instant>,
    shutdown_stdin_policy: ShutdownStdinPolicy,
    session_tmpdir: Option<PathBuf>,
    ipc: IpcHandle,
    live_output: LiveOutputCapture,
    stdout_reader: Option<OutputReader>,
    stderr_reader: Option<OutputReader>,
    expected_exit: bool,
    exit_status: Option<std::process::ExitStatus>,
    finalized: bool,
    #[cfg(target_family = "unix")]
    guardrail_stop: Arc<AtomicBool>,
    #[cfg(target_family = "unix")]
    guardrail_thread: Option<std::thread::JoinHandle<()>>,
    #[cfg(target_family = "unix")]
    guardrail_thread_handle: Option<std::thread::Thread>,
    #[cfg(target_os = "linux")]
    linux_bwrap_sandboxed: bool,
    #[cfg(target_os = "macos")]
    denial_logger: Option<crate::sandbox::DenialLogger>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShutdownStdinPolicy {
    CloseBefore,
    CloseAfter,
    #[cfg(target_family = "windows")]
    WindowsConsoleEof,
}

impl ShutdownStdinPolicy {
    fn for_worker_launch(worker_launch: &WorkerLaunch) -> Self {
        match worker_launch {
            WorkerLaunch::Builtin(Backend::Python) => Self::CloseAfter,
            #[cfg(target_family = "windows")]
            WorkerLaunch::Builtin(Backend::R)
                if matches!(worker_launch.stdin_transport(), WorkerStdinTransport::Pty) =>
            {
                Self::WindowsConsoleEof
            }
            WorkerLaunch::Builtin(Backend::R) | WorkerLaunch::Custom(_) => Self::CloseBefore,
        }
    }
}

enum StdinCommand {
    Write {
        payload: Vec<u8>,
        reply: mpsc::Sender<Result<(), WorkerError>>,
    },
    Close {
        reply: mpsc::Sender<Result<(), WorkerError>>,
    },
}

fn send_stdin_command(
    stdin_tx: &mpsc::Sender<StdinCommand>,
    payload: Option<Vec<u8>>,
    timeout: Duration,
) -> Result<(), WorkerError> {
    let (reply_tx, reply_rx) = mpsc::channel();
    let command = match payload {
        Some(payload) => StdinCommand::Write {
            payload,
            reply: reply_tx,
        },
        None => StdinCommand::Close { reply: reply_tx },
    };
    stdin_tx
        .send(command)
        .map_err(|_| WorkerError::Protocol("worker stdin unavailable".to_string()))?;
    if timeout.is_zero() {
        return Err(WorkerError::Timeout(timeout));
    }
    match reply_rx.recv_timeout(timeout) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) => Err(err),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(WorkerError::Timeout(timeout)),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(WorkerError::Protocol(
            "worker stdin thread exited unexpectedly".to_string(),
        )),
    }
}

#[cfg(target_family = "windows")]
fn validate_windows_interrupt_transport(
    stdin_transport: WorkerStdinTransport,
) -> Result<(), WorkerError> {
    match stdin_transport {
        WorkerStdinTransport::Pty => Ok(()),
        WorkerStdinTransport::Pipe => Err(WorkerError::Protocol(
            "Windows interrupt delivery requires ConPTY stdin; custom workers configured with pipe stdin cannot be interrupted"
                .to_string(),
        )),
    }
}

#[cfg(target_family = "windows")]
fn windows_interrupt_payload(
    stdin_transport: WorkerStdinTransport,
) -> Result<Vec<u8>, WorkerError> {
    validate_windows_interrupt_transport(stdin_transport)?;
    Ok(vec![0x03])
}

struct SpawnedWorker {
    child: WorkerChild,
    stdin_tx: mpsc::Sender<StdinCommand>,
    session_tmpdir: Option<PathBuf>,
    stdout_reader: Option<OutputReader>,
    stderr_reader: Option<OutputReader>,
    #[cfg(target_os = "macos")]
    denial_logger: Option<crate::sandbox::DenialLogger>,
}

struct SpawnedWorkerStdio {
    stdin_tx: mpsc::Sender<StdinCommand>,
    stdout_reader: Option<OutputReader>,
    stderr_reader: Option<OutputReader>,
}

struct SpawnedCommand {
    child: WorkerChild,
    #[cfg(any(target_family = "unix", target_family = "windows"))]
    pty_stdio: Option<SpawnedPtyStdio>,
}

#[cfg(any(target_family = "unix", target_family = "windows"))]
struct SpawnedPtyStdio {
    reader: File,
    writer: Box<dyn Write + Send>,
}

enum WorkerChild {
    Standard(Child),
    #[cfg(target_family = "windows")]
    DirectWindows(WindowsProcess),
}

impl WorkerChild {
    fn standard(child: Child) -> Self {
        Self::Standard(child)
    }

    #[cfg(target_family = "unix")]
    fn id(&self) -> u32 {
        match self {
            Self::Standard(child) => child.id(),
        }
    }

    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        match self {
            Self::Standard(child) => child.try_wait(),
            #[cfg(target_family = "windows")]
            Self::DirectWindows(child) => child.try_wait(),
        }
    }

    fn wait(&mut self) -> std::io::Result<ExitStatus> {
        match self {
            Self::Standard(child) => child.wait(),
            #[cfg(target_family = "windows")]
            Self::DirectWindows(child) => child.wait(),
        }
    }

    #[cfg(not(target_family = "unix"))]
    fn kill(&mut self) -> std::io::Result<()> {
        match self {
            Self::Standard(child) => child.kill(),
            #[cfg(target_family = "windows")]
            Self::DirectWindows(child) => child.kill(),
        }
    }

    #[cfg(target_os = "macos")]
    fn standard_child(&self) -> &Child {
        match self {
            Self::Standard(child) => child,
        }
    }

    #[cfg(target_family = "windows")]
    fn close_job(&mut self) {
        match self {
            Self::Standard(_) => {}
            Self::DirectWindows(child) => child.close_job(),
        }
    }

    #[cfg(target_family = "windows")]
    fn close_conpty(&mut self) {
        match self {
            Self::Standard(_) => {}
            Self::DirectWindows(child) => child.close_conpty(),
        }
    }
}

#[cfg(target_family = "windows")]
struct WindowsProcess {
    process: HANDLE,
    thread: HANDLE,
    job: Option<crate::windows_conpty::JobHandle>,
    _conpty: Option<crate::windows_conpty::Conpty>,
}

#[cfg(target_family = "windows")]
unsafe impl Send for WindowsProcess {}

#[cfg(target_family = "windows")]
impl WindowsProcess {
    unsafe fn from_process_information(
        proc_info: PROCESS_INFORMATION,
        conpty: Option<crate::windows_conpty::Conpty>,
        job: Option<crate::windows_conpty::JobHandle>,
    ) -> Self {
        Self {
            process: proc_info.hProcess,
            thread: proc_info.hThread,
            job,
            _conpty: conpty,
        }
    }

    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        let wait = unsafe { WaitForSingleObject(self.process, 0) };
        if wait == WAIT_TIMEOUT {
            return Ok(None);
        }
        if wait == WAIT_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        self.exit_status().map(Some)
    }

    fn wait(&mut self) -> std::io::Result<ExitStatus> {
        let wait = unsafe { WaitForSingleObject(self.process, u32::MAX) };
        if wait == WAIT_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        self.exit_status()
    }

    fn kill(&mut self) -> std::io::Result<()> {
        if let Some(job) = self.job.take() {
            drop(job);
            return Ok(());
        }
        if unsafe { TerminateProcess(self.process, 1) } == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn close_job(&mut self) {
        self.job.take();
    }

    fn close_conpty(&mut self) {
        drop(self._conpty.take());
    }

    fn exit_status(&self) -> std::io::Result<ExitStatus> {
        let mut exit_code = 0u32;
        if unsafe { GetExitCodeProcess(self.process, &mut exit_code) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(ExitStatus::from_raw(exit_code))
    }
}

#[cfg(target_family = "windows")]
impl Drop for WindowsProcess {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.thread);
            CloseHandle(self.process);
        }
    }
}

#[derive(Clone)]
pub(crate) struct WorkerSpawnContext<'a> {
    pub(crate) oversized_output: OversizedOutputMode,
    pub(crate) output_timeline: OutputTimeline,
    pub(crate) guardrail: GuardrailShared,
    pub(crate) managed_network_proxy: Option<&'a crate::managed_network::ManagedNetworkProxy>,
    #[cfg(target_os = "windows")]
    pub(crate) prepared_windows_launch: Option<crate::windows_sandbox::PreparedSandboxLaunch>,
}

struct OutputReader {
    handle: std::thread::JoinHandle<()>,
    done_rx: mpsc::Receiver<()>,
    stop_requested: Arc<AtomicBool>,
    #[cfg(target_family = "unix")]
    wake_writer: std::io::PipeWriter,
}

impl OutputReader {
    fn stop_and_join(mut self, panic_message: &'static str) -> Result<(), WorkerError> {
        if matches!(
            self.done_rx.recv_timeout(OUTPUT_READER_QUIESCE_GRACE),
            Err(mpsc::RecvTimeoutError::Timeout)
        ) {
            self.request_stop();
            let _ = self.done_rx.recv();
        }
        self.handle
            .join()
            .map_err(|_| WorkerError::Protocol(panic_message.to_string()))
    }

    fn request_stop(&mut self) {
        self.stop_requested.store(true, Ordering::Relaxed);
        #[cfg(target_family = "unix")]
        {
            let _ = self.wake_writer.write_all(&[0]);
            let _ = self.wake_writer.flush();
        }
    }
}

impl WorkerProcess {
    fn spawn(
        worker_launch: WorkerLaunch,
        exe_path: &Path,
        sandbox_state: &SandboxState,
        context: WorkerSpawnContext<'_>,
    ) -> Result<Self, WorkerError> {
        let shutdown_stdin_policy = ShutdownStdinPolicy::for_worker_launch(&worker_launch);
        #[cfg(target_family = "windows")]
        let windows_interrupt_transport = worker_launch.stdin_transport();
        let WorkerSpawnContext {
            oversized_output,
            output_timeline,
            guardrail,
            managed_network_proxy,
            #[cfg(target_os = "windows")]
            prepared_windows_launch,
        } = context;

        #[cfg(not(target_family = "unix"))]
        let _ = &guardrail;

        #[cfg(target_family = "windows")]
        let mut ipc_server = {
            if let Some(launch) = prepared_windows_launch.as_ref() {
                let mut allowed_sids = vec![launch.capability_sid()];
                if let Some(offline_sid) = launch.offline_user_sid() {
                    allowed_sids.push(offline_sid);
                }
                IpcServer::bind_with_allowed_sids(&allowed_sids)
            } else {
                IpcServer::bind()
            }
        }
        .map_err(WorkerError::Io)?;
        #[cfg(not(target_family = "windows"))]
        let mut ipc_server = IpcServer::bind().map_err(WorkerError::Io)?;
        let live_output = LiveOutputCapture::new(oversized_output, output_timeline.clone());
        #[cfg(target_os = "windows")]
        let live_output = if matches!(&worker_launch, WorkerLaunch::Builtin(_)) {
            live_output.with_windows_conpty_startup_noise_filter()
        } else {
            live_output
        };
        let SpawnedWorker {
            child,
            stdin_tx,
            session_tmpdir,
            stdout_reader,
            stderr_reader,
            #[cfg(target_os = "macos")]
            denial_logger,
        } = match &worker_launch {
            WorkerLaunch::Builtin(Backend::R) => Self::spawn_embedded_worker(
                Backend::R,
                exe_path,
                sandbox_state,
                managed_network_proxy,
                live_output.clone(),
                &mut ipc_server,
                #[cfg(target_os = "windows")]
                prepared_windows_launch.as_ref(),
            )?,
            WorkerLaunch::Builtin(Backend::Python) => Self::spawn_embedded_worker(
                Backend::Python,
                exe_path,
                sandbox_state,
                managed_network_proxy,
                live_output.clone(),
                &mut ipc_server,
                #[cfg(target_os = "windows")]
                prepared_windows_launch.as_ref(),
            )?,
            WorkerLaunch::Custom(spec) => Self::spawn_custom_worker(
                spec,
                sandbox_state,
                managed_network_proxy,
                live_output.clone(),
                &mut ipc_server,
                #[cfg(target_os = "windows")]
                prepared_windows_launch.as_ref(),
            )?,
        };
        #[allow(unused_mut)]
        let mut child = child;

        let ipc = IpcHandle::new();
        #[cfg(any(target_family = "unix", target_family = "windows"))]
        {
            let output_capture = live_output.clone();
            let image_capture = live_output.clone();
            let sideband_capture = live_output.clone();
            let handlers = IpcHandlers {
                on_output_text: Some(Arc::new(move |text| {
                    output_capture.append_output_text(
                        &text.bytes,
                        text.stream,
                        text.is_continuation,
                    );
                })),
                on_output_image: Some(Arc::new(move |image: IpcOutputImage| {
                    image_capture.append_image(image);
                })),
                on_input_wait: Some(Arc::new(move |prompt: String| {
                    sideband_capture.append_sideband(PendingSidebandKind::InputWait { prompt });
                })),
                on_input_line: {
                    let sideband_capture = live_output.clone();
                    Some(Arc::new(move |event: IpcInputLineEvent| {
                        sideband_capture.append_sideband(PendingSidebandKind::ReadlineResult {
                            prompt: event.prompt,
                            line: event.line,
                        });
                    }))
                },
                on_session_end: {
                    let sideband_capture = live_output.clone();
                    Some(Arc::new(move || {
                        sideband_capture.append_sideband(PendingSidebandKind::SessionEnd);
                    }))
                },
            };
            #[cfg(target_family = "unix")]
            ipc_server
                .connect(ipc.clone(), handlers)
                .map_err(WorkerError::Io)?;
            #[cfg(target_family = "windows")]
            handle_windows_ipc_connect_result(
                ipc_server.connect(
                    ipc.clone(),
                    handlers,
                    || child.try_wait().map(|status| status.is_some()),
                    WINDOWS_IPC_CONNECT_MAX_WAIT,
                ),
                &mut child,
            )?;
        }

        #[cfg(target_family = "unix")]
        let (guardrail_stop, guardrail_thread, guardrail_thread_handle) =
            start_memory_guardrail(child.id(), guardrail.clone());
        #[cfg(target_os = "linux")]
        let linux_bwrap_sandboxed = sandbox_state.use_linux_sandbox_bwrap
            && sandbox_state.sandbox_policy.requires_sandbox();

        Ok(Self {
            child,
            stdin_tx,
            #[cfg(target_family = "windows")]
            windows_interrupt_transport,
            #[cfg(target_family = "windows")]
            windows_interrupt_delivery_started_at: None,
            shutdown_stdin_policy,
            session_tmpdir,
            ipc,
            live_output,
            stdout_reader,
            stderr_reader,
            expected_exit: false,
            exit_status: None,
            finalized: false,
            #[cfg(target_family = "unix")]
            guardrail_stop,
            #[cfg(target_family = "unix")]
            guardrail_thread: Some(guardrail_thread),
            #[cfg(target_family = "unix")]
            guardrail_thread_handle: Some(guardrail_thread_handle),
            #[cfg(target_os = "linux")]
            linux_bwrap_sandboxed,
            #[cfg(target_os = "macos")]
            denial_logger,
        })
    }

    fn spawn_embedded_worker(
        backend: Backend,
        exe_path: &Path,
        sandbox_state: &SandboxState,
        managed_network_proxy: Option<&crate::managed_network::ManagedNetworkProxy>,
        live_output: LiveOutputCapture,
        ipc_server: &mut IpcServer,
        #[cfg(target_os = "windows")] prepared_windows_launch: Option<
            &crate::windows_sandbox::PreparedSandboxLaunch,
        >,
    ) -> Result<SpawnedWorker, WorkerError> {
        let prepared = prepare_worker_command_with_managed_network(
            exe_path,
            vec![WORKER_MODE_ARG.to_string()],
            sandbox_state,
            managed_network_proxy,
        )
        .map_err(|err| WorkerError::Sandbox(err.to_string()))?;
        #[cfg(target_os = "windows")]
        let mut prepared = prepared;
        #[cfg(target_os = "windows")]
        if let Some(prepared_windows_launch) = prepared_windows_launch {
            crate::sandbox::append_windows_prepared_capability_sid(
                &mut prepared.args,
                prepared_windows_launch.capability_sid(),
            )
            .map_err(WorkerError::Sandbox)?;
        }
        let session_tmpdir = prepared
            .env
            .get(R_SESSION_TMPDIR_ENV)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);

        let mut command = Command::new(&prepared.program);
        if let Some(arg0) = &prepared.arg0 {
            set_command_arg0(&mut command, arg0);
        }
        command.args(&prepared.args);
        command.envs(prepared.env.iter());
        command.env(
            crate::backend::INTERPRETER_ENV,
            match backend {
                Backend::R => "r",
                Backend::Python => "python",
            },
        );
        if matches!(backend, Backend::Python)
            && let Some(python_executable) =
                std::env::var_os(crate::python_runtime::PYTHON_EXECUTABLE_ENV)
        {
            command.env(
                crate::python_runtime::PYTHON_EXECUTABLE_ENV,
                python_executable,
            );
        }
        #[cfg(target_family = "unix")]
        let client_fds = ipc_server.take_child_fds().ok_or_else(|| {
            WorkerError::Protocol("IPC pipe setup failed; no client fds available".to_string())
        })?;
        #[cfg(target_family = "unix")]
        {
            command.env(IPC_READ_FD_ENV, client_fds.read_fd.to_string());
            command.env(IPC_WRITE_FD_ENV, client_fds.write_fd.to_string());
        }
        #[cfg(target_family = "windows")]
        let (pipe_to_worker, pipe_from_worker) = ipc_server.take_pipe_names().ok_or_else(|| {
            WorkerError::Protocol("IPC pipe setup failed; missing pipe names".to_string())
        })?;
        #[cfg(target_family = "windows")]
        {
            command.env(IPC_PIPE_TO_WORKER_ENV, pipe_to_worker);
            command.env(IPC_PIPE_FROM_WORKER_ENV, pipe_from_worker);
            command.creation_flags(CREATE_NEW_PROCESS_GROUP);
        }
        apply_debug_startup_env(&mut command, session_tmpdir.as_ref());
        let stdin_transport = WorkerLaunch::Builtin(backend).stdin_transport();
        #[cfg(target_os = "windows")]
        let spawn_stdin_transport =
            windows_spawn_transport(&mut command, &prepared.args, stdin_transport);
        #[cfg(target_family = "unix")]
        configure_command_process_group(&mut command, stdin_transport);
        #[cfg(not(target_os = "windows"))]
        let spawn_stdin_transport = stdin_transport;
        let child_result = spawn_command_with_transport(
            &mut command,
            spawn_stdin_transport,
            !matches!(backend, Backend::Python),
        );
        #[cfg(target_family = "unix")]
        {
            unsafe {
                libc::close(client_fds.read_fd);
                libc::close(client_fds.write_fd);
            }
        }
        let SpawnedCommand {
            mut child,
            #[cfg(any(target_family = "unix", target_family = "windows"))]
            pty_stdio,
        } = child_result?;
        if let Some(status) = child.try_wait()? {
            maybe_report_sandbox_exec_failure(&prepared.program, status)?;
            return Err(WorkerError::Protocol(format!(
                "worker process exited immediately with status {status}"
            )));
        }

        let SpawnedWorkerStdio {
            stdin_tx,
            stdout_reader,
            stderr_reader,
        } = attach_spawned_worker_stdio(
            &mut child,
            spawn_stdin_transport,
            #[cfg(any(target_family = "unix", target_family = "windows"))]
            pty_stdio,
            live_output.clone(),
        )?;

        #[cfg(target_os = "macos")]
        let mut denial_logger = prepared.denial_logger;
        #[cfg(target_os = "macos")]
        if let Some(logger) = denial_logger.as_mut() {
            logger.on_child_spawn(child.standard_child());
        }

        Ok(SpawnedWorker {
            child,
            stdin_tx,
            session_tmpdir,
            stdout_reader,
            stderr_reader,
            #[cfg(target_os = "macos")]
            denial_logger,
        })
    }

    fn spawn_custom_worker(
        spec: &CustomWorkerSpec,
        sandbox_state: &SandboxState,
        managed_network_proxy: Option<&crate::managed_network::ManagedNetworkProxy>,
        live_output: LiveOutputCapture,
        ipc_server: &mut IpcServer,
        #[cfg(target_os = "windows")] prepared_windows_launch: Option<
            &crate::windows_sandbox::PreparedSandboxLaunch,
        >,
    ) -> Result<SpawnedWorker, WorkerError> {
        let prepared = prepare_worker_command_with_managed_network(
            &spec.executable,
            spec.args.clone(),
            sandbox_state,
            managed_network_proxy,
        )
        .map_err(|err| WorkerError::Sandbox(err.to_string()))?;
        #[cfg(target_os = "windows")]
        let mut prepared = prepared;
        #[cfg(target_os = "windows")]
        if let Some(prepared_windows_launch) = prepared_windows_launch {
            crate::sandbox::append_windows_prepared_capability_sid(
                &mut prepared.args,
                prepared_windows_launch.capability_sid(),
            )
            .map_err(WorkerError::Sandbox)?;
        }
        let session_tmpdir = prepared
            .env
            .get(R_SESSION_TMPDIR_ENV)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);

        let mut command = Command::new(&prepared.program);
        if let Some(arg0) = &prepared.arg0 {
            set_command_arg0(&mut command, arg0);
        }
        command.args(&prepared.args);
        command.envs(spec.env.iter());
        command.envs(prepared.env.iter());
        match &spec.working_dir {
            CustomWorkerWorkingDir::Policy(CustomWorkerWorkingDirPolicy::Inherit) => {}
            CustomWorkerWorkingDir::Path { path } => {
                command.current_dir(path);
            }
        }
        #[cfg(target_family = "unix")]
        let client_fds = ipc_server.take_child_fds().ok_or_else(|| {
            WorkerError::Protocol("IPC pipe setup failed; no client fds available".to_string())
        })?;
        #[cfg(target_family = "unix")]
        {
            command.env(IPC_READ_FD_ENV, client_fds.read_fd.to_string());
            command.env(IPC_WRITE_FD_ENV, client_fds.write_fd.to_string());
        }
        #[cfg(target_family = "windows")]
        let (pipe_to_worker, pipe_from_worker) = ipc_server.take_pipe_names().ok_or_else(|| {
            WorkerError::Protocol("IPC pipe setup failed; missing pipe names".to_string())
        })?;
        #[cfg(target_family = "windows")]
        {
            command.env(IPC_PIPE_TO_WORKER_ENV, pipe_to_worker);
            command.env(IPC_PIPE_FROM_WORKER_ENV, pipe_from_worker);
            command.creation_flags(CREATE_NEW_PROCESS_GROUP);
        }
        apply_debug_startup_env(&mut command, session_tmpdir.as_ref());
        let stdin_transport = spec.stdin.transport();
        #[cfg(target_os = "windows")]
        let spawn_stdin_transport =
            windows_spawn_transport(&mut command, &prepared.args, stdin_transport);
        #[cfg(target_family = "unix")]
        configure_command_process_group(&mut command, stdin_transport);
        #[cfg(not(target_os = "windows"))]
        let spawn_stdin_transport = stdin_transport;
        let child_result = spawn_command_with_transport(&mut command, spawn_stdin_transport, true);
        #[cfg(target_family = "unix")]
        {
            unsafe {
                libc::close(client_fds.read_fd);
                libc::close(client_fds.write_fd);
            }
        }
        let SpawnedCommand {
            mut child,
            #[cfg(any(target_family = "unix", target_family = "windows"))]
            pty_stdio,
        } = child_result?;
        if let Some(status) = child.try_wait()? {
            maybe_report_sandbox_exec_failure(&prepared.program, status)?;
            return Err(WorkerError::Protocol(format!(
                "worker process exited immediately with status {status}"
            )));
        }

        let SpawnedWorkerStdio {
            stdin_tx,
            stdout_reader,
            stderr_reader,
        } = attach_spawned_worker_stdio(
            &mut child,
            spawn_stdin_transport,
            #[cfg(any(target_family = "unix", target_family = "windows"))]
            pty_stdio,
            live_output.clone(),
        )?;

        #[cfg(target_os = "macos")]
        let mut denial_logger = prepared.denial_logger;
        #[cfg(target_os = "macos")]
        if let Some(logger) = denial_logger.as_mut() {
            logger.on_child_spawn(child.standard_child());
        }

        Ok(SpawnedWorker {
            child,
            stdin_tx,
            session_tmpdir,
            stdout_reader,
            stderr_reader,
            #[cfg(target_os = "macos")]
            denial_logger,
        })
    }

    pub(crate) fn ipc_connection(&self) -> Option<ServerIpcConnection> {
        self.ipc.get()
    }

    pub(crate) fn note_accepted_input_starting(&self) {
        self.live_output.note_accepted_input_starting();
    }

    fn close_stdin(&mut self, timeout: Duration) -> Result<(), WorkerError> {
        send_stdin_command(&self.stdin_tx, None, timeout)
    }

    fn request_ipc_shutdown(&self) {
        if let Some(ipc) = self.ipc.get() {
            let _ = ipc.send_with_timeout(
                ServerToWorkerIpcMessage::Shutdown {},
                Duration::from_millis(200),
            );
        }
    }

    pub(crate) fn send_interrupt(&mut self) -> Result<(), WorkerError> {
        #[cfg(all(target_family = "unix", not(target_os = "linux")))]
        {
            self.send_signal(libc::SIGINT)
        }
        #[cfg(target_os = "linux")]
        {
            self.send_linux_interrupt()
        }
        #[cfg(target_family = "windows")]
        {
            self.send_windows_ctrl_c()
        }
        #[cfg(not(any(target_family = "unix", target_family = "windows")))]
        {
            Ok(())
        }
    }

    #[cfg(target_family = "windows")]
    pub(crate) fn validate_interrupt_delivery(&self) -> Result<(), WorkerError> {
        validate_windows_interrupt_transport(self.windows_interrupt_transport)
    }

    #[cfg(target_family = "windows")]
    fn send_windows_ctrl_c(&mut self) -> Result<(), WorkerError> {
        if self.child.try_wait()?.is_some() {
            return Ok(());
        }
        let payload = windows_interrupt_payload(self.windows_interrupt_transport)?;
        send_stdin_command(
            &self.stdin_tx,
            Some(payload),
            WINDOWS_INTERRUPT_STDIN_TIMEOUT,
        )
    }

    #[cfg(target_family = "windows")]
    pub(crate) fn interrupt_delivery_started_at(&self) -> Option<Instant> {
        self.windows_interrupt_delivery_started_at
    }

    #[cfg(target_family = "windows")]
    pub(crate) fn note_interrupt_delivery_started(&mut self, started_at: Instant) {
        self.windows_interrupt_delivery_started_at = Some(started_at);
    }

    #[cfg(target_family = "windows")]
    pub(crate) fn clear_interrupt_delivery(&mut self) {
        self.windows_interrupt_delivery_started_at = None;
    }

    #[cfg(target_family = "windows")]
    pub(crate) fn send_r_interrupt(&mut self) -> Result<(), WorkerError> {
        self.send_windows_ctrl_c()
    }

    #[cfg(not(target_family = "windows"))]
    pub(crate) fn send_r_interrupt(&mut self) -> Result<(), WorkerError> {
        self.send_interrupt()
    }

    fn send_sigterm(&mut self) -> Result<(), WorkerError> {
        #[cfg(target_family = "unix")]
        {
            self.send_signal_and_descendants(libc::SIGTERM)
        }
        #[cfg(not(target_family = "unix"))]
        {
            request_soft_termination(&mut self.child)
        }
    }

    fn send_sigkill(&mut self) -> Result<(), WorkerError> {
        #[cfg(target_family = "unix")]
        {
            self.send_signal_and_descendants(libc::SIGKILL)
        }
        #[cfg(not(target_family = "unix"))]
        {
            self.child.kill()?;
            Ok(())
        }
    }

    #[cfg(target_family = "unix")]
    #[cfg(target_os = "linux")]
    fn send_linux_interrupt(&self) -> Result<(), WorkerError> {
        if self.linux_bwrap_sandboxed && self.send_linux_bwrap_interrupt_descendants() {
            return Ok(());
        }
        self.send_signal(libc::SIGINT)
    }

    #[cfg(target_os = "linux")]
    fn send_linux_bwrap_interrupt_descendants(&self) -> bool {
        let root = Pid::from_u32(self.child.id());
        let mut system = System::new();
        system.refresh_processes(ProcessesToUpdate::All, true);
        let mut sent = false;
        for pid in collect_process_tree_pids(&system, root) {
            if pid == root
                || linux_pid_is_thread(pid)
                || linux_pid_exe_basename(pid).is_some_and(|name| name == "bwrap")
            {
                continue;
            }
            let _ = raw_unix_kill(pid.as_u32() as i32, libc::SIGINT);
            sent = true;
        }
        sent
    }

    #[cfg(target_family = "unix")]
    fn send_signal(&self, signal: i32) -> Result<(), WorkerError> {
        let pid = self.child.id() as i32;
        let result = raw_unix_kill(-pid, signal);
        if result == 0 {
            Ok(())
        } else {
            let err = std::io::Error::last_os_error();
            // If the process (group) is already gone, we're done.
            if err.kind() == std::io::ErrorKind::NotFound {
                return Ok(());
            }
            Err(WorkerError::Io(err))
        }
    }

    #[cfg(target_family = "unix")]
    fn send_signal_and_descendants(&self, signal: i32) -> Result<(), WorkerError> {
        let root = Pid::from_u32(self.child.id());
        let mut system = System::new();
        system.refresh_processes(ProcessesToUpdate::All, true);
        let descendants = collect_process_tree_pids(&system, root);
        let result = self.send_signal(signal);
        for pid in descendants {
            let _ = raw_unix_kill(pid.as_u32() as i32, signal);
        }
        result
    }

    #[cfg(target_family = "unix")]
    fn send_signal_descendants_only(&self, signal: i32) -> bool {
        let root = Pid::from_u32(self.child.id());
        let mut system = System::new();
        system.refresh_processes(ProcessesToUpdate::All, true);
        let mut sent = false;
        for pid in collect_process_tree_pids(&system, root) {
            if pid == root {
                continue;
            }
            let _ = raw_unix_kill(pid.as_u32() as i32, signal);
            sent = true;
        }
        sent
    }

    pub(crate) fn note_expected_exit(&mut self) {
        self.expected_exit = true;
    }

    pub(crate) fn exit_status_message(&mut self) -> Result<Option<String>, WorkerError> {
        if self.exit_status.is_none()
            && let Some(status) = self.child.try_wait()?
        {
            self.exit_status = Some(status);
        }
        let Some(status) = self.exit_status.as_ref() else {
            return Ok(None);
        };
        if status.success() {
            return Ok(None);
        }
        Ok(Some(format_exit_status_message(status)))
    }

    pub(crate) fn is_running(&mut self) -> Result<bool, WorkerError> {
        if let Some(status) = self.child.try_wait()? {
            self.exit_status = Some(status);
            let should_log = !status.success() && !self.expected_exit;
            if should_log {
                #[cfg(target_family = "unix")]
                if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&status) {
                    eprintln!("worker exited with signal {signal}");
                } else {
                    eprintln!("worker exited with status {status}");
                }
                #[cfg(not(target_family = "unix"))]
                eprintln!("worker exited with status {status}");
            }
            return Ok(false);
        }
        Ok(true)
    }

    pub(crate) fn shutdown_graceful(mut self, timeout: Duration) -> Result<(), WorkerError> {
        self.live_output.note_windows_conpty_shutdown_starting();
        self.request_ipc_shutdown();
        self.prepare_stdin_for_shutdown_wait();
        self.finish_timed_shutdown(timeout)
    }

    pub(crate) fn shutdown_for_restart(mut self, timeout: Duration) -> Result<(), WorkerError> {
        self.live_output.note_windows_conpty_shutdown_starting();
        self.request_ipc_shutdown();
        self.prepare_stdin_for_shutdown_wait();
        self.finish_timed_shutdown(timeout.min(WORKER_RESTART_SHUTDOWN_TIMEOUT))
    }

    fn prepare_stdin_for_shutdown_wait(&mut self) {
        match self.shutdown_stdin_policy {
            ShutdownStdinPolicy::CloseBefore => {
                let _ = self.close_stdin(Duration::from_millis(200));
            }
            ShutdownStdinPolicy::CloseAfter => {}
            #[cfg(target_family = "windows")]
            ShutdownStdinPolicy::WindowsConsoleEof => {
                // Keep ConPTY alive for output produced after a blocking console
                // read observes EOF. Closing the ConPTY input pipe here tears
                // down the pseudo console before R can publish that output.
                let _ = send_stdin_command(
                    &self.stdin_tx,
                    Some(vec![0x1a, b'\r']),
                    Duration::from_millis(200),
                );
            }
        }
    }

    fn finish_timed_shutdown(mut self, timeout: Duration) -> Result<(), WorkerError> {
        let start = std::time::Instant::now();
        let timeout_deadline = start + timeout;
        let term_deadline = start + shutdown_term_delay(timeout);

        if !timeout.is_zero() {
            // TODO: Replace these try_wait() polling loops with a dedicated waiter thread so
            // teardown can block on a completion signal, then escalate on timeout without spin
            // sleeps.
            loop {
                if let Some(status) = self.child.try_wait()? {
                    self.exit_status = Some(status);
                    break;
                }
                let now = std::time::Instant::now();
                if now >= term_deadline || now >= timeout_deadline {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        }

        if self.child.try_wait()?.is_none() {
            let _ = self.send_sigterm();
            let term_deadline = std::cmp::min(
                timeout_deadline,
                std::time::Instant::now() + Duration::from_secs(2),
            );
            loop {
                if let Some(status) = self.child.try_wait()? {
                    self.exit_status = Some(status);
                    break;
                }
                if std::time::Instant::now() >= term_deadline {
                    let _ = self.send_sigkill();
                    self.exit_status = Some(self.child.wait()?);
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        }

        // Ensure the stdin writer is closed before finalization. Built-in
        // Python reaches this point without an early close because Windows
        // ConPTY can terminate the console worker before sideband shutdown lets
        // an active request emit its final output.
        let _ = self.close_stdin(Duration::from_millis(200));
        self.finalize_terminated_process()
    }

    pub(crate) fn kill(mut self) -> Result<(), WorkerError> {
        let _ = self.send_sigkill();
        self.exit_status = Some(self.child.wait()?);
        self.finalize_terminated_process()
    }

    pub(crate) fn finish_exited(mut self) -> Result<(), WorkerError> {
        if self.exit_status.is_none() {
            self.exit_status = Some(self.child.wait()?);
        }
        self.finalize_terminated_process()
    }

    pub(crate) fn finish_session_end_for_respawn(mut self) -> Result<(), WorkerError> {
        self.live_output
            .append_sideband(PendingSidebandKind::SessionEnd);
        self.disable_ipc_handlers();
        if self.exit_status.is_none() {
            match self.child.try_wait()? {
                Some(status) => self.exit_status = Some(status),
                None => {
                    #[cfg(target_family = "windows")]
                    {
                        // Keep the old ConPTY lifecycle wholly ahead of
                        // respawn. A detached reaper would close ConPTY only
                        // after its raw reader and reset filter were finalized.
                        return self.shutdown_graceful(WORKER_SESSION_END_RESPAWN_SHUTDOWN_TIMEOUT);
                    }
                    #[cfg(not(target_family = "windows"))]
                    {
                        self.quiesce_raw_output_readers()?;
                        // The next spawn resets and reuses this stable session temp path.
                        // The old background reaper must not remove the respawned worker's TMPDIR.
                        self.session_tmpdir = None;
                        let _ = thread::Builder::new()
                            .name("worker-session-end-reaper".to_string())
                            .spawn(move || {
                                let _ = self
                                    .shutdown_graceful(WORKER_SESSION_END_RESPAWN_SHUTDOWN_TIMEOUT);
                            });
                        return Ok(());
                    }
                }
            }
        }
        #[cfg(target_family = "unix")]
        {
            self.send_signal_descendants_only(libc::SIGKILL);
        }
        #[cfg(target_family = "windows")]
        {
            self.child.close_job();
            self.child.close_conpty();
        }
        self.quiesce_raw_output_readers()?;
        self.detach_ipc_reader();
        self.cleanup_session_tmpdir();
        self.report_denials();
        Ok(())
    }

    fn finalize_terminated_process(&mut self) -> Result<(), WorkerError> {
        if self.finalized {
            return Ok(());
        }
        #[cfg(target_family = "unix")]
        {
            // Once the root worker is gone, kill any remaining session peers before waiting on
            // stdio or IPC readers they may still be holding open.
            if self.exit_status.is_some() {
                self.send_signal_descendants_only(libc::SIGKILL);
            } else {
                let _ = self.send_sigkill();
            }
            // TODO: Track descendants or use stronger OS-level containment so children that have
            // escaped the worker process group are still killable after the root exits.
        }
        #[cfg(target_family = "windows")]
        {
            self.child.close_job();
        }
        self.quiesce_output_producers()?;
        self.report_denials();
        self.finalized = true;
        Ok(())
    }

    fn detach_ipc_reader(&mut self) {
        if let Some(ipc) = self.ipc.get() {
            ipc.detach_reader_thread();
        }
    }

    fn disable_ipc_handlers(&mut self) {
        if let Some(ipc) = self.ipc.get() {
            ipc.disable_handlers();
        }
    }

    fn quiesce_raw_output_readers(&mut self) -> Result<(), WorkerError> {
        // Sideband session_end can overtake bytes already queued in the raw
        // ConPTY stream. Give both readers the normal bounded drain grace
        // before forcing them to stop, then finalize the lifecycle filter.
        if let Some(reader) = self.stdout_reader.take() {
            reader.stop_and_join("worker stdout reader thread panicked")?;
        }
        if let Some(reader) = self.stderr_reader.take() {
            reader.stop_and_join("worker stderr reader thread panicked")?;
        }
        self.live_output.finalize_windows_conpty_raw_text();
        Ok(())
    }

    fn quiesce_output_producers(&mut self) -> Result<(), WorkerError> {
        // Keep teardown bounded even if a detached descendant still holds stdio open. A more
        // robust long-term design would pair this with session-scoped output rings or stronger
        // OS-level containment so stale descendants cannot target a future session at all.
        // IPC is stricter than stdout/stderr by contract: only the main worker may own the
        // sideband fds. Backend startup strips the bootstrap env vars, marks the fds
        // close-on-exec, and closes them again in forked children, so EOF should track the root
        // worker lifetime.
        // Close the retained ConPTY while its reader is still active so the
        // final console repaint is captured inside the armed raw-output
        // lifecycle instead of appearing after filter finalization.
        #[cfg(target_family = "windows")]
        self.child.close_conpty();
        if let Some(reader) = self.stdout_reader.take() {
            reader.stop_and_join("worker stdout reader thread panicked")?;
        }
        if let Some(reader) = self.stderr_reader.take() {
            reader.stop_and_join("worker stderr reader thread panicked")?;
        }
        if let Some(ipc) = self.ipc.get() {
            ipc.join_reader_thread().map_err(WorkerError::Io)?;
        }
        self.live_output.finalize_windows_conpty_raw_text();
        Ok(())
    }

    fn cleanup_session_tmpdir(&mut self) {
        let Some(path) = self.session_tmpdir.take() else {
            return;
        };
        if !path.is_absolute() || path.as_path() == std::path::Path::new("/") {
            return;
        }
        cleanup_worker_session_tmpdir(
            &path,
            crate::debug_logs::log_path(crate::diagnostics::WORKER_STARTUP_LOG_FILE_NAME),
        );
    }

    fn terminate_for_drop(&mut self) {
        if self.exit_status.is_some() {
            return;
        }
        match self.child.try_wait() {
            Ok(Some(status)) => {
                self.exit_status = Some(status);
            }
            Ok(None) | Err(_) => {
                let _ = self.send_sigkill();
                if let Ok(status) = self.child.wait() {
                    self.exit_status = Some(status);
                }
            }
        }
    }

    fn stop_guardrail(&mut self) {
        #[cfg(target_family = "unix")]
        {
            self.guardrail_stop.store(true, Ordering::Relaxed);
            if let Some(thread) = self.guardrail_thread_handle.as_ref() {
                thread.unpark();
            }
            if let Some(handle) = self.guardrail_thread.take() {
                let _ = handle.join();
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn report_denials(&mut self) {
        let Some(logger) = self.denial_logger.take() else {
            return;
        };
        let denials = logger.finish();
        if denials.is_empty() {
            return;
        }
        eprintln!("\n=== Sandbox denials ===");
        for crate::sandbox::SandboxDenial { name, capability } in denials {
            eprintln!("({name}) {capability}");
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn report_denials(&mut self) {}

    #[cfg(test)]
    pub(crate) fn new_for_test(child: Child) -> Self {
        let (stdin_tx, _stdin_rx) = mpsc::channel();
        Self {
            child: WorkerChild::standard(child),
            stdin_tx,
            #[cfg(target_family = "windows")]
            windows_interrupt_transport: WorkerStdinTransport::Pipe,
            #[cfg(target_family = "windows")]
            windows_interrupt_delivery_started_at: None,
            shutdown_stdin_policy: ShutdownStdinPolicy::CloseBefore,
            session_tmpdir: None,
            ipc: IpcHandle::new(),
            live_output: LiveOutputCapture::new(
                OversizedOutputMode::Files,
                OutputTimeline::new(Arc::new(crate::output_capture::OutputRing::with_capacity(
                    crate::output_capture::OUTPUT_RING_CAPACITY_BYTES,
                ))),
            ),
            stdout_reader: None,
            stderr_reader: None,
            expected_exit: false,
            exit_status: None,
            finalized: false,
            #[cfg(target_family = "unix")]
            guardrail_stop: Arc::new(AtomicBool::new(false)),
            #[cfg(target_family = "unix")]
            guardrail_thread: None,
            #[cfg(target_family = "unix")]
            guardrail_thread_handle: None,
            #[cfg(target_os = "linux")]
            linux_bwrap_sandboxed: false,
            #[cfg(target_os = "macos")]
            denial_logger: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn set_exit_status_for_test(&mut self, status: std::process::ExitStatus) {
        self.exit_status = Some(status);
    }

    #[cfg(test)]
    pub(crate) fn wait_child_for_test(
        &mut self,
    ) -> Result<std::process::ExitStatus, std::io::Error> {
        self.child.wait()
    }

    #[cfg(all(test, target_family = "unix"))]
    pub(crate) fn set_ipc_for_test(&mut self, ipc: ServerIpcConnection) {
        self.ipc.set(ipc);
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn persist_worker_startup_log(session_tmpdir: &Path, destination: Option<PathBuf>) {
    let Some(destination) = destination else {
        return;
    };
    let source = session_tmpdir.join(crate::diagnostics::WORKER_STARTUP_LOG_FILE_NAME);
    if !source.is_file() || source == destination {
        return;
    }
    if let Err(err) = std::fs::copy(&source, &destination) {
        eprintln!(
            "Failed to persist worker startup log to {}: {err}",
            destination.display()
        );
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn cleanup_worker_session_tmpdir(
    session_tmpdir: &Path,
    worker_log_destination: Option<PathBuf>,
) {
    persist_worker_startup_log(session_tmpdir, worker_log_destination);
    if std::env::var_os("MCP_REPL_KEEP_SESSION_TMPDIR").is_some() {
        return;
    }
    if let Err(err) = std::fs::remove_dir_all(session_tmpdir)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!("Failed to remove worker session temp dir: {err}");
    }
}

impl Drop for WorkerProcess {
    fn drop(&mut self) {
        self.stop_guardrail();
        if !self.finalized {
            self.terminate_for_drop();
            let _ = self.finalize_terminated_process();
        }
        self.cleanup_session_tmpdir();
    }
}

#[cfg(target_family = "unix")]
fn start_memory_guardrail(
    root_pid: u32,
    guardrail: GuardrailShared,
) -> (
    Arc<AtomicBool>,
    std::thread::JoinHandle<()>,
    std::thread::Thread,
) {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let handle = std::thread::spawn(move || {
        let root = Pid::from_u32(root_pid);
        let mut system = System::new();
        let mut last_check = std::time::Instant::now();
        loop {
            if stop_thread.load(Ordering::Relaxed) {
                return;
            }
            let now = std::time::Instant::now();
            let busy = guardrail.busy.load(Ordering::Relaxed);
            let interval = if busy {
                WORKER_MEM_GUARDRAIL_ACTIVE_INTERVAL
            } else {
                WORKER_MEM_GUARDRAIL_IDLE_INTERVAL
            };
            if now.duration_since(last_check) < interval {
                // Use park_timeout + unpark so shutdown doesn't block for up to 1s waiting
                // for this thread to wake from sleep.
                let remaining = interval.saturating_sub(now.duration_since(last_check));
                std::thread::park_timeout(remaining.min(Duration::from_secs(60)));
                continue;
            }
            last_check = now;

            system.refresh_memory();
            system.refresh_processes(ProcessesToUpdate::All, true);

            let total_kb = system.total_memory();
            let limit_kb = (total_kb as f64 * WORKER_MEM_GUARDRAIL_RATIO) as u64;
            let (used_kb, pids) = process_tree_memory_kb(&system, root);
            if used_kb == 0 || total_kb == 0 {
                continue;
            }
            if used_kb < limit_kb {
                continue;
            }

            let used_mb = used_kb / 1024;
            let limit_mb = limit_kb / 1024;
            let total_mb = total_kb / 1024;
            let mut message = format!(
                "[repl] worker killed by memory guardrail: rss={}MB limit={}MB ({}% of host {}MB)\n",
                used_mb,
                limit_mb,
                (WORKER_MEM_GUARDRAIL_RATIO * 100.0).round() as u64,
                total_mb
            );
            if busy {
                message.push_str("[repl] previous request aborted; retry your last input\n");
            } else {
                message.push_str("[repl] worker was idle; new session started\n");
            }

            {
                let mut slot = guardrail
                    .event
                    .lock()
                    .expect("guardrail event mutex poisoned");
                if slot.is_none() {
                    *slot = Some(GuardrailEvent {
                        message: message.clone(),
                        was_busy: busy,
                        is_error: true,
                    });
                }
            }

            // Best-effort: kill process group and then any discovered descendants.
            let _ = unsafe { libc::kill(-(root_pid as i32), libc::SIGKILL) };
            for pid in pids {
                let _ = unsafe { libc::kill(pid.as_u32() as i32, libc::SIGKILL) };
            }

            return;
        }
    });
    let thread = handle.thread().clone();
    (stop, handle, thread)
}

#[cfg(target_family = "unix")]
fn process_tree_memory_kb(system: &System, root: Pid) -> (u64, Vec<Pid>) {
    let pids = collect_process_tree_pids(system, root);
    let mut total_kb: u64 = 0;
    for pid in &pids {
        if let Some(process) = system.process(*pid) {
            total_kb = total_kb.saturating_add(process.memory());
        }
    }
    (total_kb, pids)
}

#[cfg(target_family = "unix")]
fn collect_process_tree_pids(system: &System, root: Pid) -> Vec<Pid> {
    let mut children: HashMap<Pid, Vec<Pid>> = HashMap::new();
    for (proc_pid, process) in system.processes() {
        if let Some(parent) = process.parent() {
            children.entry(parent).or_default().push(*proc_pid);
        }
    }

    let mut stack = vec![root];
    let mut seen: HashSet<Pid> = HashSet::new();
    while let Some(current) = stack.pop() {
        if !seen.insert(current) {
            continue;
        }
        if let Some(kids) = children.get(&current) {
            for child in kids {
                if !seen.contains(child) {
                    stack.push(*child);
                }
            }
        }
    }

    let mut pids = Vec::new();
    for pid in seen {
        if system.process(pid).is_some() {
            pids.push(pid);
        }
    }
    pids
}

#[cfg(target_os = "linux")]
fn linux_pid_is_thread(pid: Pid) -> bool {
    let status_path = format!("/proc/{}/status", pid.as_u32());
    let Ok(status) = std::fs::read_to_string(status_path) else {
        return false;
    };
    for line in status.lines() {
        let Some(raw_tgid) = line.strip_prefix("Tgid:") else {
            continue;
        };
        let Ok(tgid) = raw_tgid.trim().parse::<u32>() else {
            return false;
        };
        return tgid != pid.as_u32();
    }
    false
}

#[cfg(target_os = "linux")]
fn linux_pid_exe_basename(pid: Pid) -> Option<String> {
    let exe = std::fs::read_link(format!("/proc/{}/exe", pid.as_u32())).ok()?;
    exe.file_name()
        .map(|name| name.to_string_lossy().to_string())
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn apply_debug_startup_env(command: &mut Command, session_tmpdir: Option<&PathBuf>) {
    crate::debug_logs::apply_child_env(command);
    if let Some(tmpdir) = session_tmpdir {
        command.env(
            crate::diagnostics::STARTUP_LOG_PATH_ENV,
            tmpdir.join(crate::diagnostics::WORKER_STARTUP_LOG_FILE_NAME),
        );
    }
}

fn maybe_report_sandbox_exec_failure(
    _program: &Path,
    _status: std::process::ExitStatus,
) -> Result<(), WorkerError> {
    #[cfg(target_os = "macos")]
    {
        let is_sandbox_exec = _program
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == "sandbox-exec");
        if is_sandbox_exec && _status.code() == Some(71) {
            return Err(WorkerError::Sandbox(
                "sandbox-exec failed (Operation not permitted). Start mcp-repl with --sandbox danger-full-access to disable sandboxing."
                    .to_string(),
            ));
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(crate) fn linux_sandbox_startup_retryable(err: &WorkerError) -> bool {
    match err {
        WorkerError::Protocol(message) => {
            message.contains("ipc disconnected while waiting for backend info")
                || message.contains("worker session ended before backend info")
                || message.contains("ipc disconnected while waiting for worker_ready")
                || message.contains("worker session ended before worker_ready")
                || message.contains("worker process exited immediately")
        }
        _ => false,
    }
}

#[cfg(target_family = "unix")]
fn spawn_output_reader<R>(
    stream: Option<R>,
    output_stream: TextStream,
    live_output: LiveOutputCapture,
) -> Result<Option<OutputReader>, WorkerError>
where
    R: Read + AsRawFd + Send + 'static,
{
    let Some(mut stream) = stream else {
        return Ok(None);
    };
    let (mut wake_reader, wake_writer) = std::io::pipe()?;
    let (done_tx, done_rx) = mpsc::channel();
    let stop_requested = Arc::new(AtomicBool::new(false));
    let handle = thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        let stream_fd = stream.as_raw_fd();
        let wake_fd = wake_reader.as_raw_fd();
        let mut stop_deadline = None;
        loop {
            let mut fds = [
                libc::pollfd {
                    fd: stream_fd,
                    events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                    revents: 0,
                },
                libc::pollfd {
                    fd: wake_fd,
                    events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                    revents: 0,
                },
            ];
            let timeout_ms = match stop_deadline {
                Some(deadline) => {
                    if std::time::Instant::now() >= deadline {
                        break;
                    }
                    poll_timeout_until(deadline)
                }
                None => -1,
            };
            let ready =
                unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
            if ready < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
            if ready == 0 {
                break;
            }
            if fds[1].revents != 0 {
                let mut wake_buffer = [0u8; 16];
                let _ = wake_reader.read(&mut wake_buffer);
                if stop_deadline.is_none() {
                    stop_deadline =
                        Some(std::time::Instant::now() + OUTPUT_READER_STOP_DRAIN_GRACE);
                }
            }
            let mut read_stream = false;
            if fds[0].revents != 0 {
                match stream.read(&mut buffer) {
                    Ok(0) => {
                        break;
                    }
                    Ok(n) => {
                        live_output.append_raw_text(&buffer[..n], output_stream);
                        read_stream = true;
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            if stop_deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
                break;
            }
            if read_stream {
                continue;
            }
        }
        live_output.finish_raw_text(output_stream);
        let _ = done_tx.send(());
    });
    Ok(Some(OutputReader {
        handle,
        done_rx,
        stop_requested,
        wake_writer,
    }))
}

#[cfg(target_family = "windows")]
fn spawn_output_reader<R>(
    stream: Option<R>,
    output_stream: TextStream,
    live_output: LiveOutputCapture,
) -> Result<Option<OutputReader>, WorkerError>
where
    R: Read + AsRawHandle + Send + 'static,
{
    let Some(mut stream) = stream else {
        return Ok(None);
    };
    let (done_tx, done_rx) = mpsc::channel();
    let stop_requested = Arc::new(AtomicBool::new(false));
    let thread_stop = stop_requested.clone();
    let handle = thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        let stream_handle = stream.as_raw_handle();
        loop {
            if thread_stop.load(Ordering::Relaxed) {
                break;
            }
            let mut available = 0u32;
            let peek_ok = unsafe {
                PeekNamedPipe(
                    stream_handle as _,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &mut available,
                    std::ptr::null_mut(),
                )
            };
            if peek_ok == 0 {
                let err = std::io::Error::last_os_error();
                match err.raw_os_error() {
                    Some(code)
                        if code == ERROR_BROKEN_PIPE as i32 || code == ERROR_HANDLE_EOF as i32 =>
                    {
                        break;
                    }
                    _ => break,
                }
            }
            if available == 0 {
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => live_output.append_raw_text(&buffer[..n], output_stream),
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        live_output.finish_raw_text(output_stream);
        let _ = done_tx.send(());
    });
    Ok(Some(OutputReader {
        handle,
        done_rx,
        stop_requested,
    }))
}

#[cfg(not(any(target_family = "unix", target_family = "windows")))]
fn spawn_output_reader<R>(
    stream: Option<R>,
    output_stream: TextStream,
    live_output: LiveOutputCapture,
) -> Result<Option<OutputReader>, WorkerError>
where
    R: Read + Send + 'static,
{
    let Some(mut stream) = stream else {
        return Ok(None);
    };
    let stop_requested = Arc::new(AtomicBool::new(false));
    let handle = thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => live_output.append_raw_text(&buffer[..n], output_stream),
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        live_output.finish_raw_text(output_stream);
    });
    Ok(Some(OutputReader {
        handle,
        stop_requested,
    }))
}

fn spawn_command_with_transport(
    command: &mut Command,
    stdin_transport: WorkerStdinTransport,
    pty_echo: bool,
) -> Result<SpawnedCommand, WorkerError> {
    match stdin_transport {
        WorkerStdinTransport::Pipe => {
            let child = command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            Ok(SpawnedCommand {
                child: WorkerChild::standard(child),
                #[cfg(any(target_family = "unix", target_family = "windows"))]
                pty_stdio: None,
            })
        }
        WorkerStdinTransport::Pty => spawn_command_with_pty(command, pty_echo),
    }
}

#[cfg(target_os = "windows")]
fn windows_spawn_transport(
    command: &mut Command,
    prepared_args: &[String],
    stdin_transport: WorkerStdinTransport,
) -> WorkerStdinTransport {
    if !matches!(stdin_transport, WorkerStdinTransport::Pty) {
        return stdin_transport;
    }
    if windows_prepared_args_start_sandbox_wrapper(prepared_args) {
        command.env(crate::windows_conpty::WINDOWS_CONPTY_REQUEST_ENV, "1");
        WorkerStdinTransport::Pipe
    } else {
        stdin_transport
    }
}

#[cfg(target_os = "windows")]
fn windows_prepared_args_start_sandbox_wrapper(prepared_args: &[String]) -> bool {
    if prepared_args
        .first()
        .is_some_and(|arg| arg == "--windows-sandbox")
    {
        return true;
    }
    prepared_args
        .first()
        .is_some_and(|arg| arg == "--windows-sandbox-logon-offline")
        && prepared_args
            .get(1)
            .is_some_and(|arg| arg == "--windows-sandbox")
}

#[cfg(target_family = "unix")]
fn spawn_command_with_pty(
    command: &mut Command,
    echo: bool,
) -> Result<SpawnedCommand, WorkerError> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|err| WorkerError::Protocol(format!("failed to open worker PTY: {err}")))?;
    let slave_path = pair
        .master
        .tty_name()
        .ok_or_else(|| WorkerError::Protocol("worker PTY has no slave path".to_string()))?;

    let stdin = open_pty_slave_stdio(&slave_path)?;
    configure_pty_slave_echo(stdin.as_raw_fd(), echo)?;
    let stdout = open_pty_slave_stdio(&slave_path)?;
    let stderr = open_pty_slave_stdio(&slave_path)?;
    command
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            #[allow(clippy::cast_lossless)]
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let writer = pair
        .master
        .take_writer()
        .map_err(|err| WorkerError::Protocol(format!("failed to open worker PTY writer: {err}")))?;
    let master_fd = pair
        .master
        .as_raw_fd()
        .ok_or_else(|| WorkerError::Protocol("worker PTY master has no fd".to_string()))?;
    let reader_fd = unsafe { libc::dup(master_fd) };
    if reader_fd == -1 {
        return Err(WorkerError::Io(std::io::Error::last_os_error()));
    }
    let reader = unsafe { File::from_raw_fd(reader_fd) };
    let child = command.spawn()?;

    Ok(SpawnedCommand {
        child: WorkerChild::standard(child),
        pty_stdio: Some(SpawnedPtyStdio { reader, writer }),
    })
}

#[cfg(any(test, target_family = "windows"))]
fn apply_command_env_overrides_for_windows_conpty(
    env_map: &mut std::collections::HashMap<String, String>,
    command_envs: impl IntoIterator<Item = (String, Option<String>)>,
) {
    for (key, value) in command_envs {
        if let Some(value) = value {
            upsert_windows_conpty_env(env_map, &key, &value);
        } else {
            remove_windows_conpty_env(env_map, &key);
        }
    }
}

#[cfg(target_family = "windows")]
fn upsert_windows_conpty_env(
    env_map: &mut std::collections::HashMap<String, String>,
    key: &str,
    value: &str,
) {
    crate::windows_conpty::upsert_env_case_insensitive(env_map, key, value);
}

#[cfg(all(test, not(target_family = "windows")))]
fn upsert_windows_conpty_env(
    env_map: &mut std::collections::HashMap<String, String>,
    key: &str,
    value: &str,
) {
    remove_windows_conpty_env(env_map, key);
    env_map.insert(key.to_string(), value.to_string());
}

#[cfg(target_family = "windows")]
fn remove_windows_conpty_env(env_map: &mut std::collections::HashMap<String, String>, key: &str) {
    crate::windows_conpty::remove_env_case_insensitive(env_map, key);
}

#[cfg(all(test, not(target_family = "windows")))]
fn remove_windows_conpty_env(env_map: &mut std::collections::HashMap<String, String>, key: &str) {
    let removals: Vec<String> = env_map
        .keys()
        .filter(|existing| existing.eq_ignore_ascii_case(key))
        .cloned()
        .collect();
    for existing in removals {
        env_map.remove(&existing);
    }
}

#[cfg(target_family = "windows")]
fn spawn_command_with_pty(
    command: &mut Command,
    _echo: bool,
) -> Result<SpawnedCommand, WorkerError> {
    let mut command_line = Vec::new();
    command_line.push(command.get_program().to_string_lossy().to_string());
    command_line.extend(
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().to_string()),
    );
    let mut env_map: std::collections::HashMap<String, String> = std::env::vars().collect();
    apply_command_env_overrides_for_windows_conpty(
        &mut env_map,
        command.get_envs().map(|(key, value)| {
            (
                key.to_string_lossy().to_string(),
                value.map(|value| value.to_string_lossy().to_string()),
            )
        }),
    );
    let (proc_info, mut conpty) = unsafe {
        crate::windows_conpty::spawn_conpty_process_direct(
            &command_line,
            command.get_current_dir(),
            &mut env_map,
        )
        .map_err(WorkerError::Protocol)?
    };
    let job = unsafe {
        crate::windows_conpty::JobHandle::kill_on_close()
            .ok()
            .and_then(|job| job.assign_process(proc_info.hProcess).ok().map(|()| job))
    };
    let writer = conpty.take_input_writer().map_err(WorkerError::Protocol)?;
    let reader = conpty.take_output_reader().map_err(WorkerError::Protocol)?;
    let child = unsafe { WindowsProcess::from_process_information(proc_info, Some(conpty), job) };
    Ok(SpawnedCommand {
        child: WorkerChild::DirectWindows(child),
        pty_stdio: Some(SpawnedPtyStdio {
            reader,
            writer: Box::new(writer),
        }),
    })
}

#[cfg(not(any(target_family = "unix", target_family = "windows")))]
fn spawn_command_with_pty(
    _command: &mut Command,
    _echo: bool,
) -> Result<SpawnedCommand, WorkerError> {
    Err(WorkerError::Protocol(
        "PTY worker stdin transport is not supported on this platform".to_string(),
    ))
}

#[cfg(target_family = "unix")]
fn open_pty_slave_stdio(path: &Path) -> Result<File, WorkerError> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(WorkerError::Io)
}

#[cfg(target_family = "unix")]
fn configure_pty_slave_echo(fd: libc::c_int, enabled: bool) -> Result<(), WorkerError> {
    let mut termios = std::mem::MaybeUninit::<libc::termios>::uninit();
    let rc = unsafe { libc::tcgetattr(fd, termios.as_mut_ptr()) };
    if rc != 0 {
        return Err(WorkerError::Io(std::io::Error::last_os_error()));
    }
    let mut termios = unsafe { termios.assume_init() };
    if enabled {
        termios.c_lflag |= libc::ECHO;
    } else {
        termios.c_lflag &= !libc::ECHO;
    }
    let rc = unsafe { libc::tcsetattr(fd, libc::TCSANOW, &termios) };
    if rc != 0 {
        return Err(WorkerError::Io(std::io::Error::last_os_error()));
    }
    Ok(())
}

fn attach_spawned_worker_stdio(
    child: &mut WorkerChild,
    stdin_transport: WorkerStdinTransport,
    #[cfg(any(target_family = "unix", target_family = "windows"))] pty_stdio: Option<
        SpawnedPtyStdio,
    >,
    live_output: LiveOutputCapture,
) -> Result<SpawnedWorkerStdio, WorkerError> {
    match stdin_transport {
        WorkerStdinTransport::Pipe => {
            #[cfg(any(target_family = "unix", target_family = "windows"))]
            let _ = pty_stdio;
            #[cfg(target_family = "windows")]
            let child = match child {
                WorkerChild::Standard(child) => child,
                WorkerChild::DirectWindows(_) => {
                    return Err(WorkerError::Protocol(
                        "pipe worker process does not expose pipe stdio".to_string(),
                    ));
                }
            };
            #[cfg(not(target_family = "windows"))]
            let WorkerChild::Standard(child) = child;
            let stdin = child
                .stdin
                .take()
                .ok_or_else(|| WorkerError::Protocol("worker stdin unavailable".to_string()))?;
            let stdin_tx = spawn_stdin_writer(stdin);
            let stdout_reader =
                spawn_output_reader(child.stdout.take(), TextStream::Stdout, live_output.clone())?;
            let stderr_reader =
                spawn_output_reader(child.stderr.take(), TextStream::Stderr, live_output)?;
            Ok(SpawnedWorkerStdio {
                stdin_tx,
                stdout_reader,
                stderr_reader,
            })
        }
        WorkerStdinTransport::Pty => {
            #[cfg(target_family = "unix")]
            {
                let pty_stdio = pty_stdio.ok_or_else(|| {
                    WorkerError::Protocol("worker PTY stdio unavailable".to_string())
                })?;
                let stdin_tx = spawn_stdin_writer(pty_stdio.writer);
                let stdout_reader =
                    spawn_output_reader(Some(pty_stdio.reader), TextStream::Stdout, live_output)?;
                Ok(SpawnedWorkerStdio {
                    stdin_tx,
                    stdout_reader,
                    stderr_reader: None,
                })
            }
            #[cfg(target_family = "windows")]
            {
                let pty_stdio = pty_stdio.ok_or_else(|| {
                    WorkerError::Protocol("worker ConPTY stdio unavailable".to_string())
                })?;
                let stdin_tx = spawn_stdin_writer(pty_stdio.writer);
                let stdout_reader =
                    spawn_output_reader(Some(pty_stdio.reader), TextStream::Stdout, live_output)?;
                Ok(SpawnedWorkerStdio {
                    stdin_tx,
                    stdout_reader,
                    stderr_reader: None,
                })
            }
            #[cfg(not(any(target_family = "unix", target_family = "windows")))]
            {
                let _ = child;
                let _ = live_output;
                Err(WorkerError::Protocol(
                    "PTY worker stdin transport is not supported on this platform".to_string(),
                ))
            }
        }
    }
}

fn spawn_stdin_writer<W>(stdin: W) -> mpsc::Sender<StdinCommand>
where
    W: Write + Send + 'static,
{
    let (tx, rx) = mpsc::channel::<StdinCommand>();
    thread::spawn(move || {
        let mut writer = std::io::BufWriter::new(stdin);
        for command in rx {
            match command {
                StdinCommand::Write { payload, reply } => {
                    let result = writer
                        .write_all(&payload)
                        .and_then(|_| writer.flush())
                        .map_err(WorkerError::Io);
                    let _ = reply.send(result);
                }
                StdinCommand::Close { reply } => {
                    let result = writer.flush().map_err(WorkerError::Io);
                    drop(writer);
                    let _ = reply.send(result);
                    return;
                }
            }
        }
    });
    tx
}

#[cfg(target_family = "unix")]
fn poll_timeout_until(deadline: std::time::Instant) -> i32 {
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    if remaining.is_zero() {
        return 0;
    }
    let millis = remaining.as_millis().max(1).min(i32::MAX as u128);
    millis as i32
}

fn shutdown_term_delay(timeout: Duration) -> Duration {
    if timeout.is_zero() {
        return Duration::from_secs(0);
    }
    let by_fraction = timeout.mul_f64(0.75);
    if timeout <= Duration::from_secs(10) {
        return by_fraction;
    }
    let by_remaining = timeout.saturating_sub(Duration::from_secs(10));
    by_fraction.min(by_remaining)
}

#[cfg(target_family = "windows")]
fn handle_windows_ipc_connect_result(
    connect_result: Result<(), std::io::Error>,
    child: &mut WorkerChild,
) -> Result<(), WorkerError> {
    match connect_result {
        Ok(()) => Ok(()),
        // Give the worker a short grace period to unwind before forcing
        // termination/reap after IPC startup failure.
        Err(err) => {
            const WRAPPER_EXIT_GRACE: Duration = Duration::from_secs(2);
            let deadline = std::time::Instant::now() + WRAPPER_EXIT_GRACE;
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) => {
                        if std::time::Instant::now() >= deadline {
                            let _ = child.kill();
                            let _ = child.wait();
                            break;
                        }
                        thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                }
            }
            Err(WorkerError::Io(err))
        }
    }
}

#[cfg(target_family = "windows")]
fn request_soft_termination(_child: &mut WorkerChild) -> Result<(), WorkerError> {
    // Let Windows workers exit through sideband shutdown when possible. Hard
    // termination remains the bounded fallback in the caller.
    Ok(())
}

#[cfg(target_family = "unix")]
fn configure_command_process_group(command: &mut Command, stdin_transport: WorkerStdinTransport) {
    if !matches!(stdin_transport, WorkerStdinTransport::Pipe) {
        return;
    }
    unsafe {
        command.pre_exec(|| {
            let _ = libc::setpgid(0, 0);
            Ok(())
        });
    }
}

#[cfg(target_family = "unix")]
fn set_command_arg0(command: &mut Command, arg0: &str) {
    command.arg0(arg0);
}

#[cfg(not(target_family = "unix"))]
fn set_command_arg0(_command: &mut Command, _arg0: &str) {}

fn format_exit_status_message(status: &std::process::ExitStatus) -> String {
    #[cfg(target_family = "unix")]
    if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(status) {
        return format!("[repl] worker exited with signal {signal}");
    }
    match status.code() {
        Some(code) => format!("[repl] worker exited with status {code}"),
        None => "[repl] worker exited with unknown status".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output_capture::{OUTPUT_RING_CAPACITY_BYTES, OutputEventKind, OutputRing};
    use crate::pending_output_tape::PendingOutputTape;
    use crate::worker_protocol::WorkerContent;
    use base64::Engine as _;
    use std::sync::{Mutex, OnceLock};

    fn env_test_mutex() -> &'static Mutex<()> {
        static TEST_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
        TEST_MUTEX.get_or_init(|| Mutex::new(()))
    }

    #[cfg(target_family = "windows")]
    #[test]
    fn session_end_output_reader_allows_bounded_natural_drain() {
        let stop_requested = Arc::new(AtomicBool::new(false));
        let thread_stop = stop_requested.clone();
        let drained = Arc::new(AtomicBool::new(false));
        let thread_drained = drained.clone();
        let (release_tx, release_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            release_rx.recv().expect("release output reader");
            if !thread_stop.load(Ordering::Relaxed) {
                thread_drained.store(true, Ordering::Relaxed);
            }
            done_tx.send(()).expect("finish output reader");
        });
        let reader = OutputReader {
            handle,
            done_rx,
            stop_requested: stop_requested.clone(),
        };
        let release = thread::spawn(move || {
            thread::sleep(Duration::from_millis(10));
            release_tx.send(()).expect("release output reader");
        });

        reader
            .stop_and_join("test output reader thread panicked")
            .expect("join output reader after bounded drain");
        release.join().expect("join output reader release");

        assert!(drained.load(Ordering::Relaxed));
        assert!(!stop_requested.load(Ordering::Relaxed));
    }

    #[cfg(target_family = "windows")]
    #[test]
    fn windows_pty_interrupt_payload_is_exact_etx() {
        assert_eq!(
            windows_interrupt_payload(WorkerStdinTransport::Pty).expect("ConPTY interrupt"),
            vec![0x03]
        );
    }

    #[cfg(target_family = "windows")]
    #[test]
    fn windows_pipe_interrupt_fails_explicitly() {
        let error = windows_interrupt_payload(WorkerStdinTransport::Pipe)
            .expect_err("pipe-only Windows workers cannot receive native Ctrl-C");
        let WorkerError::Protocol(message) = error else {
            panic!("expected protocol error, got {error}");
        };
        assert!(message.contains("ConPTY stdin"));
        assert!(message.contains("pipe stdin cannot be interrupted"));
    }

    fn capture_with_ring(
        oversized_output: OversizedOutputMode,
    ) -> (LiveOutputCapture, Arc<OutputRing>, PendingOutputTape) {
        let output_ring = Arc::new(OutputRing::with_capacity(OUTPUT_RING_CAPACITY_BYTES));
        let timeline = OutputTimeline::new(output_ring.clone());
        let tape = PendingOutputTape::with_timeline(timeline.clone());
        let capture = LiveOutputCapture::new(oversized_output, timeline);
        (capture, output_ring, tape)
    }

    fn ring_bytes(output_ring: &OutputRing) -> Vec<u8> {
        let output_end = output_ring.end_offset();
        output_ring.read_range(0, output_end).bytes
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_sandbox_pty_transport_uses_pipe_and_requests_conpty() {
        let mut command = Command::new("worker.exe");
        let prepared_args = vec![
            "--windows-sandbox".to_string(),
            "--sandbox-policy-cwd".to_string(),
            "C:\\workspace".to_string(),
        ];

        let transport =
            windows_spawn_transport(&mut command, &prepared_args, WorkerStdinTransport::Pty);

        assert!(matches!(transport, WorkerStdinTransport::Pipe));
        assert_eq!(
            command
                .get_envs()
                .find(|(key, _)| *key == crate::windows_conpty::WINDOWS_CONPTY_REQUEST_ENV)
                .and_then(|(_, value)| value)
                .map(|value| value.to_string_lossy().to_string()),
            Some("1".to_string())
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_offline_sandbox_pty_transport_uses_pipe_and_requests_conpty() {
        let mut command = Command::new("worker.exe");
        let prepared_args = vec![
            "--windows-sandbox-logon-offline".to_string(),
            "--windows-sandbox".to_string(),
            "--sandbox-policy-cwd".to_string(),
            "C:\\workspace".to_string(),
        ];

        let transport =
            windows_spawn_transport(&mut command, &prepared_args, WorkerStdinTransport::Pty);

        assert!(matches!(transport, WorkerStdinTransport::Pipe));
        assert_eq!(
            command
                .get_envs()
                .find(|(key, _)| *key == crate::windows_conpty::WINDOWS_CONPTY_REQUEST_ENV)
                .and_then(|(_, value)| value)
                .map(|value| value.to_string_lossy().to_string()),
            Some("1".to_string())
        );
    }

    #[cfg(target_family = "unix")]
    fn successful_test_child() -> Child {
        Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn exiting test child")
    }

    #[cfg(target_family = "windows")]
    fn sleeping_test_child() -> Child {
        Command::new("powershell.exe")
            .args(["-NoProfile", "-Command", "Start-Sleep -Seconds 30"])
            .spawn()
            .expect("spawn sleeping test child")
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn finish_exited_does_not_signal_reaped_root_pid() {
        let _guard = env_test_mutex().lock().expect("env mutex");
        let child = successful_test_child();
        let (result, kills) =
            capture_recorded_unix_kills(|| WorkerProcess::new_for_test(child).finish_exited());

        assert!(
            result.is_ok(),
            "expected finish_exited to succeed: {result:?}"
        );
        assert!(
            kills.is_empty(),
            "did not expect finish_exited to signal an already reaped root pid, got: {kills:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_bwrap_startup_retry_matches_worker_ready_failures() {
        for message in [
            "ipc disconnected while waiting for worker_ready",
            "worker session ended before worker_ready",
        ] {
            assert!(
                linux_sandbox_startup_retryable(&WorkerError::Protocol(message.to_string())),
                "expected worker_ready startup failure to be retryable: {message}"
            );
        }
    }

    #[test]
    fn apply_debug_startup_env_uses_mcp_repl_vars() {
        let _guard = env_test_mutex().lock().expect("env mutex");
        let original = std::env::var_os(crate::debug_logs::DEBUG_SESSION_DIR_ENV);
        let original_startup_path = std::env::var_os(crate::diagnostics::STARTUP_LOG_PATH_ENV);
        unsafe {
            std::env::set_var(
                crate::debug_logs::DEBUG_SESSION_DIR_ENV,
                "/tmp/mcp-repl-debug-session",
            );
            std::env::remove_var(crate::diagnostics::STARTUP_LOG_PATH_ENV);
        }

        let mut command = Command::new("env");
        apply_debug_startup_env(&mut command, None);
        let envs: std::collections::BTreeMap<_, _> = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect();

        match original {
            Some(value) => unsafe {
                std::env::set_var(crate::debug_logs::DEBUG_SESSION_DIR_ENV, value);
            },
            None => unsafe {
                std::env::remove_var(crate::debug_logs::DEBUG_SESSION_DIR_ENV);
            },
        }
        match original_startup_path {
            Some(value) => unsafe {
                std::env::set_var(crate::diagnostics::STARTUP_LOG_PATH_ENV, value);
            },
            None => unsafe {
                std::env::remove_var(crate::diagnostics::STARTUP_LOG_PATH_ENV);
            },
        }

        assert_eq!(
            envs.get(crate::debug_logs::DEBUG_SESSION_DIR_ENV),
            Some(&Some("/tmp/mcp-repl-debug-session".to_string()))
        );
        assert_eq!(envs.get(crate::diagnostics::STARTUP_LOG_PATH_ENV), None);
    }

    #[test]
    fn apply_debug_startup_env_uses_session_tmpdir_for_worker_log() {
        let _guard = env_test_mutex().lock().expect("env mutex");
        let original = std::env::var_os(crate::debug_logs::DEBUG_SESSION_DIR_ENV);
        let original_startup_path = std::env::var_os(crate::diagnostics::STARTUP_LOG_PATH_ENV);
        unsafe {
            std::env::set_var(
                crate::debug_logs::DEBUG_SESSION_DIR_ENV,
                "/tmp/mcp-repl-debug-session",
            );
            std::env::remove_var(crate::diagnostics::STARTUP_LOG_PATH_ENV);
        }

        let mut command = Command::new("env");
        let session_tmpdir = PathBuf::from("/tmp/mcp-repl-session-tmp");
        apply_debug_startup_env(&mut command, Some(&session_tmpdir));
        let envs: std::collections::BTreeMap<_, _> = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect();

        match original {
            Some(value) => unsafe {
                std::env::set_var(crate::debug_logs::DEBUG_SESSION_DIR_ENV, value);
            },
            None => unsafe {
                std::env::remove_var(crate::debug_logs::DEBUG_SESSION_DIR_ENV);
            },
        }
        match original_startup_path {
            Some(value) => unsafe {
                std::env::set_var(crate::diagnostics::STARTUP_LOG_PATH_ENV, value);
            },
            None => unsafe {
                std::env::remove_var(crate::diagnostics::STARTUP_LOG_PATH_ENV);
            },
        }

        assert_eq!(
            envs.get(crate::debug_logs::DEBUG_SESSION_DIR_ENV),
            Some(&Some("/tmp/mcp-repl-debug-session".to_string()))
        );
        assert_eq!(
            envs.get(crate::diagnostics::STARTUP_LOG_PATH_ENV),
            Some(&Some(
                session_tmpdir
                    .join(crate::diagnostics::WORKER_STARTUP_LOG_FILE_NAME)
                    .display()
                    .to_string()
            ))
        );
    }

    #[test]
    fn persist_worker_startup_log_copies_into_debug_session_dir() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_tmpdir = temp.path().join("session-tmp");
        let debug_session_dir = temp.path().join("debug-session");
        std::fs::create_dir_all(&session_tmpdir).expect("create session tmpdir");
        std::fs::create_dir_all(&debug_session_dir).expect("create debug session dir");

        let source = session_tmpdir.join(crate::diagnostics::WORKER_STARTUP_LOG_FILE_NAME);
        let destination = debug_session_dir.join(crate::diagnostics::WORKER_STARTUP_LOG_FILE_NAME);
        std::fs::write(&source, "worker startup log\n").expect("write source log");

        persist_worker_startup_log(&session_tmpdir, Some(destination.clone()));

        assert_eq!(
            std::fs::read_to_string(&destination).expect("read destination log"),
            "worker startup log\n"
        );
    }

    #[test]
    fn cleanup_worker_session_tmpdir_persists_log_when_keep_tmpdir_is_set() {
        let _guard = env_test_mutex().lock().expect("env mutex");
        let temp = tempfile::tempdir().expect("tempdir");
        let session_tmpdir = temp.path().join("session-tmp");
        let debug_session_dir = temp.path().join("debug-session");
        std::fs::create_dir_all(&session_tmpdir).expect("create session tmpdir");
        std::fs::create_dir_all(&debug_session_dir).expect("create debug session dir");

        let source = session_tmpdir.join(crate::diagnostics::WORKER_STARTUP_LOG_FILE_NAME);
        let destination = debug_session_dir.join(crate::diagnostics::WORKER_STARTUP_LOG_FILE_NAME);
        std::fs::write(&source, "worker startup log\n").expect("write source log");

        let original_keep = std::env::var_os("MCP_REPL_KEEP_SESSION_TMPDIR");
        unsafe {
            std::env::set_var("MCP_REPL_KEEP_SESSION_TMPDIR", "1");
        }

        cleanup_worker_session_tmpdir(&session_tmpdir, Some(destination.clone()));

        match original_keep {
            Some(value) => unsafe {
                std::env::set_var("MCP_REPL_KEEP_SESSION_TMPDIR", value);
            },
            None => unsafe {
                std::env::remove_var("MCP_REPL_KEEP_SESSION_TMPDIR");
            },
        }

        assert!(
            session_tmpdir.is_dir(),
            "session tmpdir should be preserved"
        );
        assert_eq!(
            std::fs::read_to_string(&destination).expect("read destination log"),
            "worker startup log\n"
        );
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn drop_cleans_live_worker_session_tmpdir() {
        use std::os::unix::process::CommandExt;

        let temp = tempfile::tempdir().expect("tempdir");
        let session_tmpdir = temp.path().join("session-tmp");
        std::fs::create_dir_all(&session_tmpdir).expect("create session tmpdir");
        std::fs::write(session_tmpdir.join("marker"), "temp").expect("write temp marker");

        let mut command = Command::new("sleep");
        command.arg("30");
        command.stdin(Stdio::null());
        command.stdout(Stdio::null());
        command.stderr(Stdio::null());
        unsafe {
            command.pre_exec(|| {
                let _ = libc::setpgid(0, 0);
                Ok(())
            });
        }
        let child = command.spawn().expect("spawn child");
        let pid = child.id();
        let mut process = WorkerProcess::new_for_test(child);
        process.session_tmpdir = Some(session_tmpdir.clone());

        drop(process);

        let leaked_tmpdir = session_tmpdir.exists();
        if leaked_tmpdir {
            let _ = raw_unix_kill(-(pid as i32), libc::SIGKILL);
            unsafe {
                let _ = libc::waitpid(pid as i32, std::ptr::null_mut(), 0);
            }
            let _ = std::fs::remove_dir_all(&session_tmpdir);
        }
        assert!(
            !leaked_tmpdir,
            "dropping WorkerProcess should remove the session temp dir"
        );
    }

    #[test]
    fn pager_output_capture_writes_to_shared_timeline() {
        let output_ring = Arc::new(OutputRing::with_capacity(OUTPUT_RING_CAPACITY_BYTES));
        let capture = LiveOutputCapture::new(
            OversizedOutputMode::Pager,
            OutputTimeline::new(output_ring.clone()),
        );
        capture.append_output_text(b"pager output\n", TextStream::Stdout, false);
        capture.append_image(IpcOutputImage {
            id: "img-1".to_string(),
            data: "AA==".to_string(),
            mime_type: "image/png".to_string(),
            is_new: true,
            updates_previous_image: false,
            readline_results_seen: 0,
        });
        capture.append_sideband(PendingSidebandKind::RequestBoundary);

        let output_end = output_ring.end_offset();
        let output_range = output_ring.read_range(0, output_end);
        assert!(
            output_end > 0,
            "pager mode should still append text to the output timeline"
        );
        assert_eq!(
            output_range.bytes, b"pager output\n",
            "pager mode should keep stdout text in the output timeline"
        );
        assert!(
            output_range.events.iter().any(|event| {
                matches!(
                    &event.kind,
                    OutputEventKind::Image { id, mime_type, .. }
                    if id == "img-1" && mime_type == "image/png"
                )
            }),
            "pager mode should keep image events in the output timeline"
        );
    }

    #[test]
    fn raw_windows_conpty_startup_noise_is_dropped_before_input() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();

        capture.append_raw_text(b"\x1b[?9001h\x1b[?1004h\r\n", TextStream::Stdout);

        assert_eq!(ring_bytes(&output_ring), b"");
        assert!(
            tape.drain_final_output().contents.is_empty(),
            "raw ConPTY startup toggles should not enter output bundles"
        );
    }

    #[test]
    fn sideband_terminal_mode_toggles_are_preserved() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();

        capture.append_output_text(b"\x1b[?9001h\x1b[?1004h", TextStream::Stdout, false);

        assert_eq!(ring_bytes(&output_ring), b"\x1b[?9001h\x1b[?1004h");
        assert_eq!(
            tape.drain_final_output().contents,
            vec![WorkerContent::worker_stdout("\u{1b}[?9001h\u{1b}[?1004h")]
        );
    }

    #[test]
    fn raw_windows_conpty_split_startup_noise_survives_interleaved_ipc_output() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();
        let split = 8;

        capture.append_raw_text(
            &WindowsConptyStartupNoiseFilter::PREFIX[..split],
            TextStream::Stdout,
        );
        capture.append_output_text(b"prompt", TextStream::Stdout, false);
        capture.append_raw_text(
            &WindowsConptyStartupNoiseFilter::PREFIX[split..],
            TextStream::Stdout,
        );
        capture.append_raw_text(b"\r\n", TextStream::Stdout);
        capture.finish_raw_text(TextStream::Stdout);
        capture.finalize_windows_conpty_raw_text();

        assert_eq!(ring_bytes(&output_ring), b"prompt");
        assert_eq!(
            tape.drain_final_output().contents,
            vec![WorkerContent::worker_stdout("prompt")]
        );
    }

    #[test]
    fn raw_windows_conpty_startup_noise_is_dropped_after_input_starts() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();

        capture.note_accepted_input_starting();
        capture.append_raw_text(b"\x1b[?9001h\x1b[?1004h", TextStream::Stdout);

        assert_eq!(ring_bytes(&output_ring), b"");
        assert!(
            tape.drain_final_output().contents.is_empty(),
            "queued ConPTY startup bytes must not race the first accepted input"
        );
    }

    #[test]
    fn raw_windows_conpty_startup_noise_filter_handles_split_reads() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();

        capture.append_raw_text(b"\x1b[?9001", TextStream::Stdout);
        capture.note_accepted_input_starting();
        capture.append_raw_text(b"h\x1b[?1004h\r\n", TextStream::Stdout);
        capture.append_raw_text(b"visible\n", TextStream::Stdout);
        capture.flush_unarmed_windows_conpty_ambiguous_lf();

        assert_eq!(ring_bytes(&output_ring), b"visible\n");
        assert_eq!(
            tape.drain_final_output().contents,
            vec![WorkerContent::worker_stdout("visible\n")]
        );
    }

    #[test]
    fn raw_windows_conpty_startup_blank_lines_keep_a_bounded_candidate() {
        let mut filter = WindowsConptyStartupNoiseFilter::default();

        let startup = filter.filter(WindowsConptyStartupNoiseFilter::PREFIX);
        assert!(startup.raw.is_none());
        let blank_lines = vec![b'\n'; 1024 * 1024];
        let filtered = filter.filter(&blank_lines);

        assert!(filtered.raw.is_none());
        assert!(
            filter.prefix.len() <= 1,
            "classified startup whitespace must remain bounded"
        );
    }

    #[test]
    fn raw_windows_conpty_incomplete_startup_prefix_is_preserved_at_eof() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();

        capture.append_raw_text(b"\x1b[?9001", TextStream::Stdout);
        assert_eq!(ring_bytes(&output_ring), b"");

        capture.finish_raw_text(TextStream::Stdout);
        capture.finalize_windows_conpty_raw_text();

        assert_eq!(ring_bytes(&output_ring), b"\x1b[?9001");
        assert_eq!(
            tape.drain_final_output().contents,
            vec![WorkerContent::worker_stdout("\u{1b}[?9001")]
        );
    }

    #[test]
    fn raw_windows_conpty_startup_filter_preserves_mixed_output() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();

        capture.append_raw_text(b"\x1b[?9001hvisible\n", TextStream::Stdout);
        capture.flush_unarmed_windows_conpty_ambiguous_lf();

        assert_eq!(ring_bytes(&output_ring), b"\x1b[?9001hvisible\n");
        assert_eq!(
            tape.drain_final_output().contents,
            vec![WorkerContent::worker_stdout("\u{1b}[?9001hvisible\n")]
        );
    }

    #[test]
    fn raw_terminal_mode_toggles_are_preserved_after_startup_noise_is_consumed() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();

        capture.append_raw_text(b"\x1b[?9001h\x1b[?1004h", TextStream::Stdout);
        capture.append_raw_text(b"user:\x1b[?9001h\x1b[?1004h\n", TextStream::Stdout);
        capture.flush_unarmed_windows_conpty_ambiguous_lf();

        assert_eq!(ring_bytes(&output_ring), b"user:\x1b[?9001h\x1b[?1004h\n");
        assert_eq!(
            tape.drain_final_output().contents,
            vec![WorkerContent::worker_stdout(
                "user:\u{1b}[?9001h\u{1b}[?1004h\n"
            )]
        );
    }

    #[test]
    fn raw_windows_conpty_shutdown_reset_is_dropped_only_after_shutdown_starts() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();

        capture.append_raw_text(b"\x1b[?9001h\x1b[?1004h", TextStream::Stdout);
        capture.append_raw_text(b"ready", TextStream::Stdout);
        capture.note_windows_conpty_shutdown_starting();
        capture.append_raw_text(
            WindowsConptyShutdownResetFilter::SIMPLE_SEQUENCE,
            TextStream::Stdout,
        );

        assert_eq!(ring_bytes(&output_ring), b"ready");
        assert_eq!(
            tape.drain_final_output().contents,
            vec![WorkerContent::worker_stdout("ready")]
        );
    }

    #[test]
    fn raw_windows_conpty_lf_prefixed_shutdown_reset_is_dropped_when_armed() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();

        capture.append_raw_text(b"\x1b[?9001h\x1b[?1004h", TextStream::Stdout);
        capture.append_raw_text(b"ready", TextStream::Stdout);
        capture.note_windows_conpty_shutdown_starting();
        capture.append_raw_text(
            WindowsConptyShutdownResetFilter::LF_PREFIXED_SIMPLE_SEQUENCE,
            TextStream::Stdout,
        );

        assert_eq!(ring_bytes(&output_ring), b"ready");
        assert_eq!(
            tape.drain_final_output().contents,
            vec![WorkerContent::worker_stdout("ready")]
        );
    }

    #[test]
    fn raw_windows_conpty_lf_reset_after_startup_newline_filter_is_dropped() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();

        capture.append_raw_text(WindowsConptyStartupNoiseFilter::PREFIX, TextStream::Stdout);
        capture.note_windows_conpty_shutdown_starting();
        capture.append_raw_text(
            WindowsConptyShutdownResetFilter::LF_PREFIXED_SIMPLE_SEQUENCE,
            TextStream::Stdout,
        );
        capture.finish_raw_text(TextStream::Stdout);
        capture.finalize_windows_conpty_raw_text();

        assert_eq!(ring_bytes(&output_ring), b"");
        assert!(tape.drain_final_output().contents.is_empty());
    }

    #[test]
    fn windows_conpty_cross_route_shutdown_reset_is_dropped_at_every_raw_split() {
        let reset = WindowsConptyShutdownResetFilter::BARE_SIMPLE_SEQUENCE;
        for split in 0..=reset.len() {
            let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
            let capture = capture.with_windows_conpty_startup_noise_filter();

            capture.note_windows_conpty_shutdown_starting();
            capture.append_output_text(b"\n", TextStream::Stdout, false);
            if split > 0 {
                capture.append_raw_text(&reset[..split], TextStream::Stdout);
            }
            if split < reset.len() {
                capture.append_raw_text(&reset[split..], TextStream::Stdout);
            }
            capture.finish_raw_text(TextStream::Stdout);
            capture.finalize_windows_conpty_raw_text();

            assert_eq!(
                ring_bytes(&output_ring),
                b"",
                "cross-route reset leaked with raw split at {split}"
            );
            assert!(
                tape.drain_final_output().contents.is_empty(),
                "cross-route reset entered the reply with raw split at {split}"
            );
        }
    }

    #[test]
    fn windows_conpty_split_cross_route_filter_keeps_lf_staged_until_pair_completes() {
        let reset = WindowsConptyShutdownResetFilter::BARE_SIMPLE_SEQUENCE;
        let mut filter = WindowsConptyStartupNoiseFilter::default();

        assert!(filter.arm_shutdown_reset().is_none());
        assert!(filter.stage_ipc_shutdown_lf(b"\n", false));
        let first = filter.filter(&reset[..3]);
        assert!(first.ipc_lf.is_none());
        assert!(first.raw.is_none());
        assert!(filter.pending_ipc_shutdown_lf.is_some());

        let second = filter.filter(&reset[3..]);
        assert!(second.ipc_lf.is_none());
        assert!(second.raw.is_none());
        assert!(filter.pending_ipc_shutdown_lf.is_none());
    }

    #[test]
    fn windows_conpty_cross_route_reset_is_matched_after_startup_noise() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();
        let raw = [
            WindowsConptyStartupNoiseFilter::PREFIX,
            WindowsConptyShutdownResetFilter::BARE_SIMPLE_SEQUENCE,
        ]
        .concat();

        capture.note_windows_conpty_shutdown_starting();
        capture.append_output_text(b"\n", TextStream::Stdout, false);
        capture.append_raw_text(&raw, TextStream::Stdout);
        capture.finish_raw_text(TextStream::Stdout);
        capture.finalize_windows_conpty_raw_text();

        assert_eq!(ring_bytes(&output_ring), b"");
        assert!(tape.drain_final_output().contents.is_empty());
    }

    #[test]
    fn windows_conpty_one_ipc_lf_drops_only_one_bare_raw_reset() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();
        let reset = WindowsConptyShutdownResetFilter::BARE_SIMPLE_SEQUENCE;
        let raw = [reset, reset].concat();

        capture.note_windows_conpty_shutdown_starting();
        capture.append_output_text(b"\n", TextStream::Stdout, false);
        capture.append_raw_text(&raw, TextStream::Stdout);
        capture.finish_raw_text(TextStream::Stdout);
        capture.finalize_windows_conpty_raw_text();

        assert_eq!(ring_bytes(&output_ring), reset);
        assert_eq!(
            tape.drain_final_output().contents,
            vec![WorkerContent::worker_stdout(
                String::from_utf8(reset.to_vec()).expect("test reset sequence is UTF-8")
            )]
        );
    }

    #[test]
    fn windows_conpty_cross_route_shutdown_reset_is_dropped_when_raw_arrives_first() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();

        capture.note_windows_conpty_shutdown_starting();
        capture.append_raw_text(
            WindowsConptyShutdownResetFilter::BARE_SIMPLE_SEQUENCE,
            TextStream::Stdout,
        );
        capture.append_output_text(b"\n", TextStream::Stdout, false);
        capture.finish_raw_text(TextStream::Stdout);
        capture.finalize_windows_conpty_raw_text();

        assert_eq!(ring_bytes(&output_ring), b"");
        assert!(tape.drain_final_output().contents.is_empty());
    }

    #[test]
    fn windows_conpty_bare_raw_reset_without_ipc_lf_is_preserved() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();
        let reset = WindowsConptyShutdownResetFilter::BARE_SIMPLE_SEQUENCE;

        capture.note_windows_conpty_shutdown_starting();
        capture.append_raw_text(reset, TextStream::Stdout);
        capture.finish_raw_text(TextStream::Stdout);
        capture.finalize_windows_conpty_raw_text();

        assert_eq!(ring_bytes(&output_ring), reset);
        assert_eq!(
            tape.drain_final_output().contents,
            vec![WorkerContent::worker_stdout(
                String::from_utf8(reset.to_vec()).expect("test reset sequence is UTF-8")
            )]
        );
    }

    #[test]
    fn windows_conpty_cross_route_reset_preserves_prior_ipc_cleanup() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();
        let reset = WindowsConptyShutdownResetFilter::BARE_SIMPLE_SEQUENCE;

        capture.note_windows_conpty_shutdown_starting();
        capture.append_output_text(b"cleanup", TextStream::Stdout, false);
        capture.append_output_text(b"\n", TextStream::Stdout, false);
        capture.append_raw_text(reset, TextStream::Stdout);
        capture.finish_raw_text(TextStream::Stdout);
        capture.finalize_windows_conpty_raw_text();

        let mut expected = b"cleanup\n".to_vec();
        expected.extend_from_slice(reset);
        assert_eq!(ring_bytes(&output_ring), expected);
        assert_eq!(
            tape.drain_final_output().contents,
            vec![WorkerContent::worker_stdout(
                String::from_utf8(expected).expect("test reset sequence is UTF-8")
            )]
        );
    }

    #[test]
    fn windows_conpty_unpaired_bare_reset_does_not_consume_later_cleanup_output() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();
        let reset = WindowsConptyShutdownResetFilter::BARE_SIMPLE_SEQUENCE;

        capture.note_windows_conpty_shutdown_starting();
        capture.append_raw_text(reset, TextStream::Stdout);
        capture.append_output_text(b"cleanup", TextStream::Stdout, false);
        capture.append_output_text(b"\n", TextStream::Stdout, false);
        capture.finish_raw_text(TextStream::Stdout);
        capture.finalize_windows_conpty_raw_text();

        let mut expected = reset.to_vec();
        expected.extend_from_slice(b"cleanup\n");
        assert_eq!(ring_bytes(&output_ring), expected);
        assert_eq!(
            tape.drain_final_output().contents,
            vec![WorkerContent::worker_stdout(
                String::from_utf8(expected).expect("test reset sequence is UTF-8")
            )]
        );
    }

    #[test]
    fn windows_conpty_staged_ipc_lf_precedes_ordinary_raw_output() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();

        capture.note_windows_conpty_shutdown_starting();
        capture.append_output_text(b"\n", TextStream::Stdout, false);
        capture.append_raw_text(b"ordinary", TextStream::Stdout);
        capture.finish_raw_text(TextStream::Stdout);
        capture.finalize_windows_conpty_raw_text();

        assert_eq!(ring_bytes(&output_ring), b"\nordinary");
        assert_eq!(
            tape.drain_final_output().contents,
            vec![WorkerContent::worker_stdout("\nordinary")]
        );
    }

    #[test]
    fn windows_conpty_staged_ipc_lf_is_preserved_without_raw_reset() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();

        capture.note_windows_conpty_shutdown_starting();
        capture.append_output_text(b"\n", TextStream::Stdout, false);
        capture.finish_raw_text(TextStream::Stdout);
        capture.finalize_windows_conpty_raw_text();

        assert_eq!(ring_bytes(&output_ring), b"\n");
        assert_eq!(
            tape.drain_final_output().contents,
            vec![WorkerContent::worker_stdout("\n")]
        );
    }

    #[test]
    fn windows_conpty_repeated_session_end_keeps_cross_route_pair_staged() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();

        capture.note_windows_conpty_shutdown_starting();
        capture.append_output_text(b"\n", TextStream::Stdout, false);
        capture.append_sideband(PendingSidebandKind::SessionEnd);
        capture.append_raw_text(
            WindowsConptyShutdownResetFilter::BARE_SIMPLE_SEQUENCE,
            TextStream::Stdout,
        );
        capture.finish_raw_text(TextStream::Stdout);
        capture.finalize_windows_conpty_raw_text();

        assert_eq!(ring_bytes(&output_ring), b"");
        assert!(tape.drain_final_output().contents.is_empty());
    }

    #[test]
    fn windows_conpty_cross_route_candidate_is_preserved_when_unarmed() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();
        let reset = WindowsConptyShutdownResetFilter::BARE_SIMPLE_SEQUENCE;

        capture.append_output_text(b"\n", TextStream::Stdout, false);
        capture.append_raw_text(reset, TextStream::Stdout);
        capture.finish_raw_text(TextStream::Stdout);
        capture.finalize_windows_conpty_raw_text();

        let mut expected = b"\n".to_vec();
        expected.extend_from_slice(reset);
        assert_eq!(ring_bytes(&output_ring), expected);
        assert_eq!(
            tape.drain_final_output().contents,
            vec![WorkerContent::worker_stdout(
                String::from_utf8(expected).expect("test reset sequence is UTF-8")
            )]
        );
    }

    #[test]
    fn windows_conpty_cross_route_reset_preserves_raw_suffix() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();
        let mut reset_and_suffix = WindowsConptyShutdownResetFilter::BARE_SIMPLE_SEQUENCE.to_vec();
        reset_and_suffix.extend_from_slice(b"after");

        capture.note_windows_conpty_shutdown_starting();
        capture.append_output_text(b"\n", TextStream::Stdout, false);
        capture.append_raw_text(&reset_and_suffix, TextStream::Stdout);
        capture.finish_raw_text(TextStream::Stdout);
        capture.finalize_windows_conpty_raw_text();

        assert_eq!(ring_bytes(&output_ring), b"after");
        assert_eq!(
            tape.drain_final_output().contents,
            vec![WorkerContent::worker_stdout("after")]
        );
    }

    #[test]
    fn windows_conpty_cross_route_reset_after_raw_output_preserves_staged_lf_order() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();
        let mut ordinary_and_reset = b"ordinary".to_vec();
        ordinary_and_reset
            .extend_from_slice(WindowsConptyShutdownResetFilter::BARE_SIMPLE_SEQUENCE);

        capture.note_windows_conpty_shutdown_starting();
        capture.append_output_text(b"\n", TextStream::Stdout, false);
        capture.append_raw_text(&ordinary_and_reset, TextStream::Stdout);
        capture.finish_raw_text(TextStream::Stdout);
        capture.finalize_windows_conpty_raw_text();

        let mut expected = b"\nordinary".to_vec();
        expected.extend_from_slice(WindowsConptyShutdownResetFilter::BARE_SIMPLE_SEQUENCE);
        assert_eq!(ring_bytes(&output_ring), expected);
        assert_eq!(
            tape.drain_final_output().contents,
            vec![WorkerContent::worker_stdout(
                String::from_utf8(expected).expect("test reset sequence is UTF-8")
            )]
        );
    }

    #[test]
    fn windows_conpty_raw_candidate_before_ipc_lf_keeps_arrival_order_when_disproved() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();

        capture.note_windows_conpty_shutdown_starting();
        capture.append_raw_text(b"\x1b[?", TextStream::Stdout);
        capture.append_output_text(b"\n", TextStream::Stdout, false);
        capture.append_raw_text(b"X", TextStream::Stdout);
        capture.finish_raw_text(TextStream::Stdout);
        capture.finalize_windows_conpty_raw_text();

        assert_eq!(ring_bytes(&output_ring), b"\x1b[?\nX");
        assert_eq!(
            tape.drain_final_output().contents,
            vec![WorkerContent::worker_stdout("\u{1b}[?\nX")]
        );
    }

    #[test]
    fn windows_conpty_raw_candidate_before_ipc_lf_keeps_arrival_order_at_eof() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();

        capture.note_windows_conpty_shutdown_starting();
        capture.append_raw_text(b"\x1b[?", TextStream::Stdout);
        capture.append_output_text(b"\n", TextStream::Stdout, false);
        capture.finish_raw_text(TextStream::Stdout);
        capture.finalize_windows_conpty_raw_text();

        assert_eq!(ring_bytes(&output_ring), b"\x1b[?\n");
        assert_eq!(
            tape.drain_final_output().contents,
            vec![WorkerContent::worker_stdout("\u{1b}[?\n")]
        );
    }

    #[test]
    fn windows_conpty_shutdown_marker_preserves_order_after_startup_finishes() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();

        capture.append_raw_text(WindowsConptyStartupNoiseFilter::PREFIX, TextStream::Stdout);
        capture.append_raw_text(b"ready", TextStream::Stdout);
        capture.note_windows_conpty_shutdown_starting();
        capture.append_raw_text(b"\x1b[?", TextStream::Stdout);
        capture.append_output_text(b"\n", TextStream::Stdout, false);
        capture.append_raw_text(b"X", TextStream::Stdout);
        capture.finish_raw_text(TextStream::Stdout);
        capture.finalize_windows_conpty_raw_text();

        assert_eq!(ring_bytes(&output_ring), b"ready\x1b[?\nX");
        assert_eq!(
            tape.drain_final_output().contents,
            vec![WorkerContent::worker_stdout("ready\u{1b}[?\nX")]
        );
    }

    #[test]
    fn windows_conpty_shutdown_marker_flushes_before_stderr_boundary() {
        let (capture, output_ring, _) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();

        capture.append_raw_text(WindowsConptyStartupNoiseFilter::PREFIX, TextStream::Stdout);
        capture.append_raw_text(b"ready", TextStream::Stdout);
        capture.note_windows_conpty_shutdown_starting();
        capture.append_raw_text(b"\x1b[?", TextStream::Stdout);
        capture.append_output_text(b"\n", TextStream::Stdout, false);
        capture.append_raw_text(b"err", TextStream::Stderr);
        capture.append_raw_text(b"X", TextStream::Stdout);
        capture.finish_raw_text(TextStream::Stdout);
        capture.finalize_windows_conpty_raw_text();

        assert_eq!(ring_bytes(&output_ring), b"ready\x1b[?\nerrX");
    }

    #[test]
    fn windows_conpty_startup_marker_preserves_raw_local_startup_candidate() {
        let (capture, output_ring, _) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();

        capture.note_windows_conpty_shutdown_starting();
        capture.append_raw_text(b"\x1b[?", TextStream::Stdout);
        capture.append_output_text(b"\n", TextStream::Stdout, false);
        capture.append_raw_text(b"err", TextStream::Stderr);
        capture.append_raw_text(b"X", TextStream::Stdout);
        capture.finish_raw_text(TextStream::Stdout);
        capture.finalize_windows_conpty_raw_text();

        assert_eq!(ring_bytes(&output_ring), b"\nerr\x1b[?X");
    }

    #[test]
    fn windows_conpty_startup_marker_does_not_leak_split_startup_noise() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();
        let split = 8;

        capture.note_windows_conpty_shutdown_starting();
        capture.append_raw_text(
            &WindowsConptyStartupNoiseFilter::PREFIX[..split],
            TextStream::Stdout,
        );
        capture.append_output_text(b"\n", TextStream::Stdout, false);
        capture.append_raw_text(b"err", TextStream::Stderr);
        capture.append_raw_text(
            &WindowsConptyStartupNoiseFilter::PREFIX[split..],
            TextStream::Stdout,
        );
        capture.finish_raw_text(TextStream::Stdout);
        capture.finalize_windows_conpty_raw_text();

        assert_eq!(ring_bytes(&output_ring), b"\nerr");
        assert_eq!(
            tape.drain_final_output().contents,
            vec![
                WorkerContent::worker_stdout("\n"),
                WorkerContent::worker_stderr("stderr: err")
            ]
        );
    }

    #[test]
    fn windows_conpty_shutdown_marker_adjusts_when_legacy_reset_is_dropped() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();
        let reset = WindowsConptyShutdownResetFilter::SIMPLE_SEQUENCE;
        let mut reset_tail_and_after = reset[2..].to_vec();
        reset_tail_and_after.extend_from_slice(b"after");

        capture.append_raw_text(WindowsConptyStartupNoiseFilter::PREFIX, TextStream::Stdout);
        capture.append_raw_text(b"ready", TextStream::Stdout);
        capture.note_windows_conpty_shutdown_starting();
        capture.append_raw_text(&reset[..2], TextStream::Stdout);
        capture.append_output_text(b"\n", TextStream::Stdout, false);
        capture.append_raw_text(&reset_tail_and_after, TextStream::Stdout);
        capture.finish_raw_text(TextStream::Stdout);
        capture.finalize_windows_conpty_raw_text();

        assert_eq!(ring_bytes(&output_ring), b"ready\nafter");
        assert_eq!(
            tape.drain_final_output().contents,
            vec![WorkerContent::worker_stdout("ready\nafter")]
        );
    }

    #[test]
    fn windows_conpty_reverse_reset_after_raw_prefix_pairs_with_later_ipc_lf() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();
        let mut ordinary_and_reset = b"ordinary".to_vec();
        ordinary_and_reset
            .extend_from_slice(WindowsConptyShutdownResetFilter::BARE_SIMPLE_SEQUENCE);

        capture.note_windows_conpty_shutdown_starting();
        capture.append_raw_text(&ordinary_and_reset, TextStream::Stdout);
        capture.append_output_text(b"\n", TextStream::Stdout, false);
        capture.finish_raw_text(TextStream::Stdout);
        capture.finalize_windows_conpty_raw_text();

        assert_eq!(ring_bytes(&output_ring), b"ordinary");
        assert_eq!(
            tape.drain_final_output().contents,
            vec![WorkerContent::worker_stdout("ordinary")]
        );
    }

    #[test]
    fn windows_conpty_reverse_reset_with_raw_suffix_preserves_later_ipc_lf() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();
        let mut reset_and_suffix = WindowsConptyShutdownResetFilter::BARE_SIMPLE_SEQUENCE.to_vec();
        reset_and_suffix.extend_from_slice(b"after");

        capture.note_windows_conpty_shutdown_starting();
        capture.append_raw_text(&reset_and_suffix, TextStream::Stdout);
        capture.append_output_text(b"\n", TextStream::Stdout, false);
        capture.finish_raw_text(TextStream::Stdout);
        capture.finalize_windows_conpty_raw_text();

        let mut expected = reset_and_suffix;
        expected.extend_from_slice(b"\n");
        assert_eq!(ring_bytes(&output_ring), expected);
        assert_eq!(
            tape.drain_final_output().contents,
            vec![WorkerContent::worker_stdout(
                String::from_utf8(expected).expect("test reset sequence is UTF-8")
            )]
        );
    }

    #[test]
    fn windows_conpty_reverse_pair_does_not_cross_image_or_sideband_boundaries() {
        for boundary in ["image", "request-boundary"] {
            let (capture, output_ring, _tape) = capture_with_ring(OversizedOutputMode::Files);
            let capture = capture.with_windows_conpty_startup_noise_filter();

            capture.note_windows_conpty_shutdown_starting();
            capture.append_raw_text(
                WindowsConptyShutdownResetFilter::BARE_SIMPLE_SEQUENCE,
                TextStream::Stdout,
            );
            match boundary {
                "image" => capture.append_image(IpcOutputImage {
                    id: "img-1".to_string(),
                    data: "AA==".to_string(),
                    mime_type: "image/png".to_string(),
                    is_new: true,
                    updates_previous_image: false,
                    readline_results_seen: 0,
                }),
                "request-boundary" => {
                    capture.append_sideband(PendingSidebandKind::RequestBoundary);
                }
                _ => unreachable!(),
            }
            capture.append_output_text(b"\n", TextStream::Stdout, false);
            capture.finish_raw_text(TextStream::Stdout);
            capture.finalize_windows_conpty_raw_text();

            let mut expected = WindowsConptyShutdownResetFilter::BARE_SIMPLE_SEQUENCE.to_vec();
            expected.extend_from_slice(b"\n");
            assert_eq!(
                ring_bytes(&output_ring),
                expected,
                "unpaired reset or IPC LF was lost across {boundary}"
            );
        }
    }

    #[test]
    fn windows_conpty_reverse_pair_does_not_cross_stderr_boundaries() {
        for raw_stderr in [false, true] {
            let (capture, output_ring, _tape) = capture_with_ring(OversizedOutputMode::Files);
            let capture = capture.with_windows_conpty_startup_noise_filter();

            capture.note_windows_conpty_shutdown_starting();
            capture.append_raw_text(
                WindowsConptyShutdownResetFilter::BARE_SIMPLE_SEQUENCE,
                TextStream::Stdout,
            );
            if raw_stderr {
                capture.append_raw_text(b"err", TextStream::Stderr);
            } else {
                capture.append_output_text(b"err", TextStream::Stderr, false);
            }
            capture.append_output_text(b"\n", TextStream::Stdout, false);
            capture.finish_raw_text(TextStream::Stdout);
            capture.finalize_windows_conpty_raw_text();

            let reset =
                String::from_utf8(WindowsConptyShutdownResetFilter::BARE_SIMPLE_SEQUENCE.to_vec())
                    .expect("test reset sequence is UTF-8");
            let mut expected = reset.as_bytes().to_vec();
            expected.extend_from_slice(b"err\n");
            assert_eq!(ring_bytes(&output_ring), expected);
        }
    }

    #[test]
    fn windows_conpty_legacy_reset_survives_interleaved_ipc_cleanup() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();

        capture.note_windows_conpty_shutdown_starting();
        capture.append_raw_text(
            WindowsConptyShutdownResetFilter::SIMPLE_SEQUENCE,
            TextStream::Stdout,
        );
        capture.append_output_text(b"cleanup", TextStream::Stdout, false);
        capture.finish_raw_text(TextStream::Stdout);
        capture.finalize_windows_conpty_raw_text();

        assert_eq!(ring_bytes(&output_ring), b"cleanup");
        assert_eq!(
            tape.drain_final_output().contents,
            vec![WorkerContent::worker_stdout("cleanup")]
        );
    }

    #[test]
    fn windows_conpty_legacy_reset_stays_filtered_across_forced_lf_boundary() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();

        capture.append_raw_text(WindowsConptyStartupNoiseFilter::PREFIX, TextStream::Stdout);
        capture.append_raw_text(b"ready", TextStream::Stdout);
        capture.note_windows_conpty_shutdown_starting();
        capture.append_raw_text(
            WindowsConptyShutdownResetFilter::SIMPLE_SEQUENCE,
            TextStream::Stdout,
        );
        capture.append_output_text(b"\n", TextStream::Stdout, false);
        capture.append_raw_text(b"err", TextStream::Stderr);
        capture.finish_raw_text(TextStream::Stdout);
        capture.finalize_windows_conpty_raw_text();

        assert_eq!(ring_bytes(&output_ring), b"ready\nerr");
        assert_eq!(
            tape.drain_final_output().contents,
            vec![
                WorkerContent::worker_stdout("ready\n"),
                WorkerContent::worker_stderr("stderr: err")
            ]
        );
    }

    #[test]
    fn windows_conpty_staged_ipc_lf_precedes_raw_stderr_boundary() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();

        capture.note_windows_conpty_shutdown_starting();
        capture.append_output_text(b"\n", TextStream::Stdout, false);
        capture.append_raw_text(b"err", TextStream::Stderr);
        capture.finish_raw_text(TextStream::Stdout);
        capture.finalize_windows_conpty_raw_text();

        assert_eq!(ring_bytes(&output_ring), b"\nerr");
        assert_eq!(
            tape.drain_final_output().contents,
            vec![
                WorkerContent::worker_stdout("\n"),
                WorkerContent::worker_stderr("stderr: err")
            ]
        );
    }

    #[test]
    fn windows_conpty_flushed_ipc_lf_prevents_later_reverse_pairing() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();

        capture.note_windows_conpty_shutdown_starting();
        capture.append_output_text(b"\n", TextStream::Stdout, false);
        capture.append_raw_text(b"ordinary", TextStream::Stdout);
        capture.append_raw_text(
            WindowsConptyShutdownResetFilter::BARE_SIMPLE_SEQUENCE,
            TextStream::Stdout,
        );
        capture.append_output_text(b"\n", TextStream::Stdout, false);
        capture.finish_raw_text(TextStream::Stdout);
        capture.finalize_windows_conpty_raw_text();

        let mut expected = b"\nordinary".to_vec();
        expected.extend_from_slice(WindowsConptyShutdownResetFilter::BARE_SIMPLE_SEQUENCE);
        expected.extend_from_slice(b"\n");
        assert_eq!(ring_bytes(&output_ring), expected);
        assert_eq!(
            tape.drain_final_output().contents,
            vec![WorkerContent::worker_stdout(
                String::from_utf8(expected).expect("test reset sequence is UTF-8")
            )]
        );
    }

    #[test]
    fn windows_conpty_unarmed_ipc_lf_and_raw_reset_are_preserved() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();

        // A pre-session-end IPC LF is runtime output. Only an already-armed
        // server shutdown may classify a standalone LF as half of a split
        // ConPTY lifecycle frame.
        capture.append_output_text(b"\n", TextStream::Stdout, false);
        capture.append_sideband(PendingSidebandKind::SessionEnd);
        capture.append_raw_text(
            WindowsConptyShutdownResetFilter::BARE_SIMPLE_SEQUENCE,
            TextStream::Stdout,
        );
        capture.finish_raw_text(TextStream::Stdout);
        capture.finalize_windows_conpty_raw_text();

        let mut expected = b"\n".to_vec();
        expected.extend_from_slice(WindowsConptyShutdownResetFilter::BARE_SIMPLE_SEQUENCE);
        assert_eq!(ring_bytes(&output_ring), expected);
        assert_eq!(
            tape.drain_final_output().contents,
            vec![WorkerContent::worker_stdout(
                String::from_utf8(expected).expect("test reset sequence is UTF-8")
            )]
        );
    }

    #[test]
    fn windows_conpty_unarmed_raw_reset_before_ipc_lf_is_preserved_at_session_end() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();

        capture.append_raw_text(
            WindowsConptyShutdownResetFilter::BARE_SIMPLE_SEQUENCE,
            TextStream::Stdout,
        );
        capture.append_output_text(b"\n", TextStream::Stdout, false);
        capture.append_sideband(PendingSidebandKind::SessionEnd);
        capture.finish_raw_text(TextStream::Stdout);
        capture.finalize_windows_conpty_raw_text();

        let mut expected = WindowsConptyShutdownResetFilter::BARE_SIMPLE_SEQUENCE.to_vec();
        expected.extend_from_slice(b"\n");
        assert_eq!(ring_bytes(&output_ring), expected);
        assert_eq!(
            tape.drain_final_output().contents,
            vec![WorkerContent::worker_stdout(
                String::from_utf8(expected).expect("test reset sequence is UTF-8")
            )]
        );
    }

    #[test]
    fn windows_conpty_unarmed_bare_reset_does_not_pair_across_ipc_boundary() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();
        let reset = WindowsConptyShutdownResetFilter::BARE_SIMPLE_SEQUENCE;

        capture.append_raw_text(reset, TextStream::Stdout);
        capture.append_output_text(b"cleanup", TextStream::Stdout, false);
        capture.append_sideband(PendingSidebandKind::SessionEnd);
        capture.append_output_text(b"\n", TextStream::Stdout, false);
        capture.finish_raw_text(TextStream::Stdout);
        capture.finalize_windows_conpty_raw_text();

        let mut expected = reset.to_vec();
        expected.extend_from_slice(b"cleanup\n");
        assert_eq!(ring_bytes(&output_ring), expected);
        assert_eq!(
            tape.drain_final_output().contents,
            vec![WorkerContent::worker_stdout(
                String::from_utf8(expected).expect("test reset sequence is UTF-8")
            )]
        );
    }

    #[test]
    fn windows_conpty_ambiguous_startup_prefix_cannot_pair_across_ipc_boundary() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();
        let reset = WindowsConptyShutdownResetFilter::BARE_SIMPLE_SEQUENCE;

        capture.append_raw_text(&reset[..2], TextStream::Stdout);
        capture.append_output_text(b"cleanup", TextStream::Stdout, false);
        capture.append_sideband(PendingSidebandKind::SessionEnd);
        capture.append_raw_text(&reset[2..], TextStream::Stdout);
        capture.append_output_text(b"\n", TextStream::Stdout, false);
        capture.finish_raw_text(TextStream::Stdout);
        capture.finalize_windows_conpty_raw_text();

        let mut expected = b"cleanup".to_vec();
        expected.extend_from_slice(reset);
        expected.extend_from_slice(b"\n");
        assert_eq!(ring_bytes(&output_ring), expected);
        assert_eq!(
            tape.drain_final_output().contents,
            vec![WorkerContent::worker_stdout(
                String::from_utf8(expected).expect("test reset sequence is UTF-8")
            )]
        );
    }

    #[test]
    fn raw_windows_conpty_multiple_shutdown_resets_are_dropped_until_finalization() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();
        let first_reset = [
            WindowsConptyShutdownResetFilter::TITLED_PREFIX,
            b"C:\\mcp-repl\\target\\debug\\mcp-repl.exe",
            WindowsConptyShutdownResetFilter::TITLED_SUFFIX,
        ]
        .concat();
        let second_reset = WindowsConptyShutdownResetFilter::LF_PREFIXED_SIMPLE_SEQUENCE;
        let mut between_and_second_prefix = b"between".to_vec();
        between_and_second_prefix.extend_from_slice(&second_reset[..1]);

        capture.append_raw_text(b"\x1b[?9001h\x1b[?1004h", TextStream::Stdout);
        capture.append_raw_text(b"ready", TextStream::Stdout);
        capture.note_windows_conpty_shutdown_starting();
        capture.append_raw_text(&first_reset, TextStream::Stdout);
        capture.append_raw_text(&between_and_second_prefix, TextStream::Stdout);
        capture.append_raw_text(&second_reset[1..8], TextStream::Stdout);
        capture.append_raw_text(&second_reset[8..], TextStream::Stdout);
        capture.finish_raw_text(TextStream::Stdout);
        capture.finalize_windows_conpty_raw_text();

        assert_eq!(ring_bytes(&output_ring), b"readybetween");
        assert_eq!(
            tape.drain_final_output().contents,
            vec![WorkerContent::worker_stdout("readybetween")]
        );
    }

    #[test]
    fn raw_windows_conpty_lf_prefixed_shutdown_reset_split_across_session_end_is_dropped() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();
        let reset = WindowsConptyShutdownResetFilter::LF_PREFIXED_SIMPLE_SEQUENCE;

        capture.append_raw_text(b"\x1b[?9001h\x1b[?1004h", TextStream::Stdout);
        capture.append_raw_text(b"ready", TextStream::Stdout);
        capture.append_raw_text(&reset[..1], TextStream::Stdout);
        assert_eq!(ring_bytes(&output_ring), b"ready");

        capture.append_sideband(PendingSidebandKind::SessionEnd);
        capture.append_raw_text(&reset[1..], TextStream::Stdout);
        capture.finish_raw_text(TextStream::Stdout);
        capture.finalize_windows_conpty_raw_text();

        assert_eq!(ring_bytes(&output_ring), b"ready");
        assert_eq!(
            tape.drain_final_output().contents,
            vec![WorkerContent::worker_stdout("ready")]
        );
    }

    #[test]
    fn raw_windows_conpty_unarmed_ambiguous_lf_is_flushed_before_non_session_sideband() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();

        capture.append_raw_text(b"\x1b[?9001h\x1b[?1004h", TextStream::Stdout);
        capture.append_raw_text(b"ready\n", TextStream::Stdout);
        assert_eq!(ring_bytes(&output_ring), b"ready");

        capture.append_sideband(PendingSidebandKind::RequestBoundary);

        assert_eq!(ring_bytes(&output_ring), b"ready\n");
        assert_eq!(
            tape.drain_final_output().contents,
            vec![WorkerContent::worker_stdout("ready\n")]
        );
    }

    #[test]
    fn raw_windows_conpty_ordinary_lf_precedes_deferred_session_end_marker() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();

        capture.append_raw_text(b"\x1b[?9001h\x1b[?1004h", TextStream::Stdout);
        capture.append_raw_text(b"cleanup\n", TextStream::Stdout);
        assert_eq!(ring_bytes(&output_ring), b"cleanup");

        capture.append_sideband(PendingSidebandKind::SessionEnd);
        capture.finish_raw_text(TextStream::Stdout);
        assert_eq!(ring_bytes(&output_ring), b"cleanup");

        capture.finalize_windows_conpty_raw_text();
        capture.append_raw_text(b"after", TextStream::Stdout);

        let range = output_ring.read_range(0, output_ring.end_offset());
        assert_eq!(ring_bytes(&output_ring), b"cleanup\nafter");
        assert!(range.events.iter().any(|event| {
            event.offset == b"cleanup\n".len() as u64
                && matches!(&event.kind, OutputEventKind::SessionEnd)
        }));
        assert_eq!(
            tape.drain_final_output().contents,
            vec![WorkerContent::worker_stdout("cleanup\nafter")]
        );
    }

    #[test]
    fn raw_windows_conpty_session_end_marker_queue_is_idempotent() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();

        capture.append_raw_text(b"\x1b[?9001h\x1b[?1004h", TextStream::Stdout);
        capture.append_raw_text(b"ready", TextStream::Stdout);
        capture.append_sideband(PendingSidebandKind::SessionEnd);
        capture.append_sideband(PendingSidebandKind::SessionEnd);
        capture.finish_raw_text(TextStream::Stdout);
        capture.finalize_windows_conpty_raw_text();

        let range = output_ring.read_range(0, output_ring.end_offset());
        assert_eq!(
            range
                .events
                .iter()
                .filter(|event| matches!(&event.kind, OutputEventKind::SessionEnd))
                .count(),
            1
        );
        assert_eq!(
            tape.drain_final_output().contents,
            vec![WorkerContent::worker_stdout("ready")]
        );
    }

    #[test]
    fn raw_windows_conpty_lf_prefixed_shutdown_reset_handles_split_reads() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();
        let reset = WindowsConptyShutdownResetFilter::LF_PREFIXED_SIMPLE_SEQUENCE;

        capture.append_raw_text(b"\x1b[?9001h\x1b[?1004h", TextStream::Stdout);
        capture.append_raw_text(b"before", TextStream::Stdout);
        capture.note_windows_conpty_shutdown_starting();
        capture.append_raw_text(&reset[..1], TextStream::Stdout);
        capture.append_raw_text(&reset[1..8], TextStream::Stdout);
        capture.append_raw_text(&reset[8..], TextStream::Stdout);
        capture.append_raw_text(b"after", TextStream::Stdout);

        assert_eq!(ring_bytes(&output_ring), b"beforeafter");
        assert_eq!(
            tape.drain_final_output().contents,
            vec![WorkerContent::worker_stdout("beforeafter")]
        );
    }

    #[test]
    fn raw_windows_conpty_lf_prefixed_shutdown_reset_is_preserved_when_unarmed() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();
        let reset = WindowsConptyShutdownResetFilter::LF_PREFIXED_SIMPLE_SEQUENCE;

        capture.append_raw_text(b"\x1b[?9001h\x1b[?1004h", TextStream::Stdout);
        capture.append_raw_text(b"ready", TextStream::Stdout);
        capture.append_raw_text(reset, TextStream::Stdout);
        capture.finish_raw_text(TextStream::Stdout);
        capture.finalize_windows_conpty_raw_text();

        let mut expected = b"ready".to_vec();
        expected.extend_from_slice(reset);
        assert_eq!(ring_bytes(&output_ring), expected);
        assert_eq!(
            tape.drain_final_output().contents,
            vec![WorkerContent::worker_stdout(
                String::from_utf8(expected).expect("test reset sequence is UTF-8")
            )]
        );
    }

    #[test]
    fn raw_windows_conpty_shutdown_reset_is_buffered_until_session_end_arms_filter() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();

        capture.append_raw_text(b"\x1b[?9001h\x1b[?1004h", TextStream::Stdout);
        capture.append_raw_text(b"ready", TextStream::Stdout);
        capture.append_raw_text(
            WindowsConptyShutdownResetFilter::SIMPLE_SEQUENCE,
            TextStream::Stdout,
        );
        assert_eq!(ring_bytes(&output_ring), b"ready");

        capture.note_windows_conpty_shutdown_starting();
        capture.finish_raw_text(TextStream::Stdout);
        capture.finalize_windows_conpty_raw_text();

        assert_eq!(ring_bytes(&output_ring), b"ready");
        assert_eq!(
            tape.drain_final_output().contents,
            vec![WorkerContent::worker_stdout("ready")]
        );
    }

    #[test]
    fn raw_windows_conpty_titled_reset_is_buffered_through_reader_eof_until_session_end() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();
        let reset = [
            WindowsConptyShutdownResetFilter::TITLED_PREFIX,
            b"C:\\mcp-repl\\target\\debug\\mcp-repl.exe",
            WindowsConptyShutdownResetFilter::TITLED_SUFFIX,
        ]
        .concat();

        capture.append_raw_text(b"\x1b[?9001h\x1b[?1004h", TextStream::Stdout);
        capture.append_raw_text(b"ready\n", TextStream::Stdout);
        capture.append_raw_text(&reset, TextStream::Stdout);
        capture.finish_raw_text(TextStream::Stdout);
        assert_eq!(ring_bytes(&output_ring), b"ready\n");

        capture.note_windows_conpty_shutdown_starting();
        capture.finalize_windows_conpty_raw_text();

        assert_eq!(ring_bytes(&output_ring), b"ready\n");
        assert_eq!(
            tape.drain_final_output().contents,
            vec![WorkerContent::worker_stdout("ready\n")]
        );
    }

    #[test]
    fn raw_windows_conpty_shutdown_reset_filter_preserves_surrounding_split_output() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();
        let reset = [
            WindowsConptyShutdownResetFilter::TITLED_PREFIX,
            b"C:\\mcp-repl\\target\\debug\\mcp-repl.exe",
            WindowsConptyShutdownResetFilter::TITLED_SUFFIX,
        ]
        .concat();

        capture.append_raw_text(b"\x1b[?9001h\x1b[?1004h", TextStream::Stdout);
        capture.append_raw_text(b"before\n", TextStream::Stdout);
        capture.note_windows_conpty_shutdown_starting();
        capture.append_raw_text(b"shutdown-output", TextStream::Stdout);
        capture.append_raw_text(&reset[..11], TextStream::Stdout);
        capture.append_raw_text(&reset[11..27], TextStream::Stdout);
        capture.append_raw_text(&reset[27..reset.len() - 4], TextStream::Stdout);
        capture.append_raw_text(&reset[reset.len() - 4..], TextStream::Stdout);
        capture.append_raw_text(b"after\n", TextStream::Stdout);

        assert_eq!(ring_bytes(&output_ring), b"before\nshutdown-outputafter");
        capture.finish_raw_text(TextStream::Stdout);
        capture.finalize_windows_conpty_raw_text();

        assert_eq!(ring_bytes(&output_ring), b"before\nshutdown-outputafter\n");
        assert_eq!(
            tape.drain_final_output().contents,
            vec![WorkerContent::worker_stdout(
                "before\nshutdown-outputafter\n"
            )]
        );
    }

    #[test]
    fn raw_windows_conpty_shutdown_reset_is_preserved_before_shutdown_starts() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();

        capture.append_raw_text(b"\x1b[?9001h\x1b[?1004h", TextStream::Stdout);
        capture.append_raw_text(b"ready", TextStream::Stdout);
        capture.append_raw_text(
            WindowsConptyShutdownResetFilter::SIMPLE_SEQUENCE,
            TextStream::Stdout,
        );

        assert_eq!(ring_bytes(&output_ring), b"ready");
        capture.finish_raw_text(TextStream::Stdout);
        capture.finalize_windows_conpty_raw_text();

        let mut expected = b"ready".to_vec();
        expected.extend_from_slice(WindowsConptyShutdownResetFilter::SIMPLE_SEQUENCE);
        assert_eq!(ring_bytes(&output_ring), expected);
        assert_eq!(
            tape.drain_final_output().contents,
            vec![WorkerContent::worker_stdout(
                String::from_utf8(expected).expect("test reset sequence is UTF-8")
            )]
        );
    }

    #[test]
    fn raw_windows_conpty_shutdown_filter_flushes_unmatched_partial_sequence() {
        let (capture, output_ring, _) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();
        let partial = &WindowsConptyShutdownResetFilter::SIMPLE_SEQUENCE[..9];

        capture.append_raw_text(b"\x1b[?9001h\x1b[?1004h", TextStream::Stdout);
        capture.append_raw_text(b"ready", TextStream::Stdout);
        capture.note_windows_conpty_shutdown_starting();
        capture.append_raw_text(partial, TextStream::Stdout);
        capture.finish_raw_text(TextStream::Stdout);

        let mut expected = b"ready".to_vec();
        expected.extend_from_slice(partial);
        assert_eq!(ring_bytes(&output_ring), expected);
    }

    #[test]
    fn windows_conpty_env_merge_overrides_case_insensitive_names() {
        let mut env_map = std::collections::HashMap::from([
            ("Path".to_string(), "old-path".to_string()),
            ("Temp".to_string(), "old-temp".to_string()),
        ]);

        apply_command_env_overrides_for_windows_conpty(
            &mut env_map,
            [
                ("PATH".to_string(), Some("new-path".to_string())),
                ("temp".to_string(), None),
            ],
        );

        assert_eq!(env_map.get("PATH"), Some(&"new-path".to_string()));
        assert!(
            !env_map.contains_key("Path"),
            "PATH override should replace inherited Path entry"
        );
        assert!(
            !env_map.keys().any(|key| key.eq_ignore_ascii_case("temp")),
            "temp removal should remove inherited Temp entry"
        );
    }

    #[test]
    fn files_output_capture_anchors_update_notice_before_late_prompt_shaped_text() {
        let output_ring = Arc::new(OutputRing::with_capacity(OUTPUT_RING_CAPACITY_BYTES));
        let timeline = OutputTimeline::new(output_ring);
        let tape = PendingOutputTape::with_timeline(timeline.clone());
        let capture = LiveOutputCapture::new(OversizedOutputMode::Files, timeline);

        capture.append_sideband(PendingSidebandKind::ReadlineResult {
            prompt: "> ".to_string(),
            line: "lines(4:8, 4:8)\n".to_string(),
        });
        capture.append_image(IpcOutputImage {
            id: "img-1".to_string(),
            data: "AA==".to_string(),
            mime_type: "image/png".to_string(),
            is_new: true,
            updates_previous_image: true,
            readline_results_seen: 1,
        });
        capture.append_raw_text(b"> lines(4:8, 4:8)\n", TextStream::Stdout);

        let contents = tape.drain_final_output().contents;

        assert_eq!(
            contents,
            vec![
                WorkerContent::worker_stdout_transcript_only("> lines(4:8, 4:8)\n"),
                WorkerContent::server_stdout(PREVIOUS_IMAGE_UPDATE_NOTICE),
                WorkerContent::ContentImage {
                    data: "AA==".to_string(),
                    mime_type: "image/png".to_string(),
                    id: "img-1".to_string(),
                    is_new: true,
                },
                WorkerContent::worker_stdout("> lines(4:8, 4:8)\n"),
            ]
        );
    }

    #[test]
    fn files_ipc_output_text_appends_to_tape_and_timeline_in_ipc_order() {
        let output_ring = Arc::new(OutputRing::with_capacity(OUTPUT_RING_CAPACITY_BYTES));
        let timeline = OutputTimeline::new(output_ring.clone());
        let tape = PendingOutputTape::with_timeline(timeline.clone());
        let capture = LiveOutputCapture::new(OversizedOutputMode::Files, timeline);
        let (done_tx, done_rx) = mpsc::channel();

        let output_capture = capture.clone();
        let wait_capture = capture.clone();
        let result_capture = capture.clone();
        let image_capture = capture.clone();
        let session_capture = capture.clone();
        let (server, worker) = crate::ipc::test_connection_pair_with_handlers(IpcHandlers {
            on_output_text: Some(Arc::new(move |text| {
                output_capture.append_output_text(&text.bytes, text.stream, text.is_continuation);
            })),
            on_input_wait: Some(Arc::new(move |prompt| {
                wait_capture.append_sideband(PendingSidebandKind::InputWait { prompt });
            })),
            on_input_line: Some(Arc::new(move |event| {
                result_capture.append_sideband(PendingSidebandKind::ReadlineResult {
                    prompt: event.prompt,
                    line: event.line,
                });
            })),
            on_output_image: Some(Arc::new(move |image| {
                image_capture.append_image(image);
            })),
            on_session_end: Some(Arc::new(move || {
                session_capture.append_sideband(PendingSidebandKind::SessionEnd);
                done_tx.send(()).expect("send session end marker");
            })),
        })
        .expect("ipc pair");

        worker
            .send(WorkerToServerIpcMessage::InputWait {
                prompt: "> ".to_string(),
            })
            .expect("send initial input_wait");
        server
            .wait_for_input_wait(Duration::from_millis(200))
            .expect("server observed initial input_wait");
        server.begin_input().expect("server starts input");
        worker
            .send(WorkerToServerIpcMessage::OutputText {
                stream: TextStream::Stdout,
                data_b64: base64::engine::general_purpose::STANDARD.encode(b"before\n"),
                is_continuation: false,
            })
            .expect("send stdout output_text");
        worker
            .send(WorkerToServerIpcMessage::InputLine {
                prompt: "> ".to_string(),
                text: "plot(1)\n".to_string(),
            })
            .expect("send input_line");
        worker
            .send(WorkerToServerIpcMessage::OutputImage {
                mime_type: "image/png".to_string(),
                data_b64: "AA==".to_string(),
                is_update: false,
                source: None,
            })
            .expect("send output_image");
        worker
            .send(WorkerToServerIpcMessage::OutputText {
                stream: TextStream::Stderr,
                data_b64: base64::engine::general_purpose::STANDARD.encode(b"err\n"),
                is_continuation: false,
            })
            .expect("send stderr output_text");
        worker
            .send(WorkerToServerIpcMessage::InputWait {
                prompt: "> ".to_string(),
            })
            .expect("send completion input_wait");
        worker
            .send(WorkerToServerIpcMessage::SessionEnd {
                reason: None,
                message: None,
            })
            .expect("send session_end");

        done_rx
            .recv_timeout(Duration::from_millis(200))
            .expect("server IPC consumed session_end");

        let end = output_ring.end_offset();
        let range = output_ring.read_range(0, end);
        assert_eq!(range.bytes, b"before\nerr\n");
        let image_event = range
            .events
            .iter()
            .find_map(|event| match &event.kind {
                OutputEventKind::Image { id, mime_type, .. } => Some((event.offset, id, mime_type)),
                _ => None,
            })
            .expect("timeline image event");
        assert_eq!(image_event.0, b"before\n".len() as u64);
        assert!(image_event.1.starts_with("image-"));
        assert_eq!(image_event.2, "image/png");

        let output = tape.drain_final_output();
        assert_eq!(output.contents.len(), 4);
        assert_eq!(output.contents[0], WorkerContent::worker_stdout("before\n"));
        assert_eq!(
            output.contents[1],
            WorkerContent::worker_stdout_transcript_only("> plot(1)\n")
        );
        assert!(
            matches!(
                &output.contents[2],
                WorkerContent::ContentImage {
                    data,
                    mime_type,
                    id,
                    is_new: true,
                } if data == "AA==" && mime_type == "image/png" && id.starts_with("image-")
            ),
            "expected generated image event in output order, got: {:?}",
            output.contents[2]
        );
        assert_eq!(
            output.contents[3],
            WorkerContent::worker_stderr("stderr: err\n")
        );
    }

    #[test]
    fn pager_output_capture_preserves_update_notice_image_and_late_raw_text() {
        let output_ring = Arc::new(OutputRing::with_capacity(OUTPUT_RING_CAPACITY_BYTES));
        let capture = LiveOutputCapture::new(
            OversizedOutputMode::Pager,
            OutputTimeline::new(output_ring.clone()),
        );

        capture.append_sideband(PendingSidebandKind::ReadlineResult {
            prompt: "> ".to_string(),
            line: "lines(4:8, 4:8)\n".to_string(),
        });
        capture.append_image(IpcOutputImage {
            id: "img-1".to_string(),
            data: "AA==".to_string(),
            mime_type: "image/png".to_string(),
            is_new: true,
            updates_previous_image: true,
            readline_results_seen: 1,
        });
        capture.append_raw_text(b"> lines(4:8, 4:8)\n", TextStream::Stdout);

        let end = output_ring.end_offset();
        let contents = crate::pager::contents_from_output_range(output_ring.read_range(0, end));

        assert_eq!(
            contents,
            vec![
                WorkerContent::worker_stdout_transcript_only("> lines(4:8, 4:8)\n"),
                WorkerContent::server_stdout(PREVIOUS_IMAGE_UPDATE_NOTICE),
                WorkerContent::ContentImage {
                    data: "AA==".to_string(),
                    mime_type: "image/png".to_string(),
                    id: "img-1".to_string(),
                    is_new: true,
                },
                WorkerContent::worker_stdout("> lines(4:8, 4:8)\n"),
            ]
        );
    }

    #[cfg(target_family = "windows")]
    #[test]
    fn windows_ipc_connect_error_reaps_worker_process() {
        let mut child = WorkerChild::standard(sleeping_test_child());

        let result = handle_windows_ipc_connect_result(
            Err(std::io::Error::other("ipc connect failed")),
            &mut child,
        );
        assert!(matches!(result, Err(WorkerError::Io(_))));

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let status = child.try_wait().expect("query child status");
            if status.is_some() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("connect-error handler should reap child wrapper");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(target_family = "windows")]
    #[test]
    fn windows_soft_termination_does_not_kill_child() {
        let mut child = WorkerChild::standard(sleeping_test_child());

        request_soft_termination(&mut child).expect("soft terminate call should succeed");

        let status = child.try_wait().expect("query child status");
        assert!(
            status.is_none(),
            "child should still be running after soft termination request"
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(target_family = "windows")]
    #[test]
    fn windows_ipc_connect_timeout_is_bounded() {
        assert!(
            WINDOWS_IPC_CONNECT_MAX_WAIT <= Duration::from_secs(10),
            "windows IPC connect max wait should fail fast, got {:?}",
            WINDOWS_IPC_CONNECT_MAX_WAIT
        );
    }
}
