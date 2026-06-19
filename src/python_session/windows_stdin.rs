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
    ActiveRequest, InputBatchLine, RawStdinReadError, SESSION_STATE, input_hook_prompt,
    mark_input_wait_completed_request, remember_emitted_prompt, session_state,
};
use super::stdio::{PYTHON_STDIN_FILE, PythonThreadsAllowed, StdioLineRead};
use super::{emit_output_text, emit_plots, request_platform_interrupt};

pub(super) fn interrupt_input(input_id: u64) {
    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    {
        let mut guard = state.inner.lock().unwrap();
        let write_in_flight = guard.turn_write_in_flight;
        let Some(active) = guard.active_request.as_mut() else {
            return;
        };
        if active.input_id != Some(input_id) {
            return;
        }
        active.queued_lines.clear();
        if write_in_flight {
            guard.turn_cleanup_uncertain = true;
        }
        guard.interrupt_requested = true;
        guard.waiting_for_input = false;
        state.cvar.notify_all();
    }
    request_platform_interrupt();
}

pub(super) fn begin_tracked_input_batch(input_id: u64, input: String) -> Result<(), String> {
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
        input_id: Some(input_id),
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

fn emit_input_batch_line(input_id: u64, prompt: &str, line: &mut InputBatchLine) {
    if line.input_line_emitted {
        return;
    }
    ipc::emit_input_line(input_id, prompt, &line.text);
    line.input_line_emitted = true;
}

pub(super) fn read_windows_turn_line(
    prompt: &str,
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

            match guard
                .active_request
                .as_mut()
                .and_then(|active| active.input_id)
            {
                Some(input_id) => {
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
                        guard.waiting_for_input = false;
                        guard.request_active = true;
                        Some((input_id, Some(line), prompt_already_visible))
                    } else {
                        guard.active_request.take();
                        guard.visible_input_prompt = Some(prompt.to_string());
                        Some((input_id, None, false))
                    }
                }
                None => {
                    let should_emit_idle_repl_prompt = !idle_repl_prompt_emitted
                        && (prompt == guard.python_primary_prompt
                            || prompt == guard.python_continuation_prompt);
                    if should_emit_idle_repl_prompt {
                        idle_repl_prompt_emitted = true;
                        guard.last_prompt_was_continuation =
                            prompt == guard.python_continuation_prompt;
                        drop(guard);
                        ipc::emit_readline_start(prompt);
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

        if let Some((input_id, Some(mut line), prompt_already_visible)) = action {
            emit_input_batch_line(input_id, prompt, &mut line);
            if emit_prompt_to_stdout && !prompt.is_empty() && !prompt_already_visible {
                emit_output_text(TextStream::Stdout, prompt.as_bytes());
            }
            return Ok(StdioLineRead {
                bytes: line.bytes[line.offset..].to_vec(),
                interrupted: false,
            });
        }

        if let Some((input_id, None, _)) = action {
            emit_plots();
            mark_input_wait_completed_request();
            remember_emitted_prompt(prompt);
            ipc::emit_input_wait(input_id, prompt);
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

pub(super) fn stdin_pending_byte_count() -> Option<usize> {
    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return None;
    }

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
    (ok != 0).then_some(available as usize)
}

pub(super) fn read_raw_stdin_bytes(size: usize) -> Result<Vec<u8>, RawStdinReadError> {
    if size == 0 {
        return Ok(Vec::new());
    }
    take_raw_input_bytes(size)
}

enum RawInputEvent {
    InputLine {
        input_id: u64,
        prompt: String,
        text: String,
    },
    InputWait {
        input_id: u64,
        prompt: String,
    },
    Consumed,
}

fn take_raw_input_bytes(size: usize) -> Result<Vec<u8>, RawStdinReadError> {
    let state = SESSION_STATE.get().ok_or_else(|| {
        RawStdinReadError::Runtime("Python session state is not initialized".to_string())
    })?;
    let mut output = Vec::new();
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
            let Some(input_id) = guard
                .active_request
                .as_ref()
                .and_then(|active| active.input_id)
            else {
                if !output.is_empty() {
                    return Ok(output);
                }
                if guard.active_request.is_some() {
                    return Ok(output);
                }
                let allow_threads = PythonThreadsAllowed::new();
                guard = state.cvar.wait(guard).unwrap();
                drop(guard);
                drop(allow_threads);
                continue;
            };

            if guard
                .active_request
                .as_ref()
                .is_some_and(|active| active.queued_lines.is_empty())
            {
                if !output.is_empty() {
                    return Ok(output);
                }
                guard.active_request.take();
                RawInputEvent::InputWait { input_id, prompt }
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
                        input_id,
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
                event
            }
        };
        match event {
            RawInputEvent::InputLine {
                input_id,
                prompt,
                text,
            } => ipc::emit_input_line(input_id, &prompt, &text),
            RawInputEvent::InputWait { input_id, prompt } => {
                emit_plots();
                mark_input_wait_completed_request();
                remember_emitted_prompt(&prompt);
                ipc::emit_input_wait(input_id, &prompt);
            }
            RawInputEvent::Consumed => {}
        }
    }
    Ok(output)
}
