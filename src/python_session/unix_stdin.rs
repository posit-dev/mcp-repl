use crate::ipc;
use crate::python_turn_input::normalize_pty_turn_payload;
use crate::stdin_payload::prepare_worker_stdin_payload;
use crate::worker_protocol::TextStream;

use super::state::{
    PythonReadlineState, RawStdinReadError, SESSION_STATE, SessionStateInner, StdinReadAccounting,
    mark_stdin_wait_prompt_completed_request, remember_emitted_prompt,
};
use super::stdio::{PythonThreadsAllowed, StdioLineRead};
use super::{CStdinLine, emit_output_text, emit_plots, record_background_plots};

pub(super) fn set_runtime_stdin_fd(_fd: libc::c_int) {}

pub(super) fn flush_terminal_input() {
    let _ = unsafe { libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH) };
}

pub(super) fn begin_or_append_turn_input(turn_id: u64, input: &str) -> Result<(), String> {
    let payload = normalize_pty_turn_payload(prepare_worker_stdin_payload(input));
    let Some(state) = SESSION_STATE.get() else {
        return Ok(());
    };
    let should_record_background_plots = {
        let guard = state.inner.lock().unwrap();
        !guard.request_active || guard.request_completed_at_stdin_wait
    };
    if should_record_background_plots {
        record_background_plots();
    }

    let mut guard = state.inner.lock().unwrap();
    guard.turn_input.begin_or_append(turn_id, payload)?;
    guard.interrupt_requested = false;
    guard.request_completed_at_stdin_wait = false;
    guard.request_active = true;
    guard.plot_reset_pending = true;
    state.cvar.notify_all();
    Ok(())
}

pub(super) fn discard_pending_stdin() {
    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    let mut guard = state.inner.lock().unwrap();
    guard.turn_input.clear_after_interrupt();
    guard.request_completed_at_stdin_wait = false;
    guard.request_active = true;
}

enum ReadlineAction {
    Line {
        turn_id: u64,
        bytes: Vec<u8>,
        prompt_already_visible: bool,
    },
    Idle {
        turn_id: u64,
        prompt: String,
    },
    StdinWait {
        turn_id: u64,
        prompt: String,
    },
    ReadlineStart {
        prompt: String,
    },
    WaitForTurnInput,
    Interrupted,
    Shutdown,
}

fn mark_protocol_failure_locked(guard: &mut SessionStateInner) {
    guard.session_end_emitted = true;
    guard.shutdown = true;
    guard.request_active = false;
    guard.turn_input.clear_for_protocol_failure();
}

pub(super) fn emit_protocol_failure(message: &str) {
    if let Some(state) = SESSION_STATE.get() {
        let mut guard = state.inner.lock().unwrap();
        mark_protocol_failure_locked(&mut guard);
    }
    emit_output_text(TextStream::Stderr, message.as_bytes());
    ipc::emit_session_end();
}

fn next_readline_action_locked(
    guard: &mut SessionStateInner,
    prompt: &str,
) -> Result<ReadlineAction, String> {
    guard.waiting_for_input = true;

    if guard.shutdown || guard.exit_requested {
        return Ok(ReadlineAction::Shutdown);
    }
    if guard.interrupt_requested {
        guard.interrupt_requested = false;
        return Ok(ReadlineAction::Interrupted);
    }

    if let Some(read) = guard.turn_input.consume_line()? {
        guard.waiting_for_input = false;
        guard.request_active = true;
        return Ok(ReadlineAction::Line {
            turn_id: read.turn_id,
            bytes: read.protocol_bytes,
            prompt_already_visible: false,
        });
    }

    if let Some(turn_id) = guard.turn_input.active_consumed_turn() {
        if guard.request_completed_at_stdin_wait {
            return Ok(ReadlineAction::WaitForTurnInput);
        }
        let prompt = prompt.to_string();
        if matches!(
            guard.current_readline_state,
            Some(PythonReadlineState::Primary | PythonReadlineState::Continuation)
        ) {
            let turn_id = guard
                .turn_input
                .take_completed_turn()
                .expect("active consumed turn should be completed");
            guard.request_active = false;
            return Ok(ReadlineAction::Idle { turn_id, prompt });
        }
        return Ok(ReadlineAction::StdinWait { turn_id, prompt });
    }

    Ok(ReadlineAction::ReadlineStart {
        prompt: prompt.to_string(),
    })
}

