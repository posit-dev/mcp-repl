use std::sync::{Arc, Condvar, Mutex, OnceLock};

use crate::python_input_queue::PythonInputQueue;

pub(super) static SESSION_STATE: OnceLock<Arc<SessionState>> = OnceLock::new();

pub(super) struct SessionState {
    pub(super) inner: Mutex<SessionStateInner>,
    pub(super) cvar: Condvar,
}

pub(super) struct SessionStateInner {
    pub(super) input_queue: PythonInputQueue,
    pub(super) request_active: bool,
    pub(super) cell_running: bool,
    pub(super) visible_input_prompt: Option<String>,
    pub(super) python_primary_prompt: String,
    pub(super) python_continuation_prompt: String,
    pub(super) last_prompt_was_continuation: bool,
    pub(super) exit_requested: bool,
    pub(super) shutdown: bool,
    pub(super) session_end_emitted: bool,
    pub(super) plot_reset_pending: bool,
}

pub(super) enum StdinReadAccounting {
    Accounted,
}

impl StdinReadAccounting {
    pub(super) fn discarded_after_interrupt(&self) -> bool {
        false
    }
}

pub(super) enum RawStdinReadError {
    Runtime(String),
}

impl SessionState {
    pub(super) fn new() -> Self {
        Self {
            inner: Mutex::new(SessionStateInner {
                input_queue: PythonInputQueue::new(),
                request_active: false,
                cell_running: false,
                visible_input_prompt: None,
                python_primary_prompt: ">>> ".to_string(),
                python_continuation_prompt: "... ".to_string(),
                last_prompt_was_continuation: false,
                exit_requested: false,
                shutdown: false,
                session_end_emitted: false,
                plot_reset_pending: false,
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

pub(super) fn request_active() -> bool {
    let Some(state) = SESSION_STATE.get() else {
        return false;
    };
    let guard = state.inner.lock().unwrap();
    guard.request_active
}

pub(super) fn remember_emitted_prompt(prompt: &str) {
    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    let mut guard = state.inner.lock().unwrap();
    guard.last_prompt_was_continuation = prompt == guard.python_continuation_prompt;
}

pub(super) fn mark_input_wait_completed_request() {
    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    let mut guard = state.inner.lock().unwrap();
    guard.request_active = false;
}
