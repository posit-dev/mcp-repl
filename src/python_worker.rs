use std::thread;
use std::time::Duration;

use crate::ipc::{ServerToWorkerIpcMessage, connect_from_env, set_global_ipc};
use crate::python_session::{self, PythonSession};

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    crate::diagnostics::startup_log("python-worker: run begin");
    init_ipc().map_err(|err| {
        eprintln!("python worker ipc init error: {err}");
        err
    })?;

    crate::diagnostics::startup_log("python-worker: starting Python session");
    if let Err(err) = PythonSession::start_on_current_thread() {
        if !python_session::protocol_failure_recorded() {
            eprintln!("failed to start Python session: {err}");
        }
        return Err(std::io::Error::other(err).into());
    }
    crate::diagnostics::startup_log("python-worker: Python session exited");

    Ok(())
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
        .name("python-worker-ipc".to_string())
        .spawn(move || {
            loop {
                match conn.recv(None) {
                    Some(ServerToWorkerIpcMessage::InputBatch { input }) => {
                        match python_session::begin_input(input) {
                            Ok(()) => {}
                            Err(err) => {
                                python_session::record_protocol_failure(&err);
                                break;
                            }
                        }
                    }
                    Some(ServerToWorkerIpcMessage::Interrupt {}) => {
                        python_session::interrupt();
                        #[cfg(windows)]
                        if let Err(err) = arm_windows_interrupt_delivery(&conn) {
                            python_session::record_protocol_failure(&format!(
                                "failed to arm Windows interrupt delivery: {err}"
                            ));
                            break;
                        }
                    }
                    Some(ServerToWorkerIpcMessage::Shutdown {}) => {
                        if let Err(err) = python_session::request_shutdown() {
                            python_session::record_protocol_failure(&err);
                            break;
                        }
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