fn wait_for_turn_line(
    prompt: &str,
    emit_prompt_to_stdout: bool,
    release_gil_while_waiting: bool,
) -> Result<StdioLineRead, String> {
    let state = SESSION_STATE
        .get()
        .ok_or_else(|| "Python session state is not initialized".to_string())?;
    let mut idle_prompt_emitted = false;

    loop {
        let action = {
            let mut guard = state.inner.lock().unwrap();
            match next_readline_action_locked(&mut guard, prompt) {
                Ok(ReadlineAction::ReadlineStart { prompt }) => {
                    if !idle_prompt_emitted {
                        idle_prompt_emitted = true;
                        drop(guard);
                        ipc::emit_readline_start(&prompt);
                    } else if release_gil_while_waiting {
                        let allow_threads = PythonThreadsAllowed::new();
                        guard = state.cvar.wait(guard).unwrap();
                        drop(guard);
                        drop(allow_threads);
                    } else {
                        guard = state.cvar.wait(guard).unwrap();
                        drop(guard);
                    }
                    continue;
                }
                Ok(ReadlineAction::WaitForTurnInput) => {
                    let allow_threads = release_gil_while_waiting.then(PythonThreadsAllowed::new);
                    guard = state.cvar.wait(guard).unwrap();
                    drop(guard);
                    drop(allow_threads);
                    continue;
                }
                Ok(action) => action,
                Err(message) => {
                    mark_protocol_failure_locked(&mut guard);
                    return Err(message);
                }
            }
        };

        match action {
            ReadlineAction::Line {
                turn_id,
                bytes,
                prompt_already_visible,
            } => {
                let text = String::from_utf8_lossy(&bytes);
                ipc::emit_input_line(turn_id, prompt, &text);
                if emit_prompt_to_stdout && !prompt.is_empty() && !prompt_already_visible {
                    emit_output_text(TextStream::Stdout, prompt.as_bytes());
                }
                return Ok(StdioLineRead {
                    bytes,
                    interrupted: false,
                });
            }
            ReadlineAction::Idle { turn_id, prompt } => {
                emit_plots();
                remember_emitted_prompt(&prompt);
                ipc::emit_idle(turn_id, &prompt);
            }
            ReadlineAction::StdinWait { turn_id, prompt } => {
                emit_plots();
                mark_stdin_wait_prompt_completed_request();
                remember_emitted_prompt(&prompt);
                ipc::emit_stdin_wait(turn_id, &prompt);
            }
            ReadlineAction::Interrupted => {
                return Ok(StdioLineRead {
                    bytes: Vec::new(),
                    interrupted: true,
                });
            }
            ReadlineAction::Shutdown => {
                return Ok(StdioLineRead {
                    bytes: Vec::new(),
                    interrupted: false,
                });
            }
            ReadlineAction::ReadlineStart { .. } => unreachable!(),
            ReadlineAction::WaitForTurnInput => unreachable!(),
        }
    }
}

pub(super) fn read_cpython_readline_turn_line(
    prompt: &str,
    emit_prompt_to_stdout: bool,
) -> Result<StdioLineRead, String> {
    wait_for_turn_line(prompt, emit_prompt_to_stdout, false)
}

pub(super) fn read_runtime_stdin_line(prompt: &str) -> Result<StdioLineRead, String> {
    wait_for_turn_line(prompt, !prompt.is_empty(), true)
}

