use std::collections::VecDeque;
use std::ffi::{CStr, CString, c_char, c_int, c_long};
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use crate::ipc;
use crate::python_ffi::{GilGuard, ModuleMethod, PyObject, PyPtr, PyThreadState, PythonApi};
use crate::worker_protocol::TextStream;

#[cfg(windows)]
use state::ReadlineKind;
use state::{
    ActiveRequest, PendingCell, PythonExecState, PythonReadlineState, RawStdinReadError,
    SESSION_STATE, SessionState, StdinReadAccounting, clear_current_readline_prompt,
    remember_emitted_prompt, request_active, session_state, set_current_cell_readline_prompt,
    set_current_readline_prompt,
};
#[cfg(not(any(target_family = "unix", windows)))]
use state::{input_hook_prompt, mark_input_wait_completed_request, repl_prompt_for};
use stdio::{PYTHON_STDIN_FILE, PythonRuntime, StdioLineRead, open_python_runtime};
#[cfg(all(not(target_family = "unix"), not(windows)))]
use stdio::{read_stdio_line_bytes, read_stdio_line_bytes_allowing_python_threads};

mod state;
mod stdio;
#[cfg(target_family = "unix")]
mod unix_stdin;
#[cfg(windows)]
mod windows_stdin;

const MCP_REPL_PYTHON: &str = include_str!("../python/embedded.py");

pub struct PythonSession {
    #[cfg(windows)]
    init: Arc<SessionInit>,
}

impl PythonSession {
    #[cfg(windows)]
    pub fn global() -> Result<&'static PythonSession, String> {
        SESSION
            .get()
            .ok_or_else(|| "Python session not initialized".to_string())
    }

    pub fn start_on_current_thread() -> Result<(), String> {
        let init = Arc::new(SessionInit::new());
        let session = PythonSession {
            #[cfg(windows)]
            init: init.clone(),
        };
        if SESSION.set(session).is_err() {
            return Err("Python session already initialized".to_string());
        }
        run_session_on_current_thread(init)
    }

    #[cfg(windows)]
    pub fn wait_until_ready(&self) -> Result<(), String> {
        self.init.wait_ready()
    }

    #[cfg(windows)]
    pub fn begin_input(&self, input: String) -> Result<(), String> {
        self.wait_until_ready()?;
        windows_stdin::begin_tracked_input_batch(input)
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug)]
enum InitState {
    Pending,
    Ready,
    Failed(String),
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

    fn mark_failed(&self, message: String) {
        let mut guard = self.state.lock().unwrap();
        *guard = InitState::Failed(message);
        self.cvar.notify_all();
    }

    #[cfg(windows)]
    fn wait_ready(&self) -> Result<(), String> {
        let mut guard = self.state.lock().unwrap();
        loop {
            match &*guard {
                InitState::Pending => {
                    guard = self.cvar.wait(guard).unwrap();
                }
                InitState::Ready => return Ok(()),
                InitState::Failed(message) => return Err(message.clone()),
            }
        }
    }
}

