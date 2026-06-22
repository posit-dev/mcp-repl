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
use crate::pending_output_tape::{PendingOutputTape, PendingSidebandKind};
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
use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
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
    pending_output_tape: Option<PendingOutputTape>,
    output_timeline: OutputTimeline,
    #[cfg(any(test, target_os = "windows"))]
    drop_windows_conpty_startup_noise_before_input: Option<Arc<AtomicBool>>,
}

impl LiveOutputCapture {
    pub(crate) fn new(
        oversized_output: OversizedOutputMode,
        pending_output_tape: PendingOutputTape,
        output_timeline: OutputTimeline,
    ) -> Self {
        Self {
            pending_output_tape: matches!(oversized_output, OversizedOutputMode::Files)
                .then_some(pending_output_tape),
            output_timeline,
            #[cfg(any(test, target_os = "windows"))]
            drop_windows_conpty_startup_noise_before_input: None,
        }
    }

    #[cfg(any(test, target_os = "windows"))]
    fn with_windows_conpty_startup_noise_filter(mut self) -> Self {
        self.drop_windows_conpty_startup_noise_before_input = Some(Arc::new(AtomicBool::new(true)));
        self
    }

    pub(crate) fn append_output_text(
        &self,
        bytes: &[u8],
        stream: TextStream,
        is_continuation: bool,
    ) {
        self.append_text(bytes, stream, is_continuation, true);
    }

    fn append_raw_text(&self, bytes: &[u8], stream: TextStream) {
        #[cfg(any(test, target_os = "windows"))]
        if matches!(stream, TextStream::Stdout)
            && self.should_drop_windows_conpty_startup_noise(bytes)
        {
            return;
        }
        self.append_text(bytes, stream, false, false);
    }

    #[cfg(any(test, target_os = "windows"))]
    fn note_accepted_input_starting(&self) {
        let Some(drop_startup_noise) = &self.drop_windows_conpty_startup_noise_before_input else {
            return;
        };
        drop_startup_noise.store(false, Ordering::Relaxed);
    }

    #[cfg(not(any(test, target_os = "windows")))]
    fn note_accepted_input_starting(&self) {}

    #[cfg(any(test, target_os = "windows"))]
    fn should_drop_windows_conpty_startup_noise(&self, bytes: &[u8]) -> bool {
        let Some(drop_startup_noise) = &self.drop_windows_conpty_startup_noise_before_input else {
            return false;
        };
        if !drop_startup_noise.load(Ordering::Relaxed) {
            return false;
        }

        // Windows ConPTY emits these terminal-mode toggles on its raw output
        // stream during startup. They are not Python output and do not come over
        // sideband. We only drop this exact standalone pre-input noise; after an
        // accepted input starts, raw output might be runtime/user output and is
        // passed through unchanged.
        windows_conpty_startup_noise_only(bytes)
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
                if let Some(tape) = &self.pending_output_tape {
                    if is_output_text {
                        tape.append_stdout_ipc_bytes(bytes);
                    } else {
                        tape.append_stdout_bytes(bytes);
                    }
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
                if let Some(tape) = &self.pending_output_tape {
                    if is_output_text {
                        tape.append_stderr_ipc_bytes(bytes);
                    } else {
                        tape.append_stderr_bytes(bytes);
                    }
                }
            }
        }
    }

    pub(crate) fn append_image(&self, image: IpcOutputImage) {
        if image.updates_previous_image {
            self.output_timeline.append_text_event(
                PREVIOUS_IMAGE_UPDATE_NOTICE.to_string(),
                false,
                ContentOrigin::Server,
                Some(image.readline_results_seen),
            );
            if let Some(tape) = &self.pending_output_tape {
                tape.append_stdout_status_event(
                    PREVIOUS_IMAGE_UPDATE_NOTICE.to_string(),
                    image.readline_results_seen,
                );
            }
        }
        self.output_timeline.append_image(
            image.id.clone(),
            image.mime_type.clone(),
            image.data.clone(),
            image.is_new,
            image.readline_results_seen,
        );
        if let Some(tape) = &self.pending_output_tape {
            tape.append_image(
                image.id,
                image.mime_type,
                image.data,
                image.is_new,
                image.readline_results_seen,
            );
        }
    }

    pub(crate) fn append_sideband(&self, kind: PendingSidebandKind) {
        if let Some(tape) = &self.pending_output_tape {
            tape.append_sideband(kind);
        }
    }
}

