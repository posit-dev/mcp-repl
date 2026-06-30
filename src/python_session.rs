use std::ffi::{CStr, CString, c_char, c_int, c_long};
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use crate::ipc;
use crate::python_ffi::{GilGuard, ModuleMethod, PyObject, PyPtr, PyThreadState, PythonApi};
use crate::worker_protocol::TextStream;

use state::{
    RawStdinReadError, SESSION_STATE, SessionState, StdinReadAccounting,
    mark_input_wait_completed_request, remember_emitted_prompt, request_active, session_state,
};
use stdio::{PYTHON_STDIN_FILE, PythonThreadsAllowed, StdioLineRead, open_python_runtime};
#[cfg(all(not(target_family = "unix"), not(windows)))]
use stdio::{read_stdio_line_bytes, read_stdio_line_bytes_allowing_python_threads};

mod state;
mod stdio;
#[cfg(target_family = "unix")]
mod unix_stdin;

const MCP_REPL_PYTHON: &str = include_str!("../python/embedded.py");
pub struct PythonSession;

impl PythonSession {
    pub fn start_on_current_thread() -> Result<(), String> {
        let init = Arc::new(SessionInit::new());
        let session = PythonSession;
        if SESSION.set(session).is_err() {
            return Err("Python session already initialized".to_string());
        }
        run_session_on_current_thread(init)
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug)]
enum InitState {
    Pending,
    Ready,
    Failed,
}

#[derive(Debug)]
struct SessionInit {
    state: Mutex<InitState>,
    cvar: Condvar,
}

impl SessionInit {
    fn new() -> Self {
        Self {
            state: Mutex::new(InitState::Pending),
            cvar: Condvar::new(),
        }
    }

    fn mark_ready(&self) {
        let mut guard = self.state.lock().unwrap();
        *guard = InitState::Ready;
        self.cvar.notify_all();
    }

    fn mark_failed(&self, _message: String) {
        let mut guard = self.state.lock().unwrap();
        *guard = InitState::Failed;
        self.cvar.notify_all();
    }
}

fn request_exit() {
    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    let mut guard = state.inner.lock().unwrap();
    guard.exit_requested = true;
    state.notify_runtime_input_closed();
}

fn take_exit_requested() -> bool {
    let Some(state) = SESSION_STATE.get() else {
        return false;
    };
    let mut guard = state.inner.lock().unwrap();
    let requested = guard.exit_requested;
    guard.exit_requested = false;
    requested
}

pub(crate) fn discard_unconsumed_input_for_discard_ack() -> bool {
    discard_pending_stdin()
}

pub(crate) fn begin_input(input: String) -> Result<(), String> {
    if input.is_empty() {
        return Ok(());
    }
    let state = session_state();
    let should_record_background_plots = {
        let guard = state.inner.lock().unwrap();
        !guard.request_active
    };
    if should_record_background_plots {
        record_background_plots();
    }
    {
        let mut guard = state.inner.lock().unwrap();
        if guard.shutdown {
            return Err("Python session is shutting down".to_string());
        }
        guard.input_queue.push_payload(input);
        guard.request_active = true;
        guard.plot_reset_pending = true;
    }
    state.notify_runtime_input_available();
    Ok(())
}

pub(crate) fn request_shutdown() {
    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    let mut guard = state.inner.lock().unwrap();
    // Preserve already accepted input; reset replies include output produced
    // while the old worker drains to a safe runtime boundary.
    guard.shutdown = true;
    state.notify_runtime_input_closed();
}

#[cfg(target_family = "unix")]
fn discard_pending_stdin() -> bool {
    discard_queued_input()
}

fn emit_protocol_failure(message: &str) {
    #[cfg(target_family = "unix")]
    {
        unix_stdin::emit_protocol_failure(message);
    }

    #[cfg(not(target_family = "unix"))]
    {
        let _ = message;
    }
}

#[cfg(windows)]
fn discard_pending_stdin() -> bool {
    discard_queued_input()
}

#[cfg(not(any(target_family = "unix", windows)))]
fn discard_pending_stdin() -> bool {
    discard_queued_input()
}

fn discard_queued_input() -> bool {
    let Some(state) = SESSION_STATE.get() else {
        return false;
    };
    let mut guard = state.inner.lock().unwrap();
    let discarded = guard.input_queue.discard_unconsumed_input();
    discarded
}

