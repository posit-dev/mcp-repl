use std::thread;
use std::time::Duration;

use crate::backend::{Backend, backend_from_env};
use crate::ipc::{ServerToWorkerIpcMessage, connect_from_env, set_global_ipc};
use crate::r_session::RSession;
use crate::worker_protocol::WORKER_MODE_ARG;

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
    init_ipc().map_err(|err| {
        eprintln!("worker ipc init error: {err}");
        err
    })?;

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

fn init_ipc() -> Result<(), Box<dyn std::error::Error>> {
    let conn = connect_from_env(Duration::from_secs(2))?;
    set_global_ipc(conn.clone());
    if let Err(err) = thread::Builder::new()
        .name("worker-ipc".to_string())
        .spawn(move || {
            loop {
                match conn.recv(None) {
                    Some(ServerToWorkerIpcMessage::InputBatch { input }) => {
                        match wait_for_r_session().and_then(|session| session.begin_input(input)) {
                            Ok(()) => {}
                            Err(err) => {
                                crate::output_stream::write_stderr_bytes(err.as_bytes());
                                crate::ipc::emit_session_end_with_reason("protocol_error");
                            }
                        }
                    }
                    Some(ServerToWorkerIpcMessage::Interrupt {}) => {
                        let discarded_input = crate::r_session::interrupt_pending_input();
                        crate::ipc::emit_interrupt_ack(discarded_input);
                    }
                    Some(ServerToWorkerIpcMessage::Shutdown {}) => {
                        let _ = wait_for_r_session().and_then(RSession::request_shutdown);
                    }
                    None => {
                        // Without IPC, the worker cannot participate in input accounting (prompt,
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
