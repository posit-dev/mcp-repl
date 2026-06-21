use std::collections::VecDeque;
use std::ptr;
use std::sync::atomic::Ordering;

use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Storage::FileSystem::ReadFile;
use windows_sys::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE};
use windows_sys::Win32::System::Pipes::PeekNamedPipe;

use crate::ipc;
use crate::worker_protocol::TextStream;

use super::state::{
    ActiveRequest, InputBatchLine, PythonExecState, RawStdinReadError, ReadlineKind, SESSION_STATE,
    input_hook_prompt, mark_input_wait_completed_request, mark_running_cell_after_input_locked,
    mark_waiting_input_locked, remember_emitted_prompt, session_state,
};
use super::stdio::{PYTHON_STDIN_FILE, PythonThreadsAllowed, StdioLineRead};
use super::{emit_output_text, emit_plots};

pub(super) fn begin_tracked_input_batch(input: String) -> Result<(), String> {
    let state = session_state();
    let queued_lines = prepare_input_batch_lines(&input);
    let byte_len = queued_lines
        .iter()
        .map(|line| line.bytes.len().saturating_sub(line.offset))
        .sum();
    let line_count = queued_lines.len();
    let mut guard = state.inner.lock().unwrap();
    if guard.shutdown {
        return Err("Python session is shutting down".to_string());
    }
    if guard.active_request.is_some() {
        return Err("Python session already has an active input".to_string());
    }
    guard.interrupt_requested = false;
    guard.request_active = true;
    guard.plot_reset_pending = true;
    guard.turn_write_in_flight = false;
    guard.turn_cleanup_uncertain = false;
    let started_after_continuation_prompt = guard.last_prompt_was_continuation;
    guard.active_request = Some(ActiveRequest {
        byte_len,
        line_count,
        fallback_prompt: None,
        queued_lines,
        consumed_lines: 0,
        skip_next_hook: false,
        stdin_write_complete: true,
        repl_turn_finished: false,
        started_after_continuation_prompt,
    });
    state.cvar.notify_all();
    Ok(())
}

fn prepare_input_batch_lines(input: &str) -> VecDeque<InputBatchLine> {
    if input.is_empty() {
        return VecDeque::new();
    }
    let mut input = input.to_string();
    if !input.ends_with('\n') {
        input.push('\n');
    }
    input
        .split_inclusive('\n')
        .map(|line| InputBatchLine {
            text: line.to_string(),
            bytes: line.as_bytes().to_vec(),
            offset: 0,
            input_line_emitted: false,
        })
        .collect()
}

fn emit_input_batch_line(prompt: &str, line: &mut InputBatchLine) {
    if line.input_line_emitted {
        return;
    }
    ipc::emit_input_line(prompt, &line.text);
    line.input_line_emitted = true;
}