pub(super) fn note_cpython_readline_bytes_read(
    _prompt: &str,
    _bytes: &[u8],
) -> Result<StdinReadAccounting, String> {
    Ok(StdinReadAccounting::Accounted)
}

pub(super) fn fork_child_stdin_eof(prompt: &str) -> CStdinLine {
    // Fork children inherit fd 0/1/2, but mcp-repl sideband IPC is deliberately
    // disabled in the at-fork child handler. Managed input lives in the parent
    // worker queue, so IPC-disabled children must see EOF for stdin.
    if !prompt.is_empty() {
        emit_output_text(TextStream::Stdout, prompt.as_bytes());
    }
    CStdinLine::Eof
}

pub(super) fn read_raw_stdin_bytes(size: usize) -> Result<Vec<u8>, RawStdinReadError> {
    if ipc::worker_ipc_disabled_for_process() {
        return Ok(Vec::new());
    }
    if size == 0 {
        return Ok(Vec::new());
    }

    let state = SESSION_STATE.get().ok_or_else(|| {
        RawStdinReadError::Runtime("Python session state is not initialized".to_string())
    })?;
    let mut output = Vec::new();
    while output.len() < size {
        let action = {
            let mut guard = state.inner.lock().unwrap();
            guard.waiting_for_input = true;

            if guard.shutdown || guard.exit_requested {
                return Ok(output);
            }
            if guard.interrupt_requested {
                guard.interrupt_requested = false;
                return Err(RawStdinReadError::Interrupted);
            }

            let remaining = size - output.len();
            match guard.turn_input.consume_bytes(remaining) {
                Ok(Some(read)) => {
                    guard.waiting_for_input = false;
                    guard.request_active = true;
                    ReadlineAction::Line {
                        turn_id: read.turn_id,
                        bytes: read.protocol_bytes,
                        prompt_already_visible: true,
                    }
                }
                Ok(None) => {
                    if let Some(turn_id) = guard.turn_input.active_consumed_turn() {
                        if guard.request_completed_at_stdin_wait {
                            if output.is_empty() {
                                let allow_threads = PythonThreadsAllowed::new();
                                guard = state.cvar.wait(guard).unwrap();
                                drop(guard);
                                drop(allow_threads);
                                continue;
                            }
                            return Ok(output);
                        }
                        let prompt = String::new();
                        ReadlineAction::StdinWait { turn_id, prompt }
                    } else if output.is_empty() {
                        let allow_threads = PythonThreadsAllowed::new();
                        guard = state.cvar.wait(guard).unwrap();
                        drop(guard);
                        drop(allow_threads);
                        continue;
                    } else {
                        return Ok(output);
                    }
                }
                Err(message) => {
                    mark_protocol_failure_locked(&mut guard);
                    return Err(RawStdinReadError::Runtime(message));
                }
            }
        };

        match action {
            ReadlineAction::Line { turn_id, bytes, .. } => {
                ipc::emit_input_line(turn_id, "", &String::from_utf8_lossy(&bytes));
                output.extend(bytes);
            }
            ReadlineAction::StdinWait { turn_id, prompt } => {
                emit_plots();
                mark_stdin_wait_prompt_completed_request();
                remember_emitted_prompt(&prompt);
                ipc::emit_stdin_wait(turn_id, &prompt);
            }
            _ => {}
        }
    }
    Ok(output)
}

pub(super) fn note_stdin_line_read(
    _prompt: &str,
    _bytes: &[u8],
) -> Result<StdinReadAccounting, String> {
    Ok(StdinReadAccounting::Accounted)
}

pub(super) fn stdin_pending_byte_count() -> Option<usize> {
    None
}

pub(super) fn handle_protocol_input_hook() {
    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    let mut guard = state.inner.lock().unwrap();
    if guard.shutdown {
        return;
    }
    guard.waiting_for_input = true;
    state.cvar.notify_all();
}
