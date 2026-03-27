use std::io::{BufRead, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use crate::input_protocol::parse_input_frame_header;
use crate::ipc::{
    ServerToWorkerIpcMessage, connect_from_env, emit_backend_info, emit_request_end, set_global_ipc,
};
use crate::r_session::RSession;
use crate::worker_protocol::WORKER_MODE_ARG;

struct WorkerState {
    busy: AtomicBool,
    shutting_down: AtomicBool,
}

impl Default for WorkerState {
    fn default() -> Self {
        Self {
            busy: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
        }
    }
}

impl WorkerState {
    fn try_mark_busy(&self) -> bool {
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

struct QueuedRequest {
    text: String,
}

pub fn is_worker_mode() -> bool {
    let bare = std::ffi::OsStr::new(WORKER_MODE_ARG);
    let flag = std::ffi::OsString::from(format!("--{WORKER_MODE_ARG}"));
    std::env::args_os().any(|arg| arg == bare || arg == flag)
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    crate::diagnostics::startup_log("worker: run begin");
    let state = Arc::new(WorkerState::default());
    let (request_tx, request_rx) = mpsc::sync_channel(1);
    init_ipc(state.clone()).map_err(|err| {
        eprintln!("worker ipc init error: {err}");
        err
    })?;
    emit_backend_info("r", true);
    let request_state = state.clone();
    let _request_thread = thread::Builder::new()
        .name("worker-requests".to_string())
        .spawn(move || request_loop(request_rx, request_state))
        .map_err(|err| format!("failed to spawn worker request thread: {err}"))?;

    let stdin_state = state.clone();
    let stdin_requests = request_tx.clone();
    let _stdin_thread = thread::Builder::new()
        .name("worker-stdin".to_string())
        .spawn(move || {
            if let Err(err) = stdin_loop(stdin_state, stdin_requests) {
                eprintln!("worker stdin error: {err}");
            }
        })
        .map_err(|err| format!("failed to spawn worker stdin thread: {err}"))?;

    crate::diagnostics::startup_log("worker: starting R session");
    if let Err(err) = RSession::start_on_current_thread() {
        eprintln!("failed to start R session: {err}");
        return Err(std::io::Error::other(err).into());
    }
    crate::diagnostics::startup_log("worker: R session exited");

    Ok(())
}

fn wait_for_r_session() -> Result<&'static RSession, String> {
    loop {
        if let Ok(session) = RSession::global() {
            return Ok(session);
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn init_ipc(state: Arc<WorkerState>) -> Result<(), Box<dyn std::error::Error>> {
    let conn = connect_from_env(Duration::from_secs(2))?;
    set_global_ipc(conn.clone());
    if let Err(err) = thread::Builder::new()
        .name("worker-ipc".to_string())
        .spawn(move || {
            loop {
                match conn.recv(None) {
                    Some(ServerToWorkerIpcMessage::StdinWrite { .. }) => {}
                    Some(ServerToWorkerIpcMessage::Interrupt) => {
                        let _ = crate::r_session::request_interrupt();
                        crate::r_session::clear_pending_input();
                    }
                    Some(ServerToWorkerIpcMessage::SessionEnd) => {
                        state.begin_shutdown();
                        let _ = crate::r_session::request_interrupt();
                        crate::r_session::clear_pending_input();
                        let _ = crate::r_session::request_shutdown();
                    }
                    None => {
                        // Without IPC, the worker cannot participate in turn accounting (prompt,
                        // request boundaries, etc). Exit immediately so the server can respawn.
                        std::process::exit(0);
                    }
                }
            }
        })
    {
        eprintln!("worker ipc thread error: {err}");
    }
    Ok(())
}

fn stdin_loop(
    state: Arc<WorkerState>,
    request_tx: mpsc::SyncSender<QueuedRequest>,
) -> Result<(), Box<dyn std::error::Error>> {
    let stdin = std::io::stdin();
    let mut reader = std::io::BufReader::new(stdin);
    let mut line = String::new();
    loop {
        line.clear();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            state.begin_shutdown();
            if !crate::r_session::request_shutdown() {
                crate::ipc::emit_session_end();
                std::process::exit(0);
            }
            break;
        }

        // Once shutdown is requested, stop consuming stdin immediately.
        // Remaining bytes can stay unread because this worker is terminating.
        if state.is_shutting_down() {
            break;
        }

        if let Some(len) = parse_input_frame_header(&line) {
            let mut buffer = vec![0u8; len];
            reader.read_exact(&mut buffer)?;
            let text = String::from_utf8_lossy(&buffer).to_string();
            handle_write_stdin(text, state.clone(), &request_tx);
        } else {
            handle_write_stdin(line.clone(), state.clone(), &request_tx);
        }
    }

    Ok(())
}

fn request_loop(rx: mpsc::Receiver<QueuedRequest>, state: Arc<WorkerState>) {
    for request in rx {
        let result = write_stdin_request(request.text);
        if let Err(err) = result {
            emit_stderr_message(&err.message);
            emit_request_end();
        }
        state.mark_idle();
    }
}
fn handle_write_stdin(
    text: String,
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

    if let Err(err) = request_tx.try_send(QueuedRequest { text }) {
        state.mark_idle();
        let message = match err {
            mpsc::TrySendError::Full(_) => "worker is busy; request already running".to_string(),
            mpsc::TrySendError::Disconnected(_) => {
                "worker execution thread exited unexpectedly".to_string()
            }
        };
        emit_stderr_message(&message);
        emit_request_end();
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

fn write_stdin_request(text: String) -> Result<(), WorkerExecError> {
    let session = wait_for_r_session()
        .map_err(|err| WorkerExecError::new(format!("failed to start R session: {err}")))?;

    let reply_rx = session.send_request(text).map_err(WorkerExecError::new)?;

    match reply_rx.recv() {
        Ok(_reply) => Ok(()),
        Err(err) => Err(WorkerExecError::new(format!(
            "R session reply error: {err}"
        ))),
    }
}

fn emit_stderr_message(message: &str) {
    crate::output_stream::write_stderr_bytes(message.as_bytes());
}
