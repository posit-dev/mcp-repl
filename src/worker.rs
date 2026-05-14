use std::io::BufRead;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use crate::backend::{Backend, backend_from_env};
use crate::ipc::{ServerToWorkerIpcMessage, connect_from_env, emit_worker_ready, set_global_ipc};
use crate::r_session::RSession;
use crate::worker_protocol::WORKER_MODE_ARG;

struct WorkerState {
    shutting_down: AtomicBool,
}

impl WorkerState {
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
            shutting_down: AtomicBool::new(false),
        }
    }
}

pub fn is_worker_mode() -> bool {
    let bare = std::ffi::OsStr::new(WORKER_MODE_ARG);
    let flag = std::ffi::OsString::from(format!("--{WORKER_MODE_ARG}"));
    std::env::args_os().any(|arg| arg == bare || arg == flag)
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    match backend_from_env()?.unwrap_or(Backend::R) {
        Backend::R => run_r_worker(),
        Backend::Python => crate::python_worker::run(),
    }
}

fn run_r_worker() -> Result<(), Box<dyn std::error::Error>> {
    crate::diagnostics::startup_log("worker: run begin");
    let state = Arc::new(WorkerState::default());
    init_ipc(state.clone()).map_err(|err| {
        eprintln!("worker ipc init error: {err}");
        err
    })?;
    emit_worker_ready("r", true, Some("quit(\"no\")\n"));

    let stdin_state = state.clone();
    let _stdin_thread = thread::Builder::new()
        .name("worker-stdin".to_string())
        .spawn(move || {
            if let Err(err) = stdin_loop(stdin_state) {
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
                    Some(ServerToWorkerIpcMessage::RequestStart) => {}
                    Some(ServerToWorkerIpcMessage::PythonRequestStart { .. }) => {}
                    Some(ServerToWorkerIpcMessage::StdinWrite { .. }) => {}
                    Some(ServerToWorkerIpcMessage::StdinWriteComplete) => {}
                    Some(ServerToWorkerIpcMessage::Interrupt) => {
                        crate::r_session::clear_pending_input();
                    }
                    Some(ServerToWorkerIpcMessage::PythonInterrupt { .. }) => {
                        crate::r_session::clear_pending_input();
                    }
                    Some(ServerToWorkerIpcMessage::SessionEnd) => {
                        state.begin_shutdown();
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

fn stdin_loop(state: Arc<WorkerState>) -> Result<(), Box<dyn std::error::Error>> {
    let stdin = std::io::stdin();
    let mut reader = std::io::BufReader::new(stdin);
    loop {
        if state.is_shutting_down() {
            break;
        }

        let mut buffer = Vec::new();
        let read_len = reader.read_until(b'\n', &mut buffer)?;
        if read_len == 0 {
            state.begin_shutdown();
            if !crate::r_session::request_shutdown() {
                crate::ipc::emit_session_end();
                std::process::exit(0);
            }
            break;
        }

        if state.is_shutting_down() {
            break;
        }

        let text = String::from_utf8_lossy(&buffer).to_string();
        let session = wait_for_r_session().map_err(std::io::Error::other)?;
        session.enqueue_input(text).map_err(std::io::Error::other)?;
    }

    Ok(())
}
