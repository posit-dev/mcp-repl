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
                    Some(ServerToWorkerIpcMessage::TurnStart { turn_id, input }) => {
                        match python_session::begin_turn(turn_id, input) {
                            Ok(()) => {}
                            Err(err) => {
                                emit_stderr_message(&err);
                                emit_session_end_with_reason("protocol_error", Some(turn_id));
                            }
                        }
                    }
                    Some(ServerToWorkerIpcMessage::TurnInput { turn_id, input }) => {
                        match python_session::append_turn_input(turn_id, input) {
                            Ok(()) => {}
                            Err(err) => {
                                emit_stderr_message(&err);
                                emit_session_end_with_reason("protocol_error", Some(turn_id));
                            }
                        }
                    }
                    Some(ServerToWorkerIpcMessage::Interrupt { turn_id }) => {
                        #[cfg(windows)]
                        {
                            if let Some(turn_id) = turn_id {
                                python_session::interrupt_turn(turn_id);
                            } else {
                                python_session::interrupt();
                            }
                        }
                        #[cfg(not(windows))]
                        {
                            let _ = turn_id;
                            python_session::interrupt();
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

fn emit_stderr_message(message: &str) {
    crate::output_stream::write_stderr_bytes(message.as_bytes());
}