fn request_exit() {
    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    let mut guard = state.inner.lock().unwrap();
    guard.exit_requested = true;
    state.cvar.notify_all();
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

pub(crate) fn interrupt() {
    interrupt_for_request_generation(None);
}

fn interrupt_for_request_generation(request_generation: Option<u64>) {
    let _ = request_generation;
    discard_pending_stdin();
    #[cfg(target_family = "unix")]
    unix_stdin::flush_terminal_input();
    #[cfg(not(target_family = "unix"))]
    finish_active_request_at_next_read();
    mark_interrupt_requested();
    request_platform_interrupt();
}

fn mark_interrupt_requested() {
    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    let mut guard = state.inner.lock().unwrap();
    guard.interrupt_requested = true;
    state.cvar.notify_all();
}

#[cfg(windows)]
fn request_platform_interrupt() {
    let _ = unsafe { libc::raise(libc::SIGINT) };
}

#[cfg(not(windows))]
fn request_platform_interrupt() {}

fn take_interrupt_requested() -> bool {
    let Some(state) = SESSION_STATE.get() else {
        return false;
    };
    let mut guard = state.inner.lock().unwrap();
    let requested = guard.interrupt_requested;
    guard.interrupt_requested = false;
    requested
}

pub(crate) fn begin_input(input: String) -> Result<(), String> {
    if input.is_empty() {
        return Ok(());
    }

    match input_route()? {
        InputRoute::Cell => return begin_cell_input(input),
        InputRoute::Stdin => {}
    }

    #[cfg(target_family = "unix")]
    {
        unix_stdin::begin_input_batch(&input)
    }

    #[cfg(windows)]
    {
        PythonSession::global()?.begin_input(input)
    }

    #[cfg(not(any(target_family = "unix", windows)))]
    {
        let _ = input;
        Ok(())
    }
}

enum InputRoute {
    Cell,
    Stdin,
}

fn input_route() -> Result<InputRoute, String> {
    let state = session_state();
    let guard = state.inner.lock().unwrap();
    match &guard.exec_state {
        PythonExecState::Idle => Ok(InputRoute::Cell),
        PythonExecState::WaitingInput {
            generation,
            prompt,
            kind,
        } => {
            let _ = (generation, prompt, kind);
            Ok(InputRoute::Stdin)
        }
        PythonExecState::RunningCell { .. } => {
            Err("Python session is already running a cell".to_string())
        }
        PythonExecState::ShuttingDown => Err("Python session is shutting down".to_string()),
    }
}

fn begin_cell_input(input: String) -> Result<(), String> {
    let state = session_state();
    let prompt;
    {
        let mut guard = state.inner.lock().unwrap();
        if !matches!(guard.exec_state, PythonExecState::Idle) {
            return Err("Python session is not idle".to_string());
        }
        #[cfg(target_family = "unix")]
        {
            guard.input_queue.begin_input(Vec::new())?;
        }
        guard.next_generation = guard.next_generation.saturating_add(1);
        let generation = guard.next_generation;
        prompt = guard.python_primary_prompt.clone();
        guard.pending_cell = Some(PendingCell {
            source: input.clone(),
            generation,
        });
        guard.exec_state = PythonExecState::RunningCell { generation };
        guard.request_active = true;
        guard.plot_reset_pending = true;
        guard.interrupt_requested = false;
        guard.waiting_for_input = false;
        guard.current_prompt = None;
        guard.current_readline_state = None;
        guard.active_request = Some(ActiveRequest {
            byte_len: 0,
            line_count: 0,
            fallback_prompt: None,
            queued_lines: VecDeque::new(),
            consumed_lines: 0,
            skip_next_hook: false,
            stdin_write_complete: true,
            repl_turn_finished: false,
            started_after_continuation_prompt: false,
        });
    }
    ipc::emit_input_line(&prompt, &input);
    state.cvar.notify_all();
    Ok(())
}

#[cfg_attr(target_family = "unix", allow(dead_code))]
fn finish_active_request_at_next_read() {
    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    let mut guard = state.inner.lock().unwrap();
    guard.waiting_for_input = false;
    if let Some(active) = guard.active_request.as_mut() {
        #[cfg(windows)]
        active.queued_lines.clear();
        active.line_count = active.consumed_lines.saturating_add(1);
        active.fallback_prompt = None;
        active.skip_next_hook = false;
    }
}

#[cfg(target_family = "unix")]
fn discard_pending_stdin() {
    unix_stdin::discard_pending_stdin();
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
fn discard_pending_stdin() {
    windows_stdin::discard_pending_stdin();
}

#[cfg(not(any(target_family = "unix", windows)))]
fn discard_pending_stdin() {}

fn run_session_on_current_thread(init: Arc<SessionInit>) -> Result<(), String> {
    crate::diagnostics::startup_log("python-session: init begin");
    let state = Arc::new(SessionState::new());
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
    let runtime = match open_python_runtime() {
        Ok(runtime) => runtime,
        Err(err) => {
            init.mark_failed(err.clone());
            return Err(err);
        }
    };

    if let Err(err) = configure_python(api) {
        let _gil = GilGuard::acquire();
        api.print_error();
        init.mark_failed(err.clone());
        return Err(err);
    }

    init.mark_ready();
    ipc::emit_worker_ready("python", plot_capable());

    let result = run_cell_loop(&runtime);
    crate::diagnostics::startup_log("python-session: cell loop exited; finalizing python");
    let finalize_result = finalize_python(api, thread_state);
    match &finalize_result {
        Ok(()) => crate::diagnostics::startup_log("python-session: python finalized"),
        Err(err) => {
            crate::diagnostics::startup_log(format!("python-session: finalize failed: {err}"))
        }
    }
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

fn run_cell_loop(_runtime: &PythonRuntime) -> Result<(), String> {
    let api = PythonApi::global();
    emit_ready_prompt()?;
    loop {
        let Some(cell) = wait_for_pending_cell() else {
            flush_original_stdio();
            return Ok(());
        };
        {
            let _gil = GilGuard::acquire();
            run_python_cell(api, &cell.source);
            capture_python_prompts(api)?;
            flush_original_stdio();
        }
        if take_exit_requested() {
            flush_original_stdio();
            return Ok(());
        }
        emit_plots();
        finish_cell_request(cell.generation);
    }
}

fn emit_ready_prompt() -> Result<(), String> {
    let api = PythonApi::global();
    {
        let _gil = GilGuard::acquire();
        capture_python_prompts(api)?;
    }
    let prompt = {
        let state = session_state();
        let mut guard = state.inner.lock().unwrap();
        guard.exec_state = PythonExecState::Idle;
        guard.waiting_for_input = true;
        guard.request_active = false;
        guard.active_request = None;
        guard.python_primary_prompt.clone()
    };
    remember_emitted_prompt(&prompt);
    ipc::emit_input_wait(&prompt);
    Ok(())
}

fn wait_for_pending_cell() -> Option<PendingCell> {
    let state = session_state();
    let mut guard = state.inner.lock().unwrap();
    loop {
        if guard.shutdown || guard.exit_requested {
            guard.exec_state = PythonExecState::ShuttingDown;
            return None;
        }
        if let Some(cell) = guard.pending_cell.take() {
            return Some(cell);
        }
        guard = state.cvar.wait(guard).unwrap();
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

fn finish_cell_request(generation: u64) {
    let state = session_state();
    let (prompt, active) = {
        let mut guard = state.inner.lock().unwrap();
        match guard.exec_state {
            PythonExecState::RunningCell {
                generation: current,
            }
            | PythonExecState::WaitingInput {
                generation: current,
                ..
            } if current == generation => {
                guard.exec_state = PythonExecState::Idle;
            }
            PythonExecState::ShuttingDown => return,
            _ => {}
        }
        #[cfg(target_family = "unix")]
        {
            let _ = guard.input_queue.take_completed_input();
        }
        guard.current_prompt = None;
        guard.current_readline_state = None;
        guard.waiting_for_input = true;
        guard.request_active = false;
        let active = guard.active_request.take();
        let prompt = guard.python_primary_prompt.clone();
        (prompt, active)
    };
    remember_emitted_prompt(&prompt);
    ipc::emit_input_wait(&prompt);
    complete_active_request(state, active, false);
}

fn capture_python_prompts(api: &'static PythonApi) -> Result<(), String> {
    let main = api.import_module("__main__")?;
    let func = api.get_attr_string(main.as_ptr(), "_mcp_repl_capture_prompts")?;
    let result = unsafe { (api.py_object_call_object)(func.as_ptr(), ptr::null_mut()) };
    let result = PyPtr::from_owned(result, "Python prompt capture failed")?;
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
    #[cfg(target_family = "unix")]
    {
        handle_protocol_input_hook();
    }

    #[cfg(windows)]
    {
        if let Some(state) = SESSION_STATE.get() {
            let mut guard = state.inner.lock().unwrap();
            guard.waiting_for_input = true;
            state.cvar.notify_all();
        }
    }

    #[cfg(not(any(target_family = "unix", windows)))]
    {
        let Some(state) = SESSION_STATE.get() else {
            return;
        };
        let mut completed = None;
        let mut prompt = None;
        let mut emit_readline_prompt = false;
        let mut flush_before_wait = false;
        {
            let mut guard = state.inner.lock().unwrap();
            if guard.shutdown {
                return;
            }
            let current_prompt_from_state = guard.current_prompt.clone();
            let current_readline_state = guard.current_readline_state;
            let primary_prompt = guard.python_primary_prompt.clone();
            let continuation_prompt = guard.python_continuation_prompt.clone();
            let idle_prompt = input_hook_prompt(&guard, None);
            if let Some(active) = guard.active_request.as_mut() {
                let fallback_prompt = if active.repl_turn_finished {
                    None
                } else {
                    active
                        .fallback_prompt
                        .as_deref()
                        .or_else(|| active.started_after_continuation_prompt.then_some(""))
                };
                let current_prompt = repl_prompt_for(
                    current_prompt_from_state.clone(),
                    fallback_prompt,
                    current_readline_state,
                    &primary_prompt,
                    &continuation_prompt,
                );
                if active.skip_next_hook {
                    active.skip_next_hook = false;
                } else {
                    note_input_hook_consumed_line(active);
                }
                let should_complete = if active.repl_turn_finished {
                    request_repl_turn_should_complete(active)
                } else {
                    request_prompt_wait_should_complete(active, current_readline_state)
                };
                guard.waiting_for_input = true;
                if should_complete {
                    prompt = Some(current_prompt);
                    completed = guard.active_request.take();
                } else {
                    flush_before_wait = true;
                }
            } else if !guard.waiting_for_input {
                guard.waiting_for_input = true;
                prompt = Some(idle_prompt);
                emit_readline_prompt = true;
            }
        }

        if flush_before_wait {
            flush_original_stdio();
        } else if let Some(active) = completed {
            emit_plots();
            #[cfg(not(target_family = "unix"))]
            mark_input_wait_completed_request();
            flush_original_stdio();
            let prompt = prompt.as_deref().unwrap_or(">>> ");
            remember_emitted_prompt(prompt);
            ipc::emit_input_wait(prompt);
            complete_active_request(state, Some(active), false);
        } else if emit_readline_prompt {
            let prompt = prompt.as_deref().unwrap_or(">>> ");
            remember_emitted_prompt(prompt);
            ipc::emit_input_wait(prompt);
        }
    }
}

#[cfg(target_family = "unix")]
fn handle_protocol_input_hook() {
    unix_stdin::handle_protocol_input_hook();
}

unsafe extern "C" fn pyos_input_hook() -> c_int {
    handle_input_hook();
    0
}

#[cfg_attr(target_family = "unix", allow(dead_code))]
#[cfg_attr(windows, allow(dead_code))]
fn note_input_hook_consumed_line(active: &mut ActiveRequest) {
    #[cfg(not(target_family = "unix"))]
    {
        active.consumed_lines = active.consumed_lines.saturating_add(1);
    }
    #[cfg(target_family = "unix")]
    {
        let _ = active;
    }
}

#[cfg(not(any(target_family = "unix", windows)))]
fn request_prompt_wait_should_complete(
    active: &ActiveRequest,
    _current_readline_state: Option<PythonReadlineState>,
) -> bool {
    active.consumed_lines >= active.line_count
}

#[cfg(not(any(target_family = "unix", windows)))]
fn request_repl_turn_should_complete(active: &ActiveRequest) -> bool {
    active.consumed_lines >= active.line_count
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
    #[cfg(any(target_family = "unix", windows))]
    let readline_state = set_current_cell_readline_prompt(&prompt_text);
    #[cfg(not(any(target_family = "unix", windows)))]
    set_current_cell_readline_prompt(&prompt_text);
    #[cfg(any(target_family = "unix", windows))]
    // This uses CPython's current readline callback and tracked turn state, not
    // rendered output parsing. Suppress only actual REPL prompts; client-input
    // prompts can intentionally equal sys.ps1/sys.ps2 and must stay visible.
    let suppress_repl_prompt_echo = matches!(
        readline_state,
        PythonReadlineState::Primary | PythonReadlineState::Continuation
    ) && prompt_matches_python_repl_prompt(&prompt_text);
    #[cfg(target_family = "unix")]
    flush_original_stdio();
    #[cfg(all(not(target_family = "unix"), not(windows)))]
    handle_input_hook();

    #[cfg(windows)]
    flush_original_stdio();
    #[cfg(target_family = "unix")]
    let read = match unix_stdin::read_cpython_readline_turn_line(
        &prompt_text,
        !prompt_text.is_empty() && !suppress_repl_prompt_echo,
    ) {
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
    #[cfg(windows)]
    let read = match windows_stdin::read_windows_turn_line(
        &prompt_text,
        ReadlineKind::PyOSReadline,
        !prompt_text.is_empty() && !suppress_repl_prompt_echo,
        false,
    ) {
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
    }
    let accounting = match note_cpython_readline_bytes_read(&prompt_text, &read.bytes) {
        Ok(accounting) => accounting,
        Err(err) => {
            emit_protocol_failure(&err);
            set_callback_error(&err);
            return ptr::null_mut();
        }
    };
    clear_current_readline_prompt();
    if accounting.discarded_after_interrupt() {
        return allocate_readline_result(b"\n");
    }
    if read.interrupted || take_interrupt_requested() {
        PythonApi::global().set_interrupt();
        return ptr::null_mut();
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

fn prompt_matches_python_repl_prompt(prompt: &str) -> bool {
    let Some(state) = SESSION_STATE.get() else {
        return false;
    };
    let guard = state.inner.lock().unwrap();
    prompt == guard.python_primary_prompt || prompt == guard.python_continuation_prompt
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

    set_current_readline_prompt(
        prompt_for_sideband.to_str().unwrap_or(""),
        PythonReadlineState::ClientInput,
    );
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
    #[cfg(windows)]
    let read = match windows_stdin::read_windows_turn_line(
        prompt_for_sideband.to_str().unwrap_or(""),
        ReadlineKind::RawStdin,
        !prompt.is_empty(),
        true,
    ) {
        Ok(read) => read,
        Err(err) => {
            set_callback_error(&err);
            clear_current_readline_prompt();
            return CStdinLine::Error;
        }
    };
    #[cfg(target_family = "unix")]
    let read = match unix_stdin::read_runtime_stdin_line(prompt_for_sideband.to_str().unwrap_or(""))
    {
        Ok(read) => read,
        Err(err) => {
            set_callback_error(&err);
            clear_current_readline_prompt();
            return CStdinLine::Error;
        }
    };
    #[cfg(all(not(target_family = "unix"), not(windows)))]
    let read = read_stdio_line_bytes_allowing_python_threads(stdin);
    if read.interrupted {
        #[cfg(target_family = "unix")]
        unix_stdin::flush_terminal_input();
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
    clear_current_readline_prompt();
    if accounting.discarded_after_interrupt() {
        return CStdinLine::Line("\n".to_string());
    }
    if read.interrupted || take_interrupt_requested() {
        PythonApi::global().set_interrupt();
        return CStdinLine::Error;
    }
    if read.bytes.is_empty() {
        CStdinLine::Eof
    } else {
        CStdinLine::Line(String::from_utf8_lossy(&read.bytes).to_string())
    }
}

#[cfg(target_family = "unix")]
fn read_raw_stdin_bytes(size: usize) -> Result<Vec<u8>, RawStdinReadError> {
    unix_stdin::read_raw_stdin_bytes(size)
}

#[cfg(windows)]
fn read_raw_stdin_bytes(size: usize) -> Result<Vec<u8>, RawStdinReadError> {
    windows_stdin::read_raw_stdin_bytes(size)
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

fn complete_active_request_with_options(
    state: &Arc<SessionState>,
    active: Option<ActiveRequest>,
    emit_session_end: bool,
) {
    if active.is_some() {
        state.cvar.notify_all();
    }
    if emit_session_end {
        ipc::emit_session_end();
    }
}

fn complete_active_request(
    state: &Arc<SessionState>,
    active: Option<ActiveRequest>,
    emit_session_end: bool,
) {
    complete_active_request_with_options(state, active, emit_session_end);
}

fn finish_session_end() {
    let state = session_state();
    let mut guard = state.inner.lock().unwrap();
    let should_emit = !guard.session_end_emitted;
    guard.session_end_emitted = true;
    guard.shutdown = true;
    guard.request_active = false;
    let active = guard.active_request.take();
    drop(guard);
    complete_active_request_with_options(state, active, should_emit);
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
        Err(RawStdinReadError::Interrupted) => {
            api.set_interrupt();
            return ptr::null_mut();
        }
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
