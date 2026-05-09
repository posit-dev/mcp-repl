use std::ffi::{CStr, CString, c_int, c_long};
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, mpsc};

use crate::ipc;
use crate::python_ffi::{GilGuard, ModuleMethod, PyObject, PyPtr, PythonApi};
use crate::worker_protocol::TextStream;

pub const PYTHON_LIB_ENV: &str = "MCP_REPL_PYTHON_LIB";
const MCP_REPL_PYTHON: &str = include_str!("../python/embedded.py");
const PYTHON_EOF: c_int = 11;

#[derive(Debug)]
pub struct RequestCompleted;

pub struct PythonSession {
    init: Arc<SessionInit>,
}

impl PythonSession {
    pub fn global() -> Result<&'static PythonSession, String> {
        SESSION
            .get()
            .ok_or_else(|| "Python session not initialized".to_string())
    }

    pub fn start_on_current_thread() -> Result<(), String> {
        let init = Arc::new(SessionInit::new());
        let session = PythonSession { init: init.clone() };
        if SESSION.set(session).is_err() {
            return Err("Python session already initialized".to_string());
        }
        run_session_on_current_thread(init)
    }

    pub fn wait_until_ready(&self) -> Result<(), String> {
        self.init.wait_ready()
    }

    pub fn begin_request(
        &self,
        line_count: usize,
    ) -> Result<mpsc::Receiver<RequestCompleted>, String> {
        self.wait_until_ready()?;
        let (reply_tx, reply_rx) = mpsc::channel();
        begin_tracked_request(line_count, reply_tx)?;
        Ok(reply_rx)
    }
}

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

struct PythonRuntime {
    stdin: *mut libc::FILE,
}

pub fn request_shutdown() -> bool {
    let Some(state) = SESSION_STATE.get() else {
        return false;
    };
    let mut guard = state.inner.lock().unwrap();
    guard.shutdown = true;
    state.cvar.notify_all();
    true
}

pub(crate) fn interrupt() {
    discard_pending_stdin();
    finish_active_request_at_next_read();
    if let Some(api) = PythonApi::try_global() {
        unsafe { (api.py_err_set_interrupt)() };
    }
}

fn finish_active_request_at_next_read() {
    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    let mut guard = state.inner.lock().unwrap();
    if let Some(active) = guard.active_request.as_mut() {
        active.line_count = active.consumed_lines.saturating_add(1);
        active.skip_next_hook = false;
        guard.waiting_for_input = false;
    }
}

#[cfg(target_family = "unix")]
fn discard_pending_stdin() {
    let stdin = PYTHON_STDIN_FILE.load(Ordering::SeqCst);
    if !stdin.is_null() {
        unsafe { purge_stdio_input(stdin) };
    }
    drain_stdin_fd();
}

#[cfg(not(target_family = "unix"))]
fn discard_pending_stdin() {}

#[cfg(target_os = "linux")]
unsafe fn purge_stdio_input(file: *mut libc::FILE) {
    unsafe extern "C" {
        fn __fpurge(stream: *mut libc::FILE);
    }
    unsafe { __fpurge(file) };
}

#[cfg(all(target_family = "unix", not(target_os = "linux")))]
unsafe fn purge_stdio_input(file: *mut libc::FILE) {
    unsafe extern "C" {
        fn fpurge(stream: *mut libc::FILE) -> libc::c_int;
    }
    let _ = unsafe { fpurge(file) };
}

#[cfg(target_family = "unix")]
fn drain_stdin_fd() {
    let flags = unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_GETFL) };
    if flags < 0 {
        return;
    }
    let set_nonblocking =
        unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if set_nonblocking < 0 {
        return;
    }

    let mut buffer = [0u8; 8192];
    loop {
        let n = unsafe { libc::read(libc::STDIN_FILENO, buffer.as_mut_ptr().cast(), buffer.len()) };
        if n > 0 {
            continue;
        }
        break;
    }

    let _ = unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_SETFL, flags) };
}

fn run_session_on_current_thread(init: Arc<SessionInit>) -> Result<(), String> {
    crate::diagnostics::startup_log("python-session: init begin");
    let state = Arc::new(SessionState::new());
    if SESSION_STATE.set(state.clone()).is_err() {
        let message = "Python session state already initialized".to_string();
        init.mark_failed(message.clone());
        return Err(message);
    }

    let lib_path = match python_library_path() {
        Ok(lib_path) => lib_path,
        Err(err) => {
            init.mark_failed(err.clone());
            return Err(err);
        }
    };
    let api = match PythonApi::initialize(&lib_path) {
        Ok(api) => api,
        Err(err) => {
            init.mark_failed(err.clone());
            return Err(err);
        }
    };
    if let Err(err) = initialize_python(api) {
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
        api.print_error();
        init.mark_failed(err.clone());
        return Err(err);
    }

    init.mark_ready();
    ipc::emit_backend_info(plot_capable());

    let result = run_repl(&runtime);
    crate::diagnostics::startup_log("python-session: repl exited");
    result
}

