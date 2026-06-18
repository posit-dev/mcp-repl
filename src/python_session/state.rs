use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, OnceLock};

#[cfg(target_family = "unix")]
use crate::python_turn_input::PythonTurnInput;

pub(super) static SESSION_STATE: OnceLock<Arc<SessionState>> = OnceLock::new();

pub(super) struct SessionState {
    pub(super) inner: Mutex<SessionStateInner>,
    pub(super) cvar: Condvar,
}

pub(super) struct SessionStateInner {
    pub(super) active_request: Option<ActiveRequest>,
    pub(super) request_active: bool,
    pub(super) request_completed_at_stdin_wait: bool,
    pub(super) current_prompt: Option<String>,
    pub(super) current_readline_state: Option<PythonReadlineState>,
    #[cfg(windows)]
    pub(super) visible_input_prompt: Option<String>,
    pub(super) python_primary_prompt: String,
    pub(super) python_continuation_prompt: String,
    pub(super) repl_readline_count: usize,
    pub(super) last_prompt_was_continuation: bool,
    pub(super) waiting_for_input: bool,
    pub(super) exit_requested: bool,
    pub(super) shutdown: bool,
    pub(super) session_end_emitted: bool,
    pub(super) plot_reset_pending: bool,
    pub(super) interrupt_requested: bool,
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(super) turn_write_in_flight: bool,
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(super) turn_cleanup_uncertain: bool,
    #[cfg(target_family = "unix")]
    pub(super) turn_input: PythonTurnInput,
}

#[allow(dead_code)]
pub(super) struct ActiveRequest {
    pub(super) turn_id: Option<u64>,
    pub(super) byte_len: usize,
    pub(super) line_count: usize,
    pub(super) fallback_prompt: Option<String>,
    pub(super) queued_lines: VecDeque<TurnInputLine>,
    pub(super) consumed_lines: usize,
    pub(super) skip_next_hook: bool,
    pub(super) stdin_write_complete: bool,
    pub(super) repl_turn_finished: bool,
    pub(super) started_after_continuation_prompt: bool,
}