fn run_session_on_current_thread(init: Arc<SessionInit>) -> Result<(), String> {
    crate::diagnostics::startup_log("python-session: init begin");
    let state = match SessionState::new() {
        Ok(state) => Arc::new(state),
        Err(err) => {
            init.mark_failed(err.clone());
            return Err(err);
        }
    };
    if SESSION_STATE.set(state.clone()).is_err() {
        let message = "Python session state already initialized".to_string();
        init.mark_failed(message.clone());
        return Err(message);
    }

    let runtime_config = match crate::python_runtime::resolve_python_runtime_config() {
        Ok(runtime_config) => runtime_config,
        Err(err) => {
            init.mark_failed(err.clone());
            return Err(err);
        }
    };
    let api = match PythonApi::initialize(&runtime_config.libpython) {
        Ok(api) => api,
        Err(err) => {
            init.mark_failed(err.clone());
            return Err(err);
        }
    };
    let thread_state = match initialize_python(api, &runtime_config.executable) {
        Ok(thread_state) => thread_state,
        Err(err) => {
            init.mark_failed(err.clone());
            return Err(err);
        }
    };
    if thread_state.is_null() {
        let err = "failed to release initialized Python thread state".to_string();
        init.mark_failed(err.clone());
        return Err(err);
    }
    if let Err(err) = open_python_runtime() {
        init.mark_failed(err.clone());
        return Err(err);
    }

    if let Err(err) = configure_python(api) {
        let _gil = GilGuard::acquire();
        api.print_error();
        init.mark_failed(err.clone());
        return Err(err);
    }
    if let Err(err) = configure_python_signal_wakeup_fd(api, &state) {
        let _gil = GilGuard::acquire();
        api.print_error();
        init.mark_failed(err.clone());
        return Err(err);
    }

    init.mark_ready();
    ipc::emit_worker_ready("python", plot_capable());

    let result = run_cell_loop();
    // Py_FinalizeEx follows CPython shutdown semantics, including waiting for
    // user-created non-daemon threads. While that wait is in progress the
    // worker is still alive, so the server must not synthesize session_end.
    crate::diagnostics::startup_log("python-session: cell loop exited; finalizing python");
    let finalize_result = finalize_python(api, thread_state);
    match &finalize_result {
        Ok(()) => crate::diagnostics::startup_log("python-session: python finalized"),
        Err(err) => {
            crate::diagnostics::startup_log(format!("python-session: finalize failed: {err}"))
        }
    }
    crate::diagnostics::startup_log("python-session: emitting session_end");
    finish_session_end();
    crate::diagnostics::startup_log("python-session: emitted session_end");
    result?;
    finalize_result?;
    Ok(())
}

fn initialize_python(
    api: &'static PythonApi,
    executable: &Path,
) -> Result<*mut PyThreadState, String> {
    let module_name = CString::new("_mcp_repl").expect("module name must not contain NUL");
    let module_name = module_name.into_raw();
    let rc = unsafe { (api.py_import_append_inittab)(module_name, initialize_mcp_repl_module) };
    if rc != 0 {
        return Err("failed to register _mcp_repl embedded Python module".to_string());
    }

    unsafe {
        if (api.py_is_initialized)() != 0 {
            return Err("embedded Python interpreter was already initialized".to_string());
        }
        api.set_program_name(executable)?;
        api.set_interactive_flags()?;
        (api.py_initialize_ex)(1);
        api.install_readline_function(mcp_repl_readline)?;
        let thread_state = (api.py_eval_save_thread)();
        api.install_input_hook(pyos_input_hook)?;
        Ok(thread_state)
    }
}

fn configure_python(api: &'static PythonApi) -> Result<(), String> {
    let _gil = GilGuard::acquire();
    let builtins = api.import_module("builtins")?;
    let runtime_error = api.get_attr_string(builtins.as_ptr(), "RuntimeError")?;
    RUNTIME_ERROR.store(runtime_error.as_ptr(), Ordering::SeqCst);
    let _runtime_error = runtime_error.into_raw();

    let main = api.import_module("__main__")?;
    let globals = unsafe { (api.py_module_get_dict)(main.as_ptr()) };
    if globals.is_null() {
        return Err("failed to get __main__ globals".to_string());
    }
    api.run_code(MCP_REPL_PYTHON, globals)?;
    Ok(())
}

#[cfg(target_family = "unix")]
fn configure_python_signal_wakeup_fd(
    api: &'static PythonApi,
    state: &Arc<SessionState>,
) -> Result<(), String> {
    let _gil = GilGuard::acquire();
    let main = api.import_module("__main__")?;
    let globals = unsafe { (api.py_module_get_dict)(main.as_ptr()) };
    if globals.is_null() {
        return Err("failed to get __main__ globals".to_string());
    }
    let code = format!(
        "import signal as _mcp_repl_signal\n_mcp_repl_signal.set_wakeup_fd({}, warn_on_full_buffer=False)\n",
        state.runtime_wake.signal_write_fd()
    );
    api.run_code(&code, globals)?;
    Ok(())
}

#[cfg(not(target_family = "unix"))]
fn configure_python_signal_wakeup_fd(
    _api: &'static PythonApi,
    _state: &Arc<SessionState>,
) -> Result<(), String> {
    Ok(())
}

fn run_cell_loop() -> Result<(), String> {
    let api = PythonApi::global();
    emit_top_level_input_wait()?;
    loop {
        let Some(cell) = wait_for_next_cell()? else {
            flush_original_stdio();
            return Ok(());
        };
        {
            let _gil = GilGuard::acquire();
            clear_python_stdin_buffers(api)?;
            run_python_cell(api, &cell.source);
            capture_python_prompts(api)?;
            flush_original_stdio();
        }
        if take_exit_requested() {
            mark_cell_running(false);
            flush_original_stdio();
            return Ok(());
        }
        emit_plots();
        finish_cell_request()?;
    }
}

struct CellInput {
    source: String,
}

