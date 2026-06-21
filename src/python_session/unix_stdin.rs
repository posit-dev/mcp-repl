use crate::ipc;
use crate::python_input_queue::normalize_pty_input_payload;
use crate::stdin_payload::prepare_worker_stdin_payload;
use crate::worker_protocol::TextStream;

use super::state::{
    RawStdinReadError, ReadlineKind, SESSION_STATE, SessionStateInner, StdinReadAccounting,
    mark_input_wait_completed_request, mark_running_cell_after_input_locked,
    mark_waiting_input_locked, remember_emitted_prompt,
};
use super::stdio::{PythonThreadsAllowed, StdioLineRead};
use super::{CStdinLine, emit_output_text, emit_plots, record_background_plots};

pub(super) fn set_runtime_stdin_fd(_fd: libc::c_int) {}

pub(super) fn flush_terminal_input() {
    let _ = unsafe { libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH) };
}

pub(super) fn begin_input_batch(input: &str) -> Result<(), String> {
    let payload = normalize_pty_input_payload(prepare_worker_stdin_payload(input));
    let Some(state) = SESSION_STATE.get() else {
        return Ok(());
    };
    let should_record_background_plots = {
        let guard = state.inner.lock().unwrap();
        !guard.request_active
    };
    if should_record_background_plots {
        record_background_plots();
    }

    let mut guard = state.inner.lock().unwrap();
    guard.input_queue.begin_input(payload)?;
    guard.interrupt_requested = false;
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
    guard.input_queue.clear_after_interrupt();
    guard.request_active = guard.input_queue.has_active_input();
}

enum ReadlineAction {
    Line {
        bytes: Vec<u8>,
        prompt_already_visible: bool,
    },
    InputWait {
        prompt: String,
    },
    IdleWait {
        prompt: String,
    },
    Interrupted,
    Shutdown,
}

fn mark_protocol_failure_locked(guard: &mut SessionStateInner) {
    guard.session_end_emitted = true;
    guard.shutdown = true;
    guard.request_active = false;
    guard.input_queue.clear_for_protocol_failure();
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
    kind: ReadlineKind,
) -> Result<ReadlineAction, String> {
    guard.waiting_for_input = true;

    if guard.shutdown || guard.exit_requested {
        return Ok(ReadlineAction::Shutdown);
    }
    if guard.interrupt_requested {
        guard.interrupt_requested = false;
        return Ok(ReadlineAction::Interrupted);
    }

    if let Some(read) = guard.input_queue.consume_line()? {
        let prompt_already_visible = guard.visible_input_prompt.as_deref() == Some(prompt);
        guard.visible_input_prompt = None;
        mark_running_cell_after_input_locked(guard);
        guard.waiting_for_input = false;
        guard.request_active = true;
        return Ok(ReadlineAction::Line {
            bytes: read.protocol_bytes,
            prompt_already_visible,
        });
    }

    if guard.input_queue.take_completed_input() {
        let prompt = prompt.to_string();
        mark_waiting_input_locked(guard, &prompt, kind);
        guard.visible_input_prompt = (!prompt.is_empty()).then_some(prompt.clone());
        return Ok(ReadlineAction::InputWait { prompt });
    }

    Ok(ReadlineAction::IdleWait {
        prompt: prompt.to_string(),
    })
}

fn wait_for_turn_line(
    prompt: &str,
    kind: ReadlineKind,
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
            match next_readline_action_locked(&mut guard, prompt, kind) {
                Ok(ReadlineAction::IdleWait { prompt }) => {
                    if !idle_prompt_emitted {
                        idle_prompt_emitted = true;
                        drop(guard);
                        ipc::emit_input_wait(&prompt);
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
                Ok(action) => action,
                Err(message) => {
                    mark_protocol_failure_locked(&mut guard);
                    return Err(message);
                }
            }
        };

        match action {
            ReadlineAction::Line {
                bytes,
                prompt_already_visible,
            } => {
                let text = String::from_utf8_lossy(&bytes);
                ipc::emit_input_line(prompt, &text);
                if emit_prompt_to_stdout && !prompt.is_empty() && !prompt_already_visible {
                    emit_output_text(TextStream::Stdout, prompt.as_bytes());
                }
                return Ok(StdioLineRead {
                    bytes,
                    interrupted: false,
                });
            }
            ReadlineAction::InputWait { prompt } => {
                emit_plots();
                remember_emitted_prompt(&prompt);
                mark_input_wait_completed_request();
                ipc::emit_input_wait(&prompt);
                idle_prompt_emitted = true;
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
            ReadlineAction::IdleWait { .. } => unreachable!(),
        }
    }
}

pub(super) fn read_cpython_readline_turn_line(
    prompt: &str,
    emit_prompt_to_stdout: bool,
) -> Result<StdioLineRead, String> {
    wait_for_turn_line(
        prompt,
        ReadlineKind::PyOSReadline,
        emit_prompt_to_stdout,
        false,
    )
}

pub(super) fn read_runtime_stdin_line(prompt: &str) -> Result<StdioLineRead, String> {
    wait_for_turn_line(prompt, ReadlineKind::RawStdin, !prompt.is_empty(), true)
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
            match guard.input_queue.consume_bytes(remaining) {
                Ok(Some(read)) => {
                    mark_running_cell_after_input_locked(&mut guard);
                    guard.waiting_for_input = false;
                    guard.request_active = true;
                    ReadlineAction::Line {
                        bytes: read.protocol_bytes,
                        prompt_already_visible: true,
                    }
                }
                Ok(None) => {
                    if !output.is_empty() {
                        return Ok(output);
                    }
                    if guard.input_queue.take_completed_input() {
                        let prompt = String::new();
                        mark_waiting_input_locked(&mut guard, &prompt, ReadlineKind::RawStdin);
                        ReadlineAction::InputWait { prompt }
                    } else {
                        let allow_threads = PythonThreadsAllowed::new();
                        guard = state.cvar.wait(guard).unwrap();
                        drop(guard);
                        drop(allow_threads);
                        continue;
                    }
                }
                Err(message) => {
                    mark_protocol_failure_locked(&mut guard);
                    return Err(RawStdinReadError::Runtime(message));
                }
            }
        };

        match action {
            ReadlineAction::Line { bytes, .. } => {
                ipc::emit_input_line("", &String::from_utf8_lossy(&bytes));
                output.extend(bytes);
            }
            ReadlineAction::InputWait { prompt } => {
                emit_plots();
                remember_emitted_prompt(&prompt);
                mark_input_wait_completed_request();
                ipc::emit_input_wait(&prompt);
                if !output.is_empty() {
                    return Ok(output);
                }
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
