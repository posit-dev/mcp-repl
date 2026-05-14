use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use crate::ipc::{
    ServerToWorkerIpcMessage, connect_from_env, emit_session_end, emit_stdin_write_ack,
    set_global_ipc,
};
use crate::python_session::{self, PythonSession};

struct WorkerState {
    busy: AtomicBool,
    shutting_down: AtomicBool,
}

impl WorkerState {
    fn try_mark_busy(&self) -> bool {
        if self.is_shutting_down() {
            return false;
        }
        self.busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    fn mark_idle(&self) {
        self.busy.store(false, Ordering::SeqCst);
    }

    fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
    }

    fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }
}

impl Default for WorkerState {
    fn default() -> Self {
        Self {
            busy: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
        }
    }
}

struct QueuedRequest {
    byte_len: usize,
    line_count: usize,
    final_prompt: Option<String>,
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    crate::diagnostics::startup_log("python-worker: run begin");
    let state = Arc::new(WorkerState::default());
    let (request_tx, request_rx) = mpsc::sync_channel(1);
    init_ipc(state.clone(), request_tx.clone()).map_err(|err| {
        eprintln!("python worker ipc init error: {err}");
        err
    })?;

    let request_state = state.clone();
    let _request_thread = thread::Builder::new()
        .name("python-worker-requests".to_string())
        .spawn(move || request_loop(request_rx, request_state))
        .map_err(|err| format!("failed to spawn Python worker request thread: {err}"))?;

    crate::diagnostics::startup_log("python-worker: starting Python session");
    if let Err(err) = PythonSession::start_on_current_thread() {
        eprintln!("failed to start Python session: {err}");
        return Err(std::io::Error::other(err).into());
    }
    crate::diagnostics::startup_log("python-worker: Python session exited");

    Ok(())
}

fn wait_for_python_session() -> Result<&'static PythonSession, String> {
    loop {
        if let Ok(session) = PythonSession::global() {
            return Ok(session);
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn init_ipc(
    state: Arc<WorkerState>,
    request_tx: mpsc::SyncSender<QueuedRequest>,
) -> Result<(), Box<dyn std::error::Error>> {
    let conn = connect_from_env(Duration::from_secs(2))?;
    set_global_ipc(conn.clone());
    if let Err(err) = thread::Builder::new()
        .name("python-worker-ipc".to_string())
        .spawn(move || {
            loop {
                match conn.recv(None) {
                    Some(ServerToWorkerIpcMessage::RequestStart) => {
                        python_session::mark_request_started();
                        emit_stdin_write_ack();
                    }
                    Some(ServerToWorkerIpcMessage::StdinWrite {
                        byte_len,
                        line_count,
                        final_prompt,
                    }) => {
                        handle_write_stdin(
                            byte_len,
                            line_count,
                            final_prompt,
                            state.clone(),
                            &request_tx,
                        );
                    }
                    Some(ServerToWorkerIpcMessage::StdinWriteComplete) => {
                        python_session::mark_stdin_write_complete();
                    }
                    Some(ServerToWorkerIpcMessage::Interrupt) => {
                        python_session::interrupt();
                    }
                    Some(ServerToWorkerIpcMessage::SessionEnd) => {
                        state.begin_shutdown();
                        let _ = python_session::request_shutdown();
                    }
                    None => {
                        std::process::exit(0);
                    }
                }
            }
        })
    {
        eprintln!("python worker ipc thread error: {err}");
    }
    Ok(())
}

fn request_loop(rx: mpsc::Receiver<QueuedRequest>, state: Arc<WorkerState>) {
    for request in rx {
        let result =
            write_stdin_request(request.byte_len, request.line_count, request.final_prompt);
        if let Err(err) = result {
            emit_stderr_message(&err.message);
            emit_session_end();
        }
        state.mark_idle();
    }
}

fn handle_write_stdin(
    byte_len: usize,
    line_count: usize,
    final_prompt: Option<String>,
    state: Arc<WorkerState>,
    request_tx: &mpsc::SyncSender<QueuedRequest>,
) {
    if state.is_shutting_down() {
        return;
    }

    if !state.try_mark_busy() {
        emit_stderr_message("worker is busy; request already running");
        return;
    }

    if let Err(err) = request_tx.try_send(QueuedRequest {
        byte_len,
        line_count,
        final_prompt,
    }) {
        state.mark_idle();
        let message = match err {
            mpsc::TrySendError::Full(_) => "worker is busy; request already running".to_string(),
            mpsc::TrySendError::Disconnected(_) => {
                "worker execution thread exited unexpectedly".to_string()
            }
        };
        emit_stderr_message(&message);
        emit_session_end();
    }
}

struct WorkerExecError {
    message: String,
}

impl WorkerExecError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

fn write_stdin_request(
    byte_len: usize,
    line_count: usize,
    final_prompt: Option<String>,
) -> Result<(), WorkerExecError> {
    let session = wait_for_python_session()
        .map_err(|err| WorkerExecError::new(format!("failed to start Python session: {err}")))?;
    let reply_rx = session
        .begin_request(byte_len, line_count, final_prompt)
        .map_err(WorkerExecError::new)?;
    emit_stdin_write_ack();
    reply_rx
        .recv()
        .map(|_| ())
        .map_err(|err| WorkerExecError::new(format!("Python session reply error: {err}")))
}

fn emit_stderr_message(message: &str) {
    crate::output_stream::write_stderr_bytes(message.as_bytes());
}