fn emit_top_level_input_wait() -> Result<(), String> {
    let api = PythonApi::global();
    {
        let _gil = GilGuard::acquire();
        capture_python_prompts(api)?;
    }
    {
        let state = session_state();
        let mut guard = state.inner.lock().unwrap();
        guard.request_active = false;
        guard.cell_running = false;
        guard.visible_input_prompt = None;
    }
    ipc::emit_top_level_input_wait();
    Ok(())
}

fn mark_cell_running(running: bool) {
    let state = session_state();
    let mut guard = state.inner.lock().unwrap();
    guard.cell_running = running;
}

fn wait_for_next_cell() -> Result<Option<CellInput>, String> {
    let state = session_state();
    let mut guard = state.inner.lock().unwrap();
    loop {
        if guard.exit_requested {
            return Ok(None);
        }
        drop(guard);
        if check_python_signals_and_print() {
            emit_top_level_input_wait()?;
            guard = state.inner.lock().unwrap();
            continue;
        }
        guard = state.inner.lock().unwrap();
        if !guard.input_queue.has_active_read_consumer()
            && let Some(source) = guard.input_queue.take_cell_payload()
        {
            guard.cell_running = true;
            return Ok(Some(CellInput { source }));
        }
        if guard.shutdown {
            return Ok(None);
        }
        guard = wait_for_queue_notification(state, guard, false);
    }
}

fn check_python_signals() -> bool {
    let api = PythonApi::global();
    let _gil = GilGuard::acquire();
    api.check_signals()
}

fn check_python_signals_and_print() -> bool {
    let api = PythonApi::global();
    let _gil = GilGuard::acquire();
    if api.check_signals() {
        api.print_error();
        true
    } else {
        false
    }
}

fn run_python_cell(api: &'static PythonApi, source: &str) {
    let main = match api.import_module("__main__") {
        Ok(main) => main,
        Err(_) => {
            api.print_error();
            return;
        }
    };
    let func = match api.get_attr_string(main.as_ptr(), "_mcp_repl_run_cell") {
        Ok(func) => func,
        Err(_) => {
            api.print_error();
            return;
        }
    };
    match api.call_one_string_arg(func.as_ptr(), source) {
        Ok(result) => drop(result),
        Err(_) => api.print_error(),
    }
}

fn finish_cell_request() -> Result<(), String> {
    let api = PythonApi::global();
    {
        let _gil = GilGuard::acquire();
        clear_python_stdin_buffers(api)?;
    }
    let state = session_state();
    let emit_top_level_input_wait = {
        let mut guard = state.inner.lock().unwrap();
        guard.cell_running = false;
        guard.input_queue.clear_after_cell_finish();
        if !guard.input_queue.has_active_read_consumer() {
            guard.request_active = false;
            guard.visible_input_prompt = None;
            true
        } else {
            false
        }
    };
    if emit_top_level_input_wait {
        ipc::emit_top_level_input_wait();
    }
    Ok(())
}

fn capture_python_prompts(api: &'static PythonApi) -> Result<(), String> {
    let main = api.import_module("__main__")?;
    let func = api.get_attr_string(main.as_ptr(), "_mcp_repl_capture_prompts")?;
    let result = unsafe { (api.py_object_call_object)(func.as_ptr(), ptr::null_mut()) };
    let result = PyPtr::from_owned(result, "Python prompt capture failed")?;
    drop(result);
    Ok(())
}

fn clear_python_stdin_buffers(api: &'static PythonApi) -> Result<(), String> {
    let main = api.import_module("__main__")?;
    let func = api.get_attr_string(main.as_ptr(), "_mcp_repl_clear_stdin_buffers")?;
    let result = unsafe { (api.py_object_call_object)(func.as_ptr(), ptr::null_mut()) };
    let result = PyPtr::from_owned(result, "Python stdin buffer cleanup failed")?;
    drop(result);
    Ok(())
}

fn finalize_python(
    api: &'static PythonApi,
    thread_state: *mut PyThreadState,
) -> Result<(), String> {
    unsafe {
        (api.py_eval_restore_thread)(thread_state);
        match (api.py_finalize_ex)() {
            0 => Ok(()),
            _ => Err("CPython finalization failed".to_string()),
        }
    }
}

fn set_python_prompts(primary: String, continuation: String) {
    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    let mut guard = state.inner.lock().unwrap();
    guard.python_primary_prompt = primary;
    guard.python_continuation_prompt = continuation;
}

fn handle_input_hook() {
    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    state.notify_python_input_hook();
}

unsafe extern "C" fn pyos_input_hook() -> c_int {
    handle_input_hook();
    0
}

enum QueueReadAction {
    Line {
        bytes: Vec<u8>,
        prompt_already_visible: bool,
        detached_request: bool,
        emit_input_line: bool,
    },
    InputWait {
        prompt: String,
    },
    Interrupted,
    Shutdown,
}

fn wait_for_queue_notification<'a>(
    state: &'a Arc<SessionState>,
    guard: std::sync::MutexGuard<'a, state::SessionStateInner>,
    release_gil_while_waiting: bool,
) -> std::sync::MutexGuard<'a, state::SessionStateInner> {
    #[cfg(target_family = "unix")]
    {
        drop(guard);
        if release_gil_while_waiting {
            let allow_threads = PythonThreadsAllowed::new();
            state
                .runtime_wake
                .wait()
                .expect("Python runtime wake wait failed");
            drop(allow_threads);
        } else {
            state
                .runtime_wake
                .wait()
                .expect("Python runtime wake wait failed");
        }
        state.inner.lock().unwrap()
    }

    #[cfg(not(target_family = "unix"))]
    {
        if release_gil_while_waiting {
            let allow_threads = PythonThreadsAllowed::new();
            let guard = state.cvar.wait(guard).unwrap();
            drop(guard);
            drop(allow_threads);
            state.inner.lock().unwrap()
        } else {
            state.cvar.wait(guard).unwrap()
        }
    }
}

