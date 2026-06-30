use std::thread;
use std::time::Duration;

use crate::ipc::{
    ServerToWorkerIpcMessage, connect_from_env, emit_session_end_with_reason, set_global_ipc,
};
use crate::python_session::{self, PythonSession};

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    crate::diagnostics::startup_log("python-worker: run begin");
    init_ipc().map_err(|err| {
        eprintln!("python worker ipc init error: {err}");
        err
    })?;

    crate::diagnostics::startup_log("python-worker: starting Python session");
    if let Err(err) = PythonSession::start_on_current_thread() {
        eprintln!("failed to start Python session: {err}");
        return Err(std::io::Error::other(err).into());
    }
    crate::diagnostics::startup_log("python-worker: Python session exited");

    Ok(())
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
                                emit_stderr_message(&err);
                                emit_session_end_with_reason("protocol_error");
                            }
                        }
                    }
                    Some(ServerToWorkerIpcMessage::DiscardPendingInput { discard_id }) => {
                        // Sideband discard is queue cleanup only. Runtime
                        // interruption comes from the OS signal sent by the
                        // server, not from this IPC branch.
                        let discarded_input =
                            python_session::discard_unconsumed_input_for_discard_ack();
                        crate::ipc::emit_discard_pending_input_ack(discard_id, discarded_input);
                    }
                    Some(ServerToWorkerIpcMessage::Shutdown {}) => {
                        python_session::request_shutdown();
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

fn emit_stderr_message(message: &str) {
    crate::output_stream::write_stderr_bytes(message.as_bytes());
}