fn open_python_runtime() -> Result<PythonRuntime, String> {
    let stdin = open_stdio_file(0, c"r")?;
    let stdout = open_stdio_file(1, c"w")?;
    PYTHON_STDIN_FILE.store(stdin, Ordering::SeqCst);
    PYTHON_STDOUT_FILE.store(stdout, Ordering::SeqCst);
    Ok(PythonRuntime { stdin })
}

fn open_stdio_file(fd: libc::c_int, mode: &CStr) -> Result<*mut libc::FILE, String> {
    let file = unsafe { libc::fdopen(fd, mode.as_ptr()) };
    if file.is_null() {
        Err(format!(
            "failed to open worker fd {fd} as C stdio FILE: {}",
            std::io::Error::last_os_error()
        ))
    } else {
        Ok(file)
    }
}

fn python_library_path() -> Result<PathBuf, String> {
    std::env::var_os(PYTHON_LIB_ENV)
        .map(PathBuf::from)
        .ok_or_else(|| format!("{PYTHON_LIB_ENV} is not set"))
}

fn initialize_python(api: &'static PythonApi) -> Result<(), String> {
    let module_name = CString::new("_mcp_repl").expect("module name must not contain NUL");
    let module_name = module_name.into_raw();
    let rc = unsafe { (api.py_import_append_inittab)(module_name, initialize_mcp_repl_module) };
    if rc != 0 {
        return Err("failed to register _mcp_repl embedded Python module".to_string());
    }

    unsafe {
        if (api.py_is_initialized)() == 0 {
            api.set_interactive_flags()?;
            (api.py_initialize_ex)(1);
            (api.py_eval_save_thread)();
        }
    }
    api.install_input_hook(pyos_input_hook)?;
    Ok(())
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

fn run_repl(runtime: &PythonRuntime) -> Result<(), String> {
    let api = PythonApi::global();
    loop {
        let status = {
            let _gil = GilGuard::acquire();
            unsafe {
                (api.py_run_interactive_one_flags)(
                    runtime.stdin,
                    c"<stdin>".as_ptr(),
                    ptr::null_mut(),
                )
            }
        };
        emit_plots();
        if status == PYTHON_EOF {
            finish_session_end();
            return Ok(());
        }
    }
}

fn begin_tracked_request(
    line_count: usize,
    reply: mpsc::Sender<RequestCompleted>,
) -> Result<(), String> {
    let state = session_state();
    if line_count == 0 {
        let _ = reply.send(RequestCompleted);
        return Ok(());
    }

    let mut guard = state.inner.lock().unwrap();
    while guard.active_request.is_some() && !guard.shutdown {
        guard = state.cvar.wait(guard).unwrap();
    }
    if guard.shutdown {
        return Err("Python session is shutting down".to_string());
    }

    let skip_next_hook = !guard.waiting_for_input;
    guard.waiting_for_input = false;
    guard.active_request = Some(ActiveRequest {
        reply,
        line_count,
        consumed_lines: 0,
        skip_next_hook,
    });
    guard.plot_reset_pending = true;
    state.cvar.notify_all();
    Ok(())
}

fn input_hook_prompt(guard: &SessionStateInner) -> String {
    guard
        .current_prompt
        .clone()
        .unwrap_or_else(|| ">>> ".to_string())
}

fn handle_input_hook() {
    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    let mut completed = None;
    let mut prompt = None;
    let mut emit_idle = false;
    {
        let mut guard = state.inner.lock().unwrap();
        if guard.shutdown {
            return;
        }
        let current_prompt = input_hook_prompt(&guard);
        if let Some(active) = guard.active_request.as_mut() {
            if active.skip_next_hook {
                active.skip_next_hook = false;
            } else {
                active.consumed_lines = active.consumed_lines.saturating_add(1);
            }
            let should_complete = active.consumed_lines >= active.line_count;
            guard.waiting_for_input = true;
            if should_complete {
                prompt = Some(current_prompt);
                completed = guard.active_request.take();
            }
        } else if !guard.waiting_for_input {
            guard.waiting_for_input = true;
            prompt = Some(current_prompt);
            emit_idle = true;
        }
    }

    if let Some(active) = completed {
        ipc::emit_readline_start(prompt.as_deref().unwrap_or(">>> "), true);
        complete_active_request(state, Some(active), false);
    } else if emit_idle {
        ipc::emit_readline_start(prompt.as_deref().unwrap_or(">>> "), false);
    }
}

unsafe extern "C" fn pyos_input_hook() -> c_int {
    handle_input_hook();
    0
}

fn set_current_readline_prompt(prompt: &str) {
    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    let mut guard = state.inner.lock().unwrap();
    guard.current_prompt = Some(prompt.to_string());
}

fn clear_current_readline_prompt() {
    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    let mut guard = state.inner.lock().unwrap();
    guard.current_prompt = None;
}

enum CStdinLine {
    Line(String),
    Eof,
    Error,
}

fn read_c_stdin_line(prompt: &str) -> CStdinLine {
    let api = PythonApi::global();
    let stdin = PYTHON_STDIN_FILE.load(Ordering::SeqCst);
    let stdout = PYTHON_STDOUT_FILE.load(Ordering::SeqCst);
    if stdin.is_null() || stdout.is_null() {
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

    set_current_readline_prompt(prompt_for_sideband.to_str().unwrap_or(""));
    let ptr = unsafe { (api.py_os_readline)(stdin, stdout, c"".as_ptr()) };
    clear_current_readline_prompt();
    if ptr.is_null() {
        return CStdinLine::Error;
    }
    let bytes = unsafe { CStr::from_ptr(ptr) }.to_bytes().to_vec();
    unsafe { (api.py_mem_free)(ptr.cast()) };
    if bytes.is_empty() {
        CStdinLine::Eof
    } else {
        CStdinLine::Line(String::from_utf8_lossy(&bytes).to_string())
    }
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

struct SessionState {
    inner: Mutex<SessionStateInner>,
    cvar: Condvar,
}

struct SessionStateInner {
    active_request: Option<ActiveRequest>,
    current_prompt: Option<String>,
    waiting_for_input: bool,
    shutdown: bool,
    session_end_emitted: bool,
    plot_reset_pending: bool,
}

struct ActiveRequest {
    reply: mpsc::Sender<RequestCompleted>,
    line_count: usize,
    consumed_lines: usize,
    skip_next_hook: bool,
}

impl SessionState {
    fn new() -> Self {
        Self {
            inner: Mutex::new(SessionStateInner {
                active_request: None,
                current_prompt: None,
                waiting_for_input: false,
                shutdown: false,
                session_end_emitted: false,
                plot_reset_pending: false,
            }),
            cvar: Condvar::new(),
        }
    }
}

fn session_state() -> &'static Arc<SessionState> {
    SESSION_STATE
        .get()
        .expect("Python session state was not initialized")
}

fn complete_active_request(
    state: &Arc<SessionState>,
    active: Option<ActiveRequest>,
    emit_session_end: bool,
) {
    if let Some(active) = active {
        let _ = active.reply.send(RequestCompleted);
        state.cvar.notify_all();
    }
    if emit_session_end {
        ipc::emit_session_end();
    }
}

fn finish_session_end() {
    let state = session_state();
    let mut guard = state.inner.lock().unwrap();
    let should_emit = !guard.session_end_emitted;
    guard.session_end_emitted = true;
    guard.shutdown = true;
    let active = guard.active_request.take();
    drop(guard);
    complete_active_request(state, active, should_emit);
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
            name: "emit_plot_image",
            function: py_emit_plot_image,
        },
        ModuleMethod {
            name: "has_request_active",
            function: py_has_request_active,
        },
        ModuleMethod {
            name: "take_plot_reset_pending",
            function: py_take_plot_reset_pending,
        },
        ModuleMethod {
            name: "executable",
            function: py_executable,
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
    api.long_result(message.len() as c_long)
}

unsafe extern "C" fn py_emit_plot_image(
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
    ipc::emit_plot_image(&mime_type, &data, is_update == 1, Some(&source));
    api.none()
}

unsafe extern "C" fn py_has_request_active(
    _self: *mut PyObject,
    _args: *mut PyObject,
) -> *mut PyObject {
    let Some(state) = SESSION_STATE.get() else {
        return PythonApi::global().bool_result(false);
    };
    PythonApi::global().bool_result(state.inner.lock().unwrap().active_request.is_some())
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

unsafe extern "C" fn py_executable(_self: *mut PyObject, _args: *mut PyObject) -> *mut PyObject {
    let api = PythonApi::global();
    match std::env::var("MCP_REPL_PYTHON_EXECUTABLE") {
        Ok(value) if !value.is_empty() => match api.unicode(&value) {
            Ok(value) => value.into_raw(),
            Err(_) => ptr::null_mut(),
        },
        _ => api.none(),
    }
}

fn set_callback_error(message: &str) {
    let exception = RUNTIME_ERROR.load(Ordering::SeqCst);
    if exception.is_null() {
        return;
    }
    PythonApi::global().set_runtime_error(exception, message);
}

static SESSION_STATE: OnceLock<Arc<SessionState>> = OnceLock::new();
static SESSION: OnceLock<PythonSession> = OnceLock::new();
static RUNTIME_ERROR: AtomicPtr<PyObject> = AtomicPtr::new(ptr::null_mut());
static PYTHON_STDIN_FILE: AtomicPtr<libc::FILE> = AtomicPtr::new(ptr::null_mut());
static PYTHON_STDOUT_FILE: AtomicPtr<libc::FILE> = AtomicPtr::new(ptr::null_mut());