fn release_read_consumer(state: &Arc<SessionState>) {
    let mut guard = state.inner.lock().unwrap();
    guard.input_queue.end_read_consumer();
    state.notify_runtime_input_consumer_released();
}

fn next_queue_line_action(
    state: &Arc<SessionState>,
    prompt: &str,
    prompt_wait_emitted: &mut bool,
    owns_consumer: &mut bool,
    release_gil_while_waiting: bool,
) -> QueueReadAction {
    let mut guard = state.inner.lock().unwrap();
    loop {
        if guard.exit_requested {
            if *owns_consumer {
                guard.input_queue.end_read_consumer();
                *owns_consumer = false;
            }
            state.notify_runtime_input_closed();
            return QueueReadAction::Shutdown;
        }
        if !*owns_consumer {
            if guard.input_queue.begin_read_consumer() {
                *owns_consumer = true;
            } else {
                guard = wait_for_queue_notification(state, guard, release_gil_while_waiting);
                continue;
            }
        }
        drop(guard);
        if check_python_signals() {
            let mut guard = state.inner.lock().unwrap();
            if *owns_consumer {
                guard.input_queue.end_read_consumer();
                *owns_consumer = false;
            }
            state.notify_runtime_input_consumer_released();
            return QueueReadAction::Interrupted;
        }
        guard = state.inner.lock().unwrap();
        if let Some(read) = guard.input_queue.consume_line() {
            let emit_input_line = guard.request_active;
            let prompt_already_visible = guard.visible_input_prompt.as_deref() == Some(prompt);
            let detached_request = !guard.cell_running;
            guard.visible_input_prompt = None;
            guard.request_active = true;
            if *owns_consumer {
                guard.input_queue.end_read_consumer();
                *owns_consumer = false;
                state.notify_runtime_input_consumer_released();
            }
            return QueueReadAction::Line {
                bytes: read.protocol_bytes,
                prompt_already_visible,
                detached_request,
                emit_input_line,
            };
        }
        if guard.shutdown {
            if *owns_consumer {
                guard.input_queue.end_read_consumer();
                *owns_consumer = false;
            }
            state.notify_runtime_input_closed();
            return QueueReadAction::Shutdown;
        }
        if !*prompt_wait_emitted {
            *prompt_wait_emitted = true;
            guard.visible_input_prompt = (!prompt.is_empty()).then(|| prompt.to_string());
            return QueueReadAction::InputWait {
                prompt: prompt.to_string(),
            };
        }
        guard = wait_for_queue_notification(state, guard, release_gil_while_waiting);
    }
}

fn read_queue_line(
    prompt: &str,
    emit_prompt_to_stdout: bool,
    release_gil_while_waiting: bool,
) -> Result<StdioLineRead, String> {
    let state = SESSION_STATE
        .get()
        .ok_or_else(|| "Python session state is not initialized".to_string())?;
    let mut prompt_wait_emitted = false;
    let mut owns_consumer = false;
    loop {
        match next_queue_line_action(
            state,
            prompt,
            &mut prompt_wait_emitted,
            &mut owns_consumer,
            release_gil_while_waiting,
        ) {
            QueueReadAction::Line {
                bytes,
                prompt_already_visible,
                detached_request,
                emit_input_line,
            } => {
                if emit_input_line {
                    ipc::emit_input_line(prompt, &String::from_utf8_lossy(&bytes));
                }
                if emit_prompt_to_stdout && !prompt.is_empty() && !prompt_already_visible {
                    emit_output_text(TextStream::Stdout, prompt.as_bytes());
                }
                if detached_request {
                    complete_detached_read_request();
                }
                return Ok(StdioLineRead {
                    bytes,
                    interrupted: false,
                });
            }
            QueueReadAction::InputWait { prompt } => {
                emit_plots();
                mark_input_wait_completed_request();
                remember_emitted_prompt(&prompt);
                ipc::emit_input_wait(&prompt);
            }
            QueueReadAction::Interrupted => {
                return Ok(StdioLineRead {
                    bytes: Vec::new(),
                    interrupted: true,
                });
            }
            QueueReadAction::Shutdown => {
                return Ok(StdioLineRead {
                    bytes: Vec::new(),
                    interrupted: false,
                });
            }
        }
    }
}