#[cfg(any(test, target_os = "windows"))]
fn windows_conpty_startup_noise_only(bytes: &[u8]) -> bool {
    const STARTUP_NOISE: &[u8] = b"\x1b[?9001h\x1b[?1004h";
    let Some(rest) = bytes.strip_prefix(STARTUP_NOISE) else {
        return false;
    };
    rest.iter().all(|byte| matches!(byte, b'\r' | b'\n'))
}

#[cfg(target_family = "unix")]
const WORKER_MEM_GUARDRAIL_RATIO: f64 = 0.75;
#[cfg(target_family = "unix")]
const WORKER_MEM_GUARDRAIL_ACTIVE_INTERVAL: Duration = Duration::from_secs(10);
#[cfg(target_family = "unix")]
const WORKER_MEM_GUARDRAIL_IDLE_INTERVAL: Duration = Duration::from_secs(60);

const WORKER_READY_TIMEOUT: Duration = Duration::from_secs(10);
const WORKER_SESSION_END_RESPAWN_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
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
    session_tmpdir: Option<PathBuf>,
    ipc: IpcHandle,
    live_output: LiveOutputCapture,
    stdout_reader: Option<OutputReader>,
    stderr_reader: Option<OutputReader>,
    expected_exit: bool,
    exit_status: Option<std::process::ExitStatus>,
    #[cfg(target_family = "unix")]
    guardrail_stop: Arc<AtomicBool>,
    #[cfg(target_family = "unix")]
    guardrail_thread: Option<std::thread::JoinHandle<()>>,
    #[cfg(target_family = "unix")]
    guardrail_thread_handle: Option<std::thread::Thread>,
    #[cfg(target_os = "macos")]
    denial_logger: Option<crate::sandbox::DenialLogger>,
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

    fn id(&self) -> u32 {
        match self {
            Self::Standard(child) => child.id(),
            #[cfg(target_family = "windows")]
            Self::DirectWindows(child) => child.id(),
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
}

#[cfg(target_family = "windows")]
struct WindowsProcess {
    process: HANDLE,
    thread: HANDLE,
    process_id: u32,
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
            process_id: proc_info.dwProcessId,
            job,
            _conpty: conpty,
        }
    }

    fn id(&self) -> u32 {
        self.process_id
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
    pub(crate) pending_output_tape: PendingOutputTape,
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

    fn stop_now_and_join(mut self, panic_message: &'static str) -> Result<(), WorkerError> {
        self.request_stop();
        let _ = self.done_rx.recv();
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
        let WorkerSpawnContext {
            oversized_output,
            pending_output_tape,
            output_timeline,
            guardrail,
            managed_network_proxy,
            #[cfg(target_os = "windows")]
            prepared_windows_launch,
        } = context;

        #[cfg(not(target_family = "unix"))]
        let _ = &guardrail;

        let mut ipc_server = IpcServer::bind().map_err(WorkerError::Io)?;
        let live_output = LiveOutputCapture::new(
            oversized_output,
            pending_output_tape.clone(),
            output_timeline.clone(),
        );
        #[cfg(target_os = "windows")]
        let live_output = if matches!(&worker_launch, WorkerLaunch::Builtin(Backend::Python)) {
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

        Ok(Self {
            child,
            stdin_tx,
            session_tmpdir,
            ipc,
            live_output,
            stdout_reader,
            stderr_reader,
            expected_exit: false,
            exit_status: None,
            #[cfg(target_family = "unix")]
            guardrail_stop,
            #[cfg(target_family = "unix")]
            guardrail_thread: Some(guardrail_thread),
            #[cfg(target_family = "unix")]
            guardrail_thread_handle: Some(guardrail_thread_handle),
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
        #[cfg(target_family = "unix")]
        {
            self.send_signal(libc::SIGINT)
        }
        #[cfg(target_family = "windows")]
        {
            self.send_windows_ctrl_break()
        }
        #[cfg(not(any(target_family = "unix", target_family = "windows")))]
        {
            Ok(())
        }
    }

    #[cfg(target_family = "windows")]
    fn send_windows_ctrl_break(&mut self) -> Result<(), WorkerError> {
        if self.child.try_wait()?.is_some() {
            return Ok(());
        }
        let ok = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, self.child.id()) };
        if ok != 0 {
            return Ok(());
        }

        match self.child.try_wait()? {
            Some(_) => Ok(()),
            None => Err(WorkerError::Io(std::io::Error::last_os_error())),
        }
    }

    #[cfg(target_family = "windows")]
    pub(crate) fn send_r_interrupt(&mut self) -> Result<(), WorkerError> {
        self.send_windows_ctrl_break()
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
    fn send_signal_descendants_only(&self, signal: i32) {
        let root = Pid::from_u32(self.child.id());
        let mut system = System::new();
        system.refresh_processes(ProcessesToUpdate::All, true);
        for pid in collect_process_tree_pids(&system, root) {
            if pid == root {
                continue;
            }
            let _ = raw_unix_kill(pid.as_u32() as i32, signal);
        }
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
        self.request_ipc_shutdown();
        let _ = self.close_stdin(Duration::from_millis(200));

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
        if self.exit_status.is_none() {
            match self.child.try_wait()? {
                Some(status) => self.exit_status = Some(status),
                None => {
                    self.quiesce_raw_output_readers()?;
                    // The next spawn resets and reuses this stable session temp path.
                    // The old background reaper must not remove the respawned worker's TMPDIR.
                    self.session_tmpdir = None;
                    let _ = thread::Builder::new()
                        .name("worker-session-end-reaper".to_string())
                        .spawn(move || {
                            let _ =
                                self.shutdown_graceful(WORKER_SESSION_END_RESPAWN_SHUTDOWN_TIMEOUT);
                        });
                    return Ok(());
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
        }
        self.quiesce_raw_output_readers()?;
        self.detach_ipc_reader();
        self.cleanup_session_tmpdir();
        self.report_denials();
        Ok(())
    }

    fn finalize_terminated_process(&mut self) -> Result<(), WorkerError> {
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
        self.cleanup_session_tmpdir();
        self.report_denials();
        Ok(())
    }

    fn detach_ipc_reader(&mut self) {
        if let Some(ipc) = self.ipc.get() {
            ipc.detach_reader_thread();
        }
    }

    fn quiesce_raw_output_readers(&mut self) -> Result<(), WorkerError> {
        if let Some(reader) = self.stdout_reader.take() {
            reader.stop_now_and_join("worker stdout reader thread panicked")?;
        }
        if let Some(reader) = self.stderr_reader.take() {
            reader.stop_now_and_join("worker stderr reader thread panicked")?;
        }
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
        if let Some(reader) = self.stdout_reader.take() {
            reader.stop_and_join("worker stdout reader thread panicked")?;
        }
        if let Some(reader) = self.stderr_reader.take() {
            reader.stop_and_join("worker stderr reader thread panicked")?;
        }
        if let Some(ipc) = self.ipc.get() {
            ipc.join_reader_thread().map_err(WorkerError::Io)?;
        }
        Ok(())
    }

    fn cleanup_session_tmpdir(&self) {
        let Some(path) = self.session_tmpdir.as_ref() else {
            return;
        };
        if !path.is_absolute() || path.as_path() == std::path::Path::new("/") {
            return;
        }
        cleanup_worker_session_tmpdir(
            path,
            crate::debug_logs::log_path(crate::diagnostics::WORKER_STARTUP_LOG_FILE_NAME),
        );
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
            session_tmpdir: None,
            ipc: IpcHandle::new(),
            live_output: LiveOutputCapture::new(
                OversizedOutputMode::Files,
                PendingOutputTape::new(),
                OutputTimeline::new(Arc::new(crate::output_capture::OutputRing::with_capacity(
                    crate::output_capture::OUTPUT_RING_CAPACITY_BYTES,
                ))),
            ),
            stdout_reader: None,
            stderr_reader: None,
            expected_exit: false,
            exit_status: None,
            #[cfg(target_family = "unix")]
            guardrail_stop: Arc::new(AtomicBool::new(false)),
            #[cfg(target_family = "unix")]
            guardrail_thread: None,
            #[cfg(target_family = "unix")]
            guardrail_thread_handle: None,
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
    if prepared_args
        .first()
        .is_some_and(|arg| arg == "--windows-sandbox")
    {
        command.env(crate::windows_conpty::WINDOWS_CONPTY_REQUEST_ENV, "1");
        WorkerStdinTransport::Pipe
    } else {
        stdin_transport
    }
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
                    let _ = reply.send(result);
                    break;
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
    use crate::pending_output_tape::{PendingOutputEvent, PendingOutputTape};
    use crate::worker_protocol::WorkerContent;
    use base64::Engine as _;
    use std::sync::{Mutex, OnceLock};

    fn env_test_mutex() -> &'static Mutex<()> {
        static TEST_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
        TEST_MUTEX.get_or_init(|| Mutex::new(()))
    }

    fn capture_with_ring(
        oversized_output: OversizedOutputMode,
    ) -> (LiveOutputCapture, Arc<OutputRing>, PendingOutputTape) {
        let output_ring = Arc::new(OutputRing::with_capacity(OUTPUT_RING_CAPACITY_BYTES));
        let tape = PendingOutputTape::new();
        let capture = LiveOutputCapture::new(
            oversized_output,
            tape.clone(),
            OutputTimeline::new(output_ring.clone()),
        );
        (capture, output_ring, tape)
    }

    fn ring_bytes(output_ring: &OutputRing) -> Vec<u8> {
        let output_end = output_ring.end_offset();
        output_ring.read_range(0, output_end).bytes
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

    #[test]
    fn pager_output_capture_skips_pending_output_tape() {
        let output_ring = Arc::new(OutputRing::with_capacity(OUTPUT_RING_CAPACITY_BYTES));
        let tape = PendingOutputTape::new();
        let capture = LiveOutputCapture::new(
            OversizedOutputMode::Pager,
            tape.clone(),
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

        assert!(
            tape.drain_final_snapshot().events.is_empty(),
            "pager mode should not mirror text, images, or sideband events into the pending tape"
        );
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
            tape.drain_final_snapshot()
                .format_contents_for_reply()
                .contents
                .is_empty(),
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
            tape.drain_final_snapshot()
                .format_contents_for_reply()
                .contents,
            vec![WorkerContent::worker_stdout("\u{1b}[?9001h\u{1b}[?1004h")]
        );
    }

    #[test]
    fn raw_terminal_mode_toggles_are_preserved_after_input_starts() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();

        capture.note_accepted_input_starting();
        capture.append_raw_text(b"\x1b[?9001h\x1b[?1004h", TextStream::Stdout);

        assert_eq!(ring_bytes(&output_ring), b"\x1b[?9001h\x1b[?1004h");
        assert_eq!(
            tape.drain_final_snapshot()
                .format_contents_for_reply()
                .contents,
            vec![WorkerContent::worker_stdout("\u{1b}[?9001h\u{1b}[?1004h")]
        );
    }

    #[test]
    fn raw_windows_conpty_startup_filter_preserves_mixed_output() {
        let (capture, output_ring, tape) = capture_with_ring(OversizedOutputMode::Files);
        let capture = capture.with_windows_conpty_startup_noise_filter();

        capture.append_raw_text(b"\x1b[?9001hvisible\n", TextStream::Stdout);

        assert_eq!(ring_bytes(&output_ring), b"\x1b[?9001hvisible\n");
        assert_eq!(
            tape.drain_final_snapshot()
                .format_contents_for_reply()
                .contents,
            vec![WorkerContent::worker_stdout("\u{1b}[?9001hvisible\n")]
        );
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
        let tape = PendingOutputTape::new();
        let capture = LiveOutputCapture::new(
            OversizedOutputMode::Files,
            tape.clone(),
            OutputTimeline::new(output_ring),
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

        let contents = tape
            .drain_final_snapshot()
            .format_contents_for_reply()
            .contents;

        assert_eq!(
            contents,
            vec![
                WorkerContent::server_stdout(PREVIOUS_IMAGE_UPDATE_NOTICE),
                WorkerContent::ContentImage {
                    data: "AA==".to_string(),
                    mime_type: "image/png".to_string(),
                    id: "img-1".to_string(),
                    is_new: true,
                },
                WorkerContent::stdout("> lines(4:8, 4:8)\n"),
            ]
        );
    }

    #[test]
    fn files_ipc_output_text_appends_to_tape_and_timeline_in_ipc_order() {
        let output_ring = Arc::new(OutputRing::with_capacity(OUTPUT_RING_CAPACITY_BYTES));
        let tape = PendingOutputTape::new();
        let capture = LiveOutputCapture::new(
            OversizedOutputMode::Files,
            tape.clone(),
            OutputTimeline::new(output_ring.clone()),
        );
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

        let snapshot = tape.drain_final_snapshot();
        assert_eq!(snapshot.events.len(), 7);
        assert!(matches!(
            &snapshot.events[0],
            PendingOutputEvent::Sideband {
                kind: PendingSidebandKind::InputWait { prompt },
                ..
            } if prompt == "> "
        ));
        assert!(matches!(
            &snapshot.events[1],
            PendingOutputEvent::TextFragment {
                stream: TextStream::Stdout,
                origin: ContentOrigin::Worker,
                bytes,
                ..
            } if bytes == b"before\n"
        ));
        assert!(matches!(
            &snapshot.events[2],
            PendingOutputEvent::Sideband {
                kind: PendingSidebandKind::ReadlineResult { prompt, line, .. },
                ..
            } if prompt == "> " && line == "plot(1)\n"
        ));
        assert!(matches!(
            &snapshot.events[3],
            PendingOutputEvent::Image {
                id,
                mime_type,
                readline_results_seen: 1,
                ..
            } if id.starts_with("image-") && mime_type == "image/png"
        ));
        assert!(matches!(
            &snapshot.events[4],
            PendingOutputEvent::TextFragment {
                stream: TextStream::Stderr,
                origin: ContentOrigin::Worker,
                bytes,
                ..
            } if bytes == b"err\n"
        ));
        assert!(matches!(
            &snapshot.events[5],
            PendingOutputEvent::Sideband {
                kind: PendingSidebandKind::InputWait { prompt },
                ..
            } if prompt == "> "
        ));
        assert!(matches!(
            &snapshot.events[6],
            PendingOutputEvent::Sideband {
                kind: PendingSidebandKind::SessionEnd,
                ..
            }
        ));

        let end = output_ring.end_offset();
        let range = output_ring.read_range(0, end);
        assert_eq!(range.bytes, b"before\n\nstderr: err\n");
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
    }

    #[test]
    fn pager_output_capture_preserves_update_notice_image_and_late_raw_text() {
        let output_ring = Arc::new(OutputRing::with_capacity(OUTPUT_RING_CAPACITY_BYTES));
        let capture = LiveOutputCapture::new(
            OversizedOutputMode::Pager,
            PendingOutputTape::new(),
            OutputTimeline::new(output_ring.clone()),
        );

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