pub(super) struct TurnInputLine {
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(super) text: String,
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(super) bytes: Vec<u8>,
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(super) offset: usize,
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(super) input_line_emitted: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum PythonReadlineState {
    Primary,
    Continuation,
    ClientInput,
}

pub(super) enum StdinReadAccounting {
    Accounted,
    #[cfg(target_family = "unix")]
    DiscardedAfterInterrupt,
}

impl StdinReadAccounting {
    pub(super) fn discarded_after_interrupt(&self) -> bool {
        #[cfg(target_family = "unix")]
        {
            matches!(self, Self::DiscardedAfterInterrupt)
        }
        #[cfg(not(target_family = "unix"))]
        {
            false
        }
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
pub(super) enum RawStdinReadError {
    Interrupted,
    Runtime(String),
}

impl SessionState {
    pub(super) fn new() -> Self {
        Self {
            inner: Mutex::new(SessionStateInner {
                active_request: None,
                request_active: false,
                request_completed_at_stdin_wait: false,
                current_prompt: None,
                current_readline_state: None,
                #[cfg(windows)]
                visible_input_prompt: None,
                python_primary_prompt: ">>> ".to_string(),
                python_continuation_prompt: "... ".to_string(),
                repl_readline_count: 0,
                last_prompt_was_continuation: false,
                waiting_for_input: false,
                exit_requested: false,
                shutdown: false,
                session_end_emitted: false,
                plot_reset_pending: false,
                interrupt_requested: false,
                turn_write_in_flight: false,
                turn_cleanup_uncertain: false,
                #[cfg(target_family = "unix")]
                turn_input: PythonTurnInput::new(),
            }),
            cvar: Condvar::new(),
        }
    }
}

pub(super) fn session_state() -> &'static Arc<SessionState> {
    SESSION_STATE
        .get()
        .expect("Python session state was not initialized")
}

pub(super) fn repl_prompt_for(
    current_prompt: Option<String>,
    fallback_prompt: Option<&str>,
    readline_state: Option<PythonReadlineState>,
    primary_prompt: &str,
    continuation_prompt: &str,
) -> String {
    if let Some(prompt) = current_prompt {
        return prompt;
    }
    if fallback_prompt.is_some()
        || matches!(readline_state, Some(PythonReadlineState::Continuation))
    {
        return continuation_prompt.to_string();
    }
    primary_prompt.to_string()
}

#[cfg_attr(target_family = "unix", allow(dead_code))]
pub(super) fn input_hook_prompt(
    guard: &SessionStateInner,
    fallback_prompt: Option<&str>,
) -> String {
    repl_prompt_for(
        guard.current_prompt.clone(),
        fallback_prompt,
        guard.current_readline_state,
        &guard.python_primary_prompt,
        &guard.python_continuation_prompt,
    )
}

pub(super) fn begin_repl_turn() {
    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    let mut guard = state.inner.lock().unwrap();
    guard.repl_readline_count = 0;
}

pub(super) fn set_current_repl_readline_prompt(prompt: &str) -> PythonReadlineState {
    let Some(state) = SESSION_STATE.get() else {
        return PythonReadlineState::Primary;
    };
    let mut guard = state.inner.lock().unwrap();
    let started_after_continuation_prompt = guard
        .active_request
        .as_ref()
        .is_some_and(|active| active.started_after_continuation_prompt);
    let readline_state = if prompt == guard.python_continuation_prompt
        || (prompt.is_empty() && started_after_continuation_prompt)
    {
        PythonReadlineState::Continuation
    } else if guard.repl_readline_count > 0 {
        PythonReadlineState::ClientInput
    } else {
        PythonReadlineState::Primary
    };
    guard.repl_readline_count = guard.repl_readline_count.saturating_add(1);
    guard.current_prompt = if prompt.is_empty() {
        None
    } else {
        Some(prompt.to_string())
    };
    guard.current_readline_state = Some(readline_state);
    readline_state
}

pub(super) fn set_current_readline_prompt(prompt: &str, readline_state: PythonReadlineState) {
    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    let mut guard = state.inner.lock().unwrap();
    guard.current_prompt = Some(prompt.to_string());
    guard.current_readline_state = Some(readline_state);
}

pub(super) fn clear_current_readline_prompt() {
    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    let mut guard = state.inner.lock().unwrap();
    guard.current_prompt = None;
    guard.current_readline_state = None;
}

pub(super) fn remember_emitted_prompt(prompt: &str) {
    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    let mut guard = state.inner.lock().unwrap();
    guard.last_prompt_was_continuation = prompt == guard.python_continuation_prompt;
}

pub(super) fn mark_stdin_wait_prompt_completed_request() {
    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    let mut guard = state.inner.lock().unwrap();
    // An input()/sys.stdin.readline() prompt with no buffered answer is the
    // response boundary for the current MCP request. The Python read can then
    // block while background Python threads keep running. Clear the plot gate at
    // this boundary to prevent those background updates from being attributed to
    // the request that already completed. Callers flush prompt-time plots before
    // closing this gate.
    guard.request_active = false;
    guard.request_completed_at_stdin_wait = true;
}

pub(super) fn request_active() -> bool {
    let Some(state) = SESSION_STATE.get() else {
        return false;
    };
    let guard = state.inner.lock().unwrap();
    guard.request_active && !guard.request_completed_at_stdin_wait
}

#[cfg(target_family = "unix")]
#[cfg_attr(not(test), allow(dead_code))]
fn prompt_wait_can_complete(
    active: &ActiveRequest,
    current_readline_state: Option<PythonReadlineState>,
) -> bool {
    active.consumed_lines >= active.line_count
        || matches!(
            current_readline_state,
            Some(PythonReadlineState::ClientInput | PythonReadlineState::Continuation)
        )
        || active.fallback_prompt.is_some()
}

#[cfg(test)]
mod tests {
    #[cfg(target_family = "unix")]
    use super::*;

    #[cfg(target_family = "unix")]
    fn active_request_for_prompt_wait(
        line_count: usize,
        consumed_lines: usize,
        fallback_prompt: Option<&str>,
    ) -> ActiveRequest {
        ActiveRequest {
            turn_id: None,
            byte_len: 1,
            line_count,
            fallback_prompt: fallback_prompt.map(str::to_string),
            queued_lines: VecDeque::new(),
            consumed_lines,
            skip_next_hook: false,
            stdin_write_complete: true,
            repl_turn_finished: false,
            started_after_continuation_prompt: false,
        }
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn unix_prompt_wait_requires_progress_for_primary_prompt() {
        let active = active_request_for_prompt_wait(3, 1, None);

        assert!(!prompt_wait_can_complete(&active, None));
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn unix_prompt_wait_allows_client_input_prompt() {
        let active = active_request_for_prompt_wait(1, 0, None);

        assert!(prompt_wait_can_complete(
            &active,
            Some(PythonReadlineState::ClientInput)
        ));
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn unix_prompt_wait_allows_continuation_prompt() {
        let active = active_request_for_prompt_wait(2, 1, None);

        assert!(prompt_wait_can_complete(
            &active,
            Some(PythonReadlineState::Continuation)
        ));
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn unix_prompt_wait_requires_progress_for_custom_primary_prompt() {
        let active = active_request_for_prompt_wait(1, 0, None);

        assert!(!prompt_wait_can_complete(
            &active,
            Some(PythonReadlineState::Primary)
        ));
    }
}