fn read_queue_raw_bytes(size: usize) -> Result<Vec<u8>, RawStdinReadError> {
    if size == 0 {
        return Ok(Vec::new());
    }
    let state = SESSION_STATE.get().ok_or_else(|| {
        RawStdinReadError::Runtime("Python session state is not initialized".to_string())
    })?;
    let mut output = Vec::new();
    let mut prompt_wait_emitted = false;
    let mut owns_consumer = false;
    while output.len() < size {
        let action = {
            let mut guard = state.inner.lock().unwrap();
            loop {
                if guard.exit_requested {
                    if owns_consumer {
                        guard.input_queue.end_read_consumer();
                        state.notify_runtime_input_closed();
                    }
                    return Ok(output);
                }
                if !output.is_empty() {
                    return Ok(output);
                }
                if !owns_consumer {
                    if guard.input_queue.begin_read_consumer() {
                        owns_consumer = true;
                    } else {
                        guard = wait_for_queue_notification(state, guard, true);
                        continue;
                    }
                }
                drop(guard);
                if check_python_signals() {
                    let mut guard = state.inner.lock().unwrap();
                    if owns_consumer {
                        guard.input_queue.end_read_consumer();
                    }
                    state.notify_runtime_input_consumer_released();
                    return Err(RawStdinReadError::Interrupted);
                }
                guard = state.inner.lock().unwrap();
                let remaining = size - output.len();
                if let Some(read) = guard.input_queue.consume_bytes(remaining) {
                    let emit_input_line = guard.request_active;
                    guard.visible_input_prompt = None;
                    guard.request_active = true;
                    if owns_consumer {
                        guard.input_queue.end_read_consumer();
                        owns_consumer = false;
                        state.notify_runtime_input_consumer_released();
                    }
                    break QueueReadAction::Line {
                        bytes: read.protocol_bytes,
                        prompt_already_visible: true,
                        detached_request: !guard.cell_running,
                        emit_input_line,
                    };
                }
                if guard.shutdown {
                    if owns_consumer {
                        guard.input_queue.end_read_consumer();
                        state.notify_runtime_input_closed();
                    }
                    return Ok(output);
                }
                if !prompt_wait_emitted {
                    prompt_wait_emitted = true;
                    guard.visible_input_prompt = None;
                    break QueueReadAction::InputWait {
                        prompt: String::new(),
                    };
                }
                guard = wait_for_queue_notification(state, guard, true);
            }
        };

        match action {
            QueueReadAction::Line {
                bytes,
                detached_request,
                emit_input_line,
                ..
            } => {
                if emit_input_line {
                    ipc::emit_input_line("", &String::from_utf8_lossy(&bytes));
                }
                output.extend(bytes);
                if detached_request {
                    complete_detached_read_request();
                }
            }
            QueueReadAction::InputWait { prompt } => {
                emit_plots();
                mark_input_wait_completed_request();
                remember_emitted_prompt(&prompt);
                ipc::emit_input_wait(&prompt);
            }
            QueueReadAction::Interrupted => return Err(RawStdinReadError::Interrupted),
            QueueReadAction::Shutdown => return Ok(output),
        }
    }
    if owns_consumer {
        release_read_consumer(state);
    }
    Ok(output)
}

fn complete_detached_read_request() {
    let state = session_state();
    {
        let mut guard = state.inner.lock().unwrap();
        guard.request_active = false;
        guard.visible_input_prompt = None;
    }
    ipc::emit_top_level_input_wait();
}

unsafe extern "C" fn mcp_repl_readline(
    stdin: *mut libc::FILE,
    _stdout: *mut libc::FILE,
    prompt: *const c_char,
) -> *mut c_char {
    #[cfg(any(target_family = "unix", windows))]
    let _ = stdin;
    let prompt_text = if prompt.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(prompt) }
            .to_string_lossy()
            .into_owned()
    };
    #[cfg(target_family = "unix")]
    if ipc::worker_ipc_disabled_for_process() {
        return allocate_readline_result(&[]);
    }
    #[cfg(target_family = "unix")]
    flush_original_stdio();
    #[cfg(all(not(target_family = "unix"), not(windows)))]
    handle_input_hook();

    #[cfg(windows)]
    flush_original_stdio();
    #[cfg(any(target_family = "unix", windows))]
    let read = match read_queue_line(&prompt_text, !prompt_text.is_empty(), false) {
        Ok(read) => read,
        Err(err) => {
            emit_output_text(TextStream::Stderr, err.as_bytes());
            ipc::emit_session_end_with_reason("protocol_error");
            request_exit();
            StdioLineRead {
                bytes: Vec::new(),
                interrupted: true,
            }
        }
    };
    #[cfg(all(not(target_family = "unix"), not(windows)))]
    let read = read_stdio_line_bytes(stdin);
    if read.interrupted {
        #[cfg(target_family = "unix")]
        unix_stdin::flush_terminal_input();
        return ptr::null_mut();
    }
    let accounting = match note_cpython_readline_bytes_read(&prompt_text, &read.bytes) {
        Ok(accounting) => accounting,
        Err(err) => {
            emit_protocol_failure(&err);
            set_callback_error(&err);
            return ptr::null_mut();
        }
    };
    if accounting.discarded_after_runtime_interrupt() {
        return allocate_readline_result(b"\n");
    }
    allocate_readline_result(&read.bytes)
}

fn allocate_readline_result(bytes: &[u8]) -> *mut c_char {
    let api = PythonApi::global();
    let result = unsafe { (api.py_mem_raw_malloc)(bytes.len().saturating_add(1)) }.cast::<c_char>();
    if result.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr().cast::<c_char>(), result, bytes.len());
        *result.add(bytes.len()) = 0;
    }
    result
}