pub(super) fn read_windows_turn_line(
    prompt: &str,
    kind: ReadlineKind,
    emit_prompt_to_stdout: bool,
    release_gil_while_waiting: bool,
) -> Result<StdioLineRead, String> {
    let state = SESSION_STATE
        .get()
        .ok_or_else(|| "Python session state is not initialized".to_string())?;
    let mut idle_repl_prompt_emitted = false;

    loop {
        let action = {
            let mut guard = state.inner.lock().unwrap();
            guard.waiting_for_input = true;
            state.cvar.notify_all();

            if guard.shutdown || guard.exit_requested {
                return Ok(StdioLineRead {
                    bytes: Vec::new(),
                    interrupted: false,
                });
            }

            if guard.interrupt_requested {
                guard.interrupt_requested = false;
                return Ok(StdioLineRead {
                    bytes: Vec::new(),
                    interrupted: true,
                });
            }

            if guard.turn_cleanup_uncertain {
                if release_gil_while_waiting {
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

            match guard.active_request.as_mut() {
                Some(_) => {
                    let line = {
                        let active = guard.active_request.as_mut().expect("active input exists");
                        let line = active.queued_lines.pop_front();
                        if line.is_some() {
                            active.consumed_lines = active.consumed_lines.saturating_add(1);
                        }
                        line
                    };
                    if let Some(line) = line {
                        let prompt_already_visible =
                            guard.visible_input_prompt.as_deref() == Some(prompt);
                        guard.visible_input_prompt = None;
                        mark_running_cell_after_input_locked(&mut guard);
                        guard.waiting_for_input = false;
                        guard.request_active = true;
                        Some((Some(line), prompt_already_visible))
                    } else {
                        guard.active_request.take();
                        guard.visible_input_prompt = Some(prompt.to_string());
                        mark_waiting_input_locked(&mut guard, prompt, kind);
                        Some((None, false))
                    }
                }
                None => {
                    if matches!(
                        guard.exec_state,
                        PythonExecState::RunningCell { .. } | PythonExecState::WaitingInput { .. }
                    ) {
                        if !idle_repl_prompt_emitted {
                            mark_waiting_input_locked(&mut guard, prompt, kind);
                            guard.visible_input_prompt =
                                (!prompt.is_empty()).then(|| prompt.to_string());
                            drop(guard);
                            ipc::emit_input_wait(prompt);
                            idle_repl_prompt_emitted = true;
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
                    let should_emit_idle_repl_prompt = !idle_repl_prompt_emitted
                        && (prompt == guard.python_primary_prompt
                            || prompt == guard.python_continuation_prompt);
                    if should_emit_idle_repl_prompt {
                        idle_repl_prompt_emitted = true;
                        guard.last_prompt_was_continuation =
                            prompt == guard.python_continuation_prompt;
                        drop(guard);
                        ipc::emit_input_wait(prompt);
                        continue;
                    }
                    if release_gil_while_waiting {
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
            }
        };

        if let Some((Some(mut line), prompt_already_visible)) = action {
            emit_input_batch_line(prompt, &mut line);
            if emit_prompt_to_stdout && !prompt.is_empty() && !prompt_already_visible {
                emit_output_text(TextStream::Stdout, prompt.as_bytes());
            }
            return Ok(StdioLineRead {
                bytes: line.bytes[line.offset..].to_vec(),
                interrupted: false,
            });
        }

        if let Some((None, _)) = action {
            emit_plots();
            mark_input_wait_completed_request();
            remember_emitted_prompt(prompt);
            ipc::emit_input_wait(prompt);
            idle_repl_prompt_emitted = true;
        }
    }
}

pub(super) fn discard_pending_stdin() {
    let stdin = PYTHON_STDIN_FILE.load(Ordering::SeqCst);
    if !stdin.is_null() {
        unsafe {
            libc::fflush(stdin);
        }
    }
    drain_stdin_pipe();
}

fn drain_stdin_pipe() {
    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return;
    }

    let mut buffer = [0u8; 8192];
    loop {
        let mut available = 0u32;
        let ok = unsafe {
            PeekNamedPipe(
                handle,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                &mut available,
                ptr::null_mut(),
            )
        };
        if ok == 0 || available == 0 {
            break;
        }

        let to_read = available.min(buffer.len() as u32);
        let mut read = 0u32;
        let ok = unsafe {
            ReadFile(
                handle,
                buffer.as_mut_ptr().cast(),
                to_read,
                &mut read,
                ptr::null_mut(),
            )
        };
        if ok == 0 || read == 0 {
            break;
        }
    }
}

pub(super) fn read_raw_stdin_bytes(size: usize) -> Result<Vec<u8>, RawStdinReadError> {
    if size == 0 {
        return Ok(Vec::new());
    }
    take_raw_input_bytes(size)
}

enum RawInputEvent {
    InputLine { prompt: String, text: String },
    InputWait { prompt: String },
    Consumed,
}

fn take_raw_input_bytes(size: usize) -> Result<Vec<u8>, RawStdinReadError> {
    let state = SESSION_STATE.get().ok_or_else(|| {
        RawStdinReadError::Runtime("Python session state is not initialized".to_string())
    })?;
    let mut output = Vec::new();
    let mut input_wait_emitted = false;
    while output.len() < size {
        let event = {
            let mut guard = state.inner.lock().unwrap();
            guard.waiting_for_input = true;
            state.cvar.notify_all();

            if guard.shutdown || guard.exit_requested {
                return Ok(output);
            }

            if guard.interrupt_requested {
                guard.interrupt_requested = false;
                return Err(RawStdinReadError::Interrupted);
            }

            if guard.turn_cleanup_uncertain {
                let allow_threads = PythonThreadsAllowed::new();
                guard = state.cvar.wait(guard).unwrap();
                drop(guard);
                drop(allow_threads);
                continue;
            }

            let prompt = input_hook_prompt(&guard, None);
            if guard.active_request.is_none() {
                if !output.is_empty() {
                    return Ok(output);
                }
                if matches!(
                    guard.exec_state,
                    PythonExecState::RunningCell { .. } | PythonExecState::WaitingInput { .. }
                ) && !input_wait_emitted
                {
                    mark_waiting_input_locked(&mut guard, &prompt, ReadlineKind::RawStdin);
                    RawInputEvent::InputWait { prompt }
                } else {
                    let allow_threads = PythonThreadsAllowed::new();
                    guard = state.cvar.wait(guard).unwrap();
                    drop(guard);
                    drop(allow_threads);
                    continue;
                }
            } else if guard
                .active_request
                .as_ref()
                .is_some_and(|active| active.queued_lines.is_empty())
            {
                if !output.is_empty() {
                    return Ok(output);
                }
                guard.active_request.take();
                mark_waiting_input_locked(&mut guard, &prompt, ReadlineKind::RawStdin);
                RawInputEvent::InputWait { prompt }
            } else {
                let active = guard.active_request.as_mut().expect("active input exists");
                let line = active
                    .queued_lines
                    .front_mut()
                    .expect("active input has queued input");
                let event = if line.input_line_emitted {
                    RawInputEvent::Consumed
                } else {
                    line.input_line_emitted = true;
                    RawInputEvent::InputLine {
                        prompt,
                        text: line.text.clone(),
                    }
                };
                let available = &line.bytes[line.offset..];
                let take = available.len().min(size - output.len());
                output.extend_from_slice(&available[..take]);
                line.offset += take;
                if line.offset >= line.bytes.len() {
                    active.queued_lines.pop_front();
                }
                mark_running_cell_after_input_locked(&mut guard);
                event
            }
        };
        match event {
            RawInputEvent::InputLine { prompt, text } => ipc::emit_input_line(&prompt, &text),
            RawInputEvent::InputWait { prompt } => {
                input_wait_emitted = true;
                emit_plots();
                mark_input_wait_completed_request();
                remember_emitted_prompt(&prompt);
                ipc::emit_input_wait(&prompt);
            }
            RawInputEvent::Consumed => {}
        }
    }
    Ok(output)
}
