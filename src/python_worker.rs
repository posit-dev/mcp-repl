use std::thread;
use std::time::Duration;

use base64::Engine as _;

use crate::ipc::{
    ServerToWorkerIpcMessage, connect_from_env, emit_python_interrupt_ack, emit_session_end,
    emit_session_end_with_reason, emit_stdin_write_ack, set_global_ipc,
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

fn wait_for_python_session() -> Result<&'static PythonSession, String> {
    loop {
        if let Ok(session) = PythonSession::global() {
            return Ok(session);
        }
        thread::sleep(Duration::from_millis(5));
    }
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
                        match wait_for_python_session()
                            .and_then(|session| session.begin_turn(turn_id, input))
                        {
                            Ok(()) => {}
                            Err(err) => {
                                emit_stderr_message(&err);
                                emit_session_end_with_reason("protocol_error", Some(turn_id));
                            }
                        }
                    }
                    Some(ServerToWorkerIpcMessage::PythonRequestStart {
                        request_generation,
                        stdin_b64,
                    }) => {
                        let stdin_bytes =
                            match base64::engine::general_purpose::STANDARD.decode(stdin_b64) {
                                Ok(bytes) => bytes,
                                Err(_) => {
                                    emit_stderr_message("invalid python_request_start stdin_b64");
                                    emit_session_end();
                                    continue;
                                }
                            };
                        python_session::mark_request_started_for_generation(
                            request_generation,
                            stdin_bytes,
                        );
                        emit_stdin_write_ack();
                    }
                    Some(ServerToWorkerIpcMessage::StdinWriteComplete) => {
                        python_session::mark_stdin_write_complete();
                    }
                    Some(ServerToWorkerIpcMessage::Interrupt { turn_id }) => {
                        if let Some(turn_id) = turn_id {
                            python_session::interrupt_turn(turn_id);
                        } else {
                            python_session::interrupt();
                        }
                    }
                    Some(ServerToWorkerIpcMessage::PythonInterrupt { request_generation }) => {
                        python_session::interrupt_request_generation(request_generation);
                        emit_python_interrupt_ack();
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