#[cfg(target_family = "unix")]
fn note_cpython_readline_bytes_read(
    prompt: &str,
    bytes: &[u8],
) -> Result<StdinReadAccounting, String> {
    unix_stdin::note_cpython_readline_bytes_read(prompt, bytes)
}

#[cfg(not(target_family = "unix"))]
fn note_cpython_readline_bytes_read(
    _prompt: &str,
    bytes: &[u8],
) -> Result<StdinReadAccounting, String> {
    note_stdin_line_read("", bytes)
}

enum CStdinLine {
    Line(String),
    Eof,
    Error,
}

fn read_c_stdin_line(prompt: &str) -> CStdinLine {
    #[cfg(target_family = "unix")]
    if ipc::worker_ipc_disabled_for_process() {
        return unix_stdin::fork_child_stdin_eof(prompt);
    }

    let stdin = PYTHON_STDIN_FILE.load(Ordering::SeqCst);
    if stdin.is_null() {
        set_callback_error("Python stdio files are not initialized");
        return CStdinLine::Error;
    }

    let prompt_for_sideband = match CString::new(prompt) {
        Ok(prompt) => prompt,
        Err(err) => {
            set_callback_error(&format!("readline prompt contains NUL: {err}"));
            return CStdinLine::Error;
        }
    };

    #[cfg(target_family = "unix")]
    flush_original_stdio();
    #[cfg(all(not(target_family = "unix"), not(windows)))]
    {
        flush_original_stdio();
        handle_input_hook();
        emit_output_text(TextStream::Stdout, prompt.as_bytes());
    }
    #[cfg(windows)]
    flush_original_stdio();
    #[cfg(any(target_family = "unix", windows))]
    let read = match read_queue_line(
        prompt_for_sideband.to_str().unwrap_or(""),
        !prompt.is_empty(),
        true,
    ) {
        Ok(read) => read,
        Err(err) => {
            set_callback_error(&err);
            return CStdinLine::Error;
        }
    };
    #[cfg(all(not(target_family = "unix"), not(windows)))]
    let read = read_stdio_line_bytes_allowing_python_threads(stdin);
    if read.interrupted {
        #[cfg(target_family = "unix")]
        unix_stdin::flush_terminal_input();
        return CStdinLine::Error;
    }
    let accounting =
        match note_stdin_line_read(prompt_for_sideband.to_str().unwrap_or(""), &read.bytes) {
            Ok(accounting) => accounting,
            Err(err) => {
                emit_protocol_failure(&err);
                set_callback_error(&err);
                return CStdinLine::Error;
            }
        };
    if accounting.discarded_after_runtime_interrupt() {
        return CStdinLine::Line("\n".to_string());
    }
    if read.bytes.is_empty() {
        CStdinLine::Eof
    } else {
        CStdinLine::Line(String::from_utf8_lossy(&read.bytes).to_string())
    }
}

#[cfg(target_family = "unix")]
fn read_raw_stdin_bytes(size: usize) -> Result<Vec<u8>, RawStdinReadError> {
    if ipc::worker_ipc_disabled_for_process() {
        return Ok(Vec::new());
    }
    read_queue_raw_bytes(size)
}

#[cfg(windows)]
fn read_raw_stdin_bytes(size: usize) -> Result<Vec<u8>, RawStdinReadError> {
    read_queue_raw_bytes(size)
}

#[cfg(not(any(target_family = "unix", windows)))]
fn read_raw_stdin_bytes(_size: usize) -> Result<Vec<u8>, RawStdinReadError> {
    Ok(Vec::new())
}

#[cfg(target_family = "unix")]
fn note_stdin_line_read(prompt: &str, bytes: &[u8]) -> Result<StdinReadAccounting, String> {
    unix_stdin::note_stdin_line_read(prompt, bytes)
}

#[cfg(not(target_family = "unix"))]
fn note_stdin_line_read(_prompt: &str, _bytes: &[u8]) -> Result<StdinReadAccounting, String> {
    Ok(StdinReadAccounting::Accounted)
}

fn plot_capable() -> bool {
    let _gil = GilGuard::acquire();
    let api = PythonApi::global();
    let Ok(main) = api.import_module("__main__") else {
        return false;
    };
    let Ok(func) = api.get_attr_string(main.as_ptr(), "_mcp_repl_plot_capable") else {
        api.clear_error();
        return false;
    };
    let result = unsafe { (api.py_object_call_object)(func.as_ptr(), ptr::null_mut()) };
    let Ok(result) = PyPtr::from_owned(result, "plot capability call failed") else {
        api.clear_error();
        return false;
    };
    unsafe { (api.py_object_is_true)(result.as_ptr()) == 1 }
}

fn emit_plots() {
    if !request_active() {
        return;
    }
    let _gil = GilGuard::acquire();
    let api = PythonApi::global();
    let Ok(main) = api.import_module("__main__") else {
        api.clear_error();
        return;
    };
    let Ok(func) = api.get_attr_string(main.as_ptr(), "_mcp_repl_emit_plots") else {
        api.clear_error();
        return;
    };
    let result = unsafe { (api.py_object_call_object)(func.as_ptr(), ptr::null_mut()) };
    if result.is_null() {
        api.clear_error();
    } else {
        drop(PyPtr::from_owned(result, "plot emission result"));
    }
}

