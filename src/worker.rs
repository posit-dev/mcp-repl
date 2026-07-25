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
        if !crate::r_session::protocol_failure_recorded() {
            eprintln!("failed to start R session: {err}");
        }
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

#[cfg(windows)]
fn arm_windows_interrupt_delivery(conn: &crate::ipc::WorkerIpcConnection) -> Result<(), String> {
    crate::windows_interrupt_observer::notify_interrupt_cleanup_complete()
        .map_err(|err| format!("failed to finish Windows interrupt cleanup: {err}"))?;
    match crate::windows_interrupt_observer::rearm()
        .map_err(|err| format!("failed to rearm Windows interrupt observer: {err}"))?
    {
        crate::windows_interrupt_observer::RearmOutcome::Rearmed => {}
        crate::windows_interrupt_observer::RearmOutcome::InterruptInFlight => {
            return Err(
                "Windows interrupt observer found a native dispatch in flight before delivery"
                    .to_string(),
            );
        }
    }
    conn.send(crate::ipc::WorkerToServerIpcMessage::InterruptArmed {})
        .map_err(|err| format!("failed to acknowledge armed Windows interrupt: {err}"))
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
                                crate::r_session::record_protocol_failure(&err);
                                break;
                            }
                        }
                    }
                    Some(ServerToWorkerIpcMessage::Interrupt {}) => {
                        crate::r_session::discard_pending_input_after_interrupt();
                        #[cfg(windows)]
                        if let Err(err) = arm_windows_interrupt_delivery(&conn) {
                            crate::r_session::record_protocol_failure(&format!(
                                "failed to arm Windows interrupt delivery: {err}"
                            ));
                            break;
                        }
                    }
                    Some(ServerToWorkerIpcMessage::Shutdown {}) => {
                        if let Err(err) = wait_for_r_session().and_then(RSession::request_shutdown)
                        {
                            crate::r_session::record_protocol_failure(&err);
                            break;
                        }
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