#[cfg(target_family = "unix")]
fn record_background_plots() {
    let _gil = GilGuard::acquire();
    let api = PythonApi::global();
    let Ok(main) = api.import_module("__main__") else {
        return;
    };
    let Ok(func) = api.get_attr_string(main.as_ptr(), "_mcp_repl_record_background_plots") else {
        return;
    };
    let result = unsafe { (api.py_object_call_object)(func.as_ptr(), ptr::null_mut()) };
    if let Ok(result) = PyPtr::from_owned(result, "Python background plot recording failed") {
        drop(result);
    }
}

#[cfg(not(target_family = "unix"))]
fn record_background_plots() {}

fn flush_original_stdio() {
    {
        let _gil = GilGuard::acquire();
        let api = PythonApi::global();
        let Ok(main) = api.import_module("__main__") else {
            api.clear_error();
            unsafe {
                libc::fflush(ptr::null_mut());
            }
            return;
        };
        let Ok(func) = api.get_attr_string(main.as_ptr(), "_mcp_repl_flush_original_stdio") else {
            api.clear_error();
            unsafe {
                libc::fflush(ptr::null_mut());
            }
            return;
        };
        let result = unsafe { (api.py_object_call_object)(func.as_ptr(), ptr::null_mut()) };
        if result.is_null() {
            api.clear_error();
        } else {
            drop(PyPtr::from_owned(result, "original stdio flush result"));
        }
    }
    unsafe {
        libc::fflush(ptr::null_mut());
    }
}

fn finish_session_end() {
    let state = session_state();
    let mut guard = state.inner.lock().unwrap();
    let should_emit = !guard.session_end_emitted;
    guard.session_end_emitted = true;
    guard.shutdown = true;
    guard.request_active = false;
    guard.cell_running = false;
    guard.input_queue.clear_for_session_end();
    drop(guard);
    state.notify_runtime_input_closed();
    if should_emit {
        ipc::emit_session_end();
    }
}

fn emit_output_text(stream: TextStream, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    match ipc::emit_output_text(stream, bytes) {
        Ok(()) => {}
        Err(_) if ipc::worker_ipc_disabled_for_process() => match stream {
            TextStream::Stdout => crate::output_stream::write_stdout_bytes(bytes),
            TextStream::Stderr => crate::output_stream::write_stderr_bytes(bytes),
        },
        Err(err) => panic!("failed to send Python output over worker IPC: {err}"),
    }
}

unsafe extern "C" fn initialize_mcp_repl_module() -> *mut PyObject {
    let api = PythonApi::global();
    let methods = [
        ModuleMethod {
            name: "readline",
            function: py_readline,
        },
        ModuleMethod {
            name: "write",
            function: py_write,
        },
        ModuleMethod {
            name: "write_bytes",
            function: py_write_bytes,
        },
        ModuleMethod {
            name: "raw_stdin_read",
            function: py_raw_stdin_read,
        },
        ModuleMethod {
            name: "restore_readline_function",
            function: py_restore_readline_function,
        },
        ModuleMethod {
            name: "request_exit",
            function: py_request_exit,
        },
        ModuleMethod {
            name: "emit_plot_image",
            function: py_emit_output_image,
        },
        ModuleMethod {
            name: "set_python_prompts",
            function: py_set_python_prompts,
        },
        ModuleMethod {
            name: "has_request_active",
            function: py_has_request_active,
        },
        ModuleMethod {
            name: "take_plot_reset_pending",
            function: py_take_plot_reset_pending,
        },
    ];
    api.create_module("_mcp_repl", &methods)
}

unsafe extern "C" fn py_readline(_self: *mut PyObject, args: *mut PyObject) -> *mut PyObject {
    let api = PythonApi::global();
    if api.tuple_size(args) != 1 {
        set_callback_error("readline expects exactly one argument");
        return ptr::null_mut();
    }
    let Some(prompt) = api.unicode_arg(args, 0) else {
        return ptr::null_mut();
    };
    match read_c_stdin_line(&prompt) {
        CStdinLine::Line(line) => match api.unicode(&line) {
            Ok(value) => value.into_raw(),
            Err(_) => ptr::null_mut(),
        },
        CStdinLine::Eof => api.none(),
        CStdinLine::Error => ptr::null_mut(),
    }
}

unsafe extern "C" fn py_write(_self: *mut PyObject, args: *mut PyObject) -> *mut PyObject {
    let api = PythonApi::global();
    if api.tuple_size(args) != 2 {
        set_callback_error("write expects exactly two arguments");
        return ptr::null_mut();
    }
    let Some(stream) = api.unicode_arg(args, 0) else {
        return ptr::null_mut();
    };
    let Some(message) = api.unicode_arg(args, 1) else {
        return ptr::null_mut();
    };
    let stream = match stream.as_str() {
        "stdout" => TextStream::Stdout,
        "stderr" => TextStream::Stderr,
        _ => {
            set_callback_error("write stream must be 'stdout' or 'stderr'");
            return ptr::null_mut();
        }
    };
    emit_output_text(stream, message.as_bytes());
    api.long_result(message.chars().count() as c_long)
}

unsafe extern "C" fn py_write_bytes(_self: *mut PyObject, args: *mut PyObject) -> *mut PyObject {
    let api = PythonApi::global();
    if api.tuple_size(args) != 2 {
        set_callback_error("write_bytes expects exactly two arguments");
        return ptr::null_mut();
    }
    let Some(stream) = api.unicode_arg(args, 0) else {
        return ptr::null_mut();
    };
    let Some(bytes) = api.bytes_arg(args, 1) else {
        return ptr::null_mut();
    };
    let stream = match stream.as_str() {
        "stdout" => TextStream::Stdout,
        "stderr" => TextStream::Stderr,
        _ => {
            set_callback_error("write_bytes stream must be 'stdout' or 'stderr'");
            return ptr::null_mut();
        }
    };
    emit_output_text(stream, &bytes);
    api.long_result(bytes.len() as c_long)
}

unsafe extern "C" fn py_raw_stdin_read(_self: *mut PyObject, args: *mut PyObject) -> *mut PyObject {
    let api = PythonApi::global();
    if api.tuple_size(args) != 1 {
        set_callback_error("raw_stdin_read expects exactly one argument");
        return ptr::null_mut();
    }
    let Some(size) = api.long_arg(args, 0) else {
        return ptr::null_mut();
    };
    let Ok(size) = usize::try_from(size) else {
        set_callback_error("raw_stdin_read size must be non-negative");
        return ptr::null_mut();
    };
    let bytes = match read_raw_stdin_bytes(size) {
        Ok(bytes) => bytes,
        Err(RawStdinReadError::Interrupted) => return ptr::null_mut(),
        Err(RawStdinReadError::Runtime(message)) => {
            set_callback_error(&message);
            return ptr::null_mut();
        }
    };
    match api.bytes(&bytes) {
        Ok(value) => value.into_raw(),
        Err(_) => ptr::null_mut(),
    }
}

unsafe extern "C" fn py_restore_readline_function(
    _self: *mut PyObject,
    args: *mut PyObject,
) -> *mut PyObject {
    let api = PythonApi::global();
    if api.tuple_size(args) != 0 {
        set_callback_error("restore_readline_function expects no arguments");
        return ptr::null_mut();
    }
    if let Err(err) = api.install_readline_function(mcp_repl_readline) {
        set_callback_error(&err);
        return ptr::null_mut();
    }
    api.none()
}

unsafe extern "C" fn py_request_exit(_self: *mut PyObject, args: *mut PyObject) -> *mut PyObject {
    let api = PythonApi::global();
    if api.tuple_size(args) != 0 {
        set_callback_error("request_exit expects no arguments");
        return ptr::null_mut();
    }
    request_exit();
    api.none()
}

unsafe extern "C" fn py_emit_output_image(
    _self: *mut PyObject,
    args: *mut PyObject,
) -> *mut PyObject {
    let api = PythonApi::global();
    if api.tuple_size(args) != 4 {
        set_callback_error("emit_plot_image expects exactly four arguments");
        return ptr::null_mut();
    }
    let Some(mime_type) = api.unicode_arg(args, 0) else {
        return ptr::null_mut();
    };
    let Some(data) = api.unicode_arg(args, 1) else {
        return ptr::null_mut();
    };
    let is_update = unsafe { (api.py_tuple_get_item)(args, 2) };
    if is_update.is_null() {
        return ptr::null_mut();
    }
    let is_update = unsafe { (api.py_object_is_true)(is_update) };
    if is_update < 0 {
        return ptr::null_mut();
    }
    let Some(source) = api.unicode_arg(args, 3) else {
        return ptr::null_mut();
    };
    ipc::emit_output_image(&mime_type, &data, is_update == 1, Some(&source));
    api.none()
}

unsafe extern "C" fn py_set_python_prompts(
    _self: *mut PyObject,
    args: *mut PyObject,
) -> *mut PyObject {
    let api = PythonApi::global();
    if api.tuple_size(args) != 2 {
        set_callback_error("set_python_prompts expects exactly two arguments");
        return ptr::null_mut();
    }
    let Some(primary) = api.unicode_arg(args, 0) else {
        return ptr::null_mut();
    };
    let Some(continuation) = api.unicode_arg(args, 1) else {
        return ptr::null_mut();
    };
    set_python_prompts(primary, continuation);
    api.none()
}

unsafe extern "C" fn py_has_request_active(
    _self: *mut PyObject,
    _args: *mut PyObject,
) -> *mut PyObject {
    PythonApi::global().bool_result(request_active())
}

unsafe extern "C" fn py_take_plot_reset_pending(
    _self: *mut PyObject,
    _args: *mut PyObject,
) -> *mut PyObject {
    let Some(state) = SESSION_STATE.get() else {
        return PythonApi::global().bool_result(false);
    };
    let mut guard = state.inner.lock().unwrap();
    let pending = guard.plot_reset_pending;
    guard.plot_reset_pending = false;
    PythonApi::global().bool_result(pending)
}

fn set_callback_error(message: &str) {
    let exception = RUNTIME_ERROR.load(Ordering::SeqCst);
    if exception.is_null() {
        return;
    }
    PythonApi::global().set_runtime_error(exception, message);
}

static SESSION: OnceLock<PythonSession> = OnceLock::new();
static RUNTIME_ERROR: AtomicPtr<PyObject> = AtomicPtr::new(ptr::null_mut());
