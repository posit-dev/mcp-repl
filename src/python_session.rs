use std::ffi::{CStr, CString, c_char, c_int, c_long};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, mpsc};

use serde::Deserialize;

use crate::ipc;
use crate::python_ffi::{GilGuard, ModuleMethod, PyObject, PyPtr, PyThreadState, PythonApi};
use crate::worker_protocol::TextStream;
#[cfg(windows)]
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::ReadFile;
#[cfg(windows)]
use windows_sys::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE};
#[cfg(windows)]
use windows_sys::Win32::System::Pipes::PeekNamedPipe;
#[cfg(windows)]
use windows_sys::Win32::System::Threading::SetEvent;

pub const PYTHON_EXECUTABLE_ENV: &str = "MCP_REPL_PYTHON_EXECUTABLE";
const MCP_REPL_PYTHON: &str = include_str!("../python/embedded.py");
const PYTHON_EOF: c_int = 11;
const PYTHON_PROGRAM: &str = "python3";
const PYTHON_PROGRAM_FALLBACK: &str = "python";
const PYTHON_CONFIG_SNIPPET: &str = r#"
import json
import sys
import sysconfig

def var(name):
    value = sysconfig.get_config_var(name)
    return "" if value is None else str(value)

print(json.dumps({
    "executable": sys.executable,
    "base_executable": getattr(sys, "_base_executable", sys.executable),
    "prefix": sys.prefix,
    "base_prefix": sys.base_prefix,
    "exec_prefix": sys.exec_prefix,
    "base_exec_prefix": sys.base_exec_prefix,
    "version": [sys.version_info[0], sys.version_info[1]],
    "ldlibrary": var("LDLIBRARY"),
    "instsoname": var("INSTSONAME"),
    "libdir": var("LIBDIR"),
    "libpl": var("LIBPL"),
    "bindir": var("BINDIR"),
    "pythonframeworkprefix": var("PYTHONFRAMEWORKPREFIX"),
    "pythonframeworkinstalldir": var("PYTHONFRAMEWORKINSTALLDIR"),
}))
"#;

#[derive(Debug)]
struct PythonRuntimeConfig {
    executable: PathBuf,
    libpython: PathBuf,
}

#[derive(Debug, Deserialize)]
struct PythonRuntimeProbe {
    executable: String,
    base_executable: String,
    prefix: String,
    base_prefix: String,
    exec_prefix: String,
    base_exec_prefix: String,
    version: [u64; 2],
    ldlibrary: String,
    instsoname: String,
    libdir: String,
    libpl: String,
    #[cfg(windows)]
    bindir: String,
    pythonframeworkprefix: String,
    pythonframeworkinstalldir: String,
}

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
        byte_len: usize,
        line_count: usize,
        fallback_prompt: Option<String>,
    ) -> Result<mpsc::Receiver<RequestCompleted>, String> {
        self.wait_until_ready()?;
        let (reply_tx, reply_rx) = mpsc::channel();
        begin_tracked_request(byte_len, line_count, fallback_prompt, reply_tx)?;
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
    discard_pending_stdin();
    finish_active_request_at_next_read();
    let prompt_interrupt = mark_interrupt_requested();
    if !prompt_interrupt && let Some(api) = PythonApi::try_global() {
        unsafe {
            (api.py_err_set_interrupt)();
            wake_python_sigint_event(api);
        }
    }
}

pub(crate) fn interrupt_prompt() {
    discard_pending_stdin();
    finish_active_request_at_next_read();
    mark_interrupt_requested();
}

#[cfg(windows)]
unsafe fn wake_python_sigint_event(api: &PythonApi) {
    let Some(sigint_event) = api.pyos_sigint_event else {
        return;
    };
    let event = unsafe { sigint_event() };
    if !event.is_null() {
        unsafe {
            SetEvent(event);
        }
    }
}

#[cfg(not(windows))]
unsafe fn wake_python_sigint_event(_api: &PythonApi) {}

fn mark_interrupt_requested() -> bool {
    let Some(state) = SESSION_STATE.get() else {
        return false;
    };
    let mut guard = state.inner.lock().unwrap();
    let prompt_interrupt = guard.current_prompt.is_some()
        || (guard.active_request.is_none() && guard.waiting_for_input);
    guard.interrupt_requested = true;
    state.cvar.notify_all();
    prompt_interrupt
}

fn take_interrupt_requested() -> bool {
    let Some(state) = SESSION_STATE.get() else {
        return false;
    };
    let mut guard = state.inner.lock().unwrap();
    let requested = guard.interrupt_requested;
    guard.interrupt_requested = false;
    requested
}

pub(crate) fn mark_stdin_write_complete() {
    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    let mut completed = None;
    let mut prompt = None;
    {
        let mut guard = state.inner.lock().unwrap();
        let current_prompt_from_state = guard.current_prompt.clone();
        let waiting_for_input = guard.waiting_for_input;
        if let Some(active) = guard.active_request.as_mut() {
            active.stdin_write_complete = true;
            let should_complete = if active.repl_turn_finished {
                request_repl_turn_should_complete(active)
            } else {
                request_prompt_wait_should_complete(active, current_prompt_from_state.as_deref())
            };
            if waiting_for_input && should_complete {
                prompt = Some(
                    current_prompt_from_state
                        .or_else(|| active.fallback_prompt.clone())
                        .unwrap_or_else(|| ">>> ".to_string()),
                );
                completed = guard.active_request.take();
            }
        }
    }

    if let Some(active) = completed {
        emit_plots();
        ipc::emit_readline_start(prompt.as_deref().unwrap_or(">>> "), true);
        complete_active_request(state, Some(active), false);
    }
}

fn finish_active_request_at_next_read() {
    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    let mut guard = state.inner.lock().unwrap();
    guard.waiting_for_input = false;
    if let Some(active) = guard.active_request.as_mut() {
        active.line_count = active.consumed_lines.saturating_add(1);
        active.fallback_prompt = None;
        active.skip_next_hook = false;
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

#[cfg(windows)]
fn discard_pending_stdin() {
    let stdin = PYTHON_STDIN_FILE.load(Ordering::SeqCst);
    if !stdin.is_null() {
        unsafe {
            libc::fflush(stdin);
        }
    }
    drain_stdin_pipe();
}

#[cfg(not(any(target_family = "unix", windows)))]
fn discard_pending_stdin() {}

#[cfg(windows)]
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

    let runtime_config = match resolve_python_runtime_config() {
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
    ipc::emit_backend_info(plot_capable());

    let result = run_repl(&runtime);
    let finalize_result = finalize_python(api, thread_state);
    finish_session_end();
    crate::diagnostics::startup_log("python-session: repl exited");
    result?;
    finalize_result?;
    Ok(())
}

fn open_python_runtime() -> Result<PythonRuntime, String> {
    let stdin = open_stdio_file(0, c"r")?;
    set_stdio_unbuffered(stdin, 0)?;
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

fn set_stdio_unbuffered(file: *mut libc::FILE, fd: libc::c_int) -> Result<(), String> {
    let rc = unsafe { libc::setvbuf(file, ptr::null_mut(), libc::_IONBF, 0) };
    if rc == 0 {
        Ok(())
    } else {
        Err(format!("failed to configure worker fd {fd} as unbuffered"))
    }
}

fn find_dot_venv_python(start: &Path) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    // Search HOME itself, then stop. Do not ascend to HOME's parent.
    let stop_at_home = home
        .as_ref()
        .filter(|home| start.starts_with(home.as_path()))
        .cloned();
    let mut dir = start.to_path_buf();
    loop {
        for candidate in [
            dir.join(".venv").join("bin").join("python"),
            dir.join(".venv").join("bin").join("python3"),
        ] {
            if candidate.is_file() {
                return Some(candidate);
            }
        }

        if let Some(stop) = stop_at_home.as_ref()
            && &dir == stop
        {
            break;
        }

        let Some(parent) = dir.parent() else {
            break;
        };
        if parent == dir {
            break;
        }
        dir = parent.to_path_buf();
    }
    None
}

fn find_program_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if !candidate.is_file() {
            continue;
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(&candidate)
                && meta.permissions().mode() & 0o111 != 0
            {
                return Some(candidate);
            }
        }

        #[cfg(not(unix))]
        {
            return Some(candidate);
        }
    }
    None
}

fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

fn python_program_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(venv_python) = std::env::current_dir()
        .ok()
        .and_then(|cwd| find_dot_venv_python(&cwd))
    {
        push_unique_path(&mut candidates, venv_python);
    }
    push_unique_path(
        &mut candidates,
        find_program_on_path(PYTHON_PROGRAM).unwrap_or_else(|| PathBuf::from(PYTHON_PROGRAM)),
    );
    push_unique_path(
        &mut candidates,
        find_program_on_path(PYTHON_PROGRAM_FALLBACK)
            .unwrap_or_else(|| PathBuf::from(PYTHON_PROGRAM_FALLBACK)),
    );
    candidates
}

pub(crate) fn resolve_python_program() -> PathBuf {
    select_python_program(python_program_candidates(), python_program_starts)
}

fn query_python_runtime_config(executable: &Path) -> Result<PythonRuntimeConfig, String> {
    let output = Command::new(executable)
        .arg("-I")
        .arg("-c")
        .arg(PYTHON_CONFIG_SNIPPET)
        .stdin(Stdio::null())
        .output()
        .map_err(|err| {
            format!(
                "failed to query Python runtime config from {}: {err}",
                executable.display()
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "failed to query Python runtime config from {}: {}",
            executable.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let probe: PythonRuntimeProbe = serde_json::from_slice(&output.stdout).map_err(|err| {
        format!(
            "failed to parse Python runtime config from {}: {err}",
            executable.display()
        )
    })?;
    let libpython = resolve_libpython_path(&probe).ok_or_else(|| {
        format!(
            "failed to locate a shared libpython for {}",
            executable.display()
        )
    })?;
    let executable = first_non_empty([probe.executable.as_str(), probe.base_executable.as_str()])
        .map(PathBuf::from)
        .unwrap_or_else(|| executable.to_path_buf());
    Ok(PythonRuntimeConfig {
        executable,
        libpython,
    })
}

fn python_program_starts(program: &Path) -> bool {
    Command::new(program)
        .args(["-c", "import sys; sys.exit(0)"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn select_python_program(
    mut candidates: Vec<PathBuf>,
    mut starts: impl FnMut(&Path) -> bool,
) -> PathBuf {
    if candidates.is_empty() {
        candidates.push(PathBuf::from(PYTHON_PROGRAM));
    }
    candidates
        .iter()
        .find(|candidate| starts(candidate))
        .cloned()
        .unwrap_or_else(|| candidates.remove(0))
}

fn select_python_runtime_config(
    executable_override: Option<PathBuf>,
    mut candidates: Vec<PathBuf>,
    mut query: impl FnMut(&Path) -> Result<PythonRuntimeConfig, String>,
) -> Result<PythonRuntimeConfig, String> {
    if let Some(executable) = executable_override {
        return query(&executable);
    }

    if candidates.is_empty() {
        candidates.push(PathBuf::from(PYTHON_PROGRAM));
    }

    let mut errors = Vec::new();
    for candidate in candidates {
        match query(&candidate) {
            Ok(config) => return Ok(config),
            Err(err) => errors.push(format!("{}: {err}", candidate.display())),
        }
    }

    Err(format!(
        "failed to query Python runtime config from candidate interpreters: {}",
        errors.join("; ")
    ))
}

fn resolve_python_runtime_config() -> Result<PythonRuntimeConfig, String> {
    select_python_runtime_config(
        std::env::var_os(PYTHON_EXECUTABLE_ENV).map(PathBuf::from),
        python_program_candidates(),
        query_python_runtime_config,
    )
}

fn resolve_libpython_path(probe: &PythonRuntimeProbe) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    push_python_library_candidates(&mut candidates, probe, &probe.ldlibrary);
    if probe.instsoname != probe.ldlibrary {
        push_python_library_candidates(&mut candidates, probe, &probe.instsoname);
    }
    push_windows_python_library_candidates(&mut candidates, probe);

    let version = format!("{}.{}", probe.version[0], probe.version[1]);
    for root in [
        probe.base_exec_prefix.as_str(),
        probe.exec_prefix.as_str(),
        probe.base_prefix.as_str(),
        probe.prefix.as_str(),
    ] {
        if root.is_empty() {
            continue;
        }
        candidates.push(
            Path::new(root)
                .join("lib")
                .join(format!("libpython{version}.so")),
        );
        candidates.push(
            Path::new(root)
                .join("lib")
                .join(format!("libpython{version}.dylib")),
        );
        candidates.push(Path::new(root).join("Python"));
    }

    candidates.into_iter().find(|candidate| candidate.is_file())
}

#[cfg(windows)]
fn push_windows_python_library_candidates(
    candidates: &mut Vec<PathBuf>,
    probe: &PythonRuntimeProbe,
) {
    let compact_version = format!("{}{}", probe.version[0], probe.version[1]);
    let library_names = [
        format!("python{compact_version}.dll"),
        "python3.dll".to_string(),
    ];

    for root in [
        Path::new(probe.executable.as_str()).parent(),
        Path::new(probe.base_executable.as_str()).parent(),
        non_empty(&probe.bindir).map(Path::new),
        non_empty(&probe.base_exec_prefix).map(Path::new),
        non_empty(&probe.exec_prefix).map(Path::new),
        non_empty(&probe.base_prefix).map(Path::new),
        non_empty(&probe.prefix).map(Path::new),
    ]
    .into_iter()
    .flatten()
    {
        for library in &library_names {
            candidates.push(root.join(library));
        }
    }
}

#[cfg(not(windows))]
fn push_windows_python_library_candidates(
    _candidates: &mut Vec<PathBuf>,
    _probe: &PythonRuntimeProbe,
) {
}

fn push_python_library_candidates(
    candidates: &mut Vec<PathBuf>,
    probe: &PythonRuntimeProbe,
    library: &str,
) {
    let Some(library) = non_empty(library) else {
        return;
    };
    let path = Path::new(library);
    if path.is_absolute() {
        candidates.push(path.to_path_buf());
    }
    for executable in [probe.executable.as_str(), probe.base_executable.as_str()] {
        let Some(executable) = non_empty(executable) else {
            continue;
        };
        let Some(parent) = Path::new(executable).parent() else {
            continue;
        };
        candidates.push(parent.join(library));
    }
    for root in [
        probe.libdir.as_str(),
        probe.libpl.as_str(),
        probe.pythonframeworkprefix.as_str(),
        probe.pythonframeworkinstalldir.as_str(),
    ] {
        if let Some(root) = non_empty(root) {
            candidates.push(Path::new(root).join(library));
        }
    }
}

fn first_non_empty<'a>(values: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    values.into_iter().find_map(non_empty)
}

fn non_empty(value: &str) -> Option<&str> {
    let value = value.trim();
    if value.is_empty() { None } else { Some(value) }
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
        if take_exit_requested() {
            flush_original_stdio();
            return Ok(());
        }
        emit_plots();
        finish_repl_turn_request();
        if status == PYTHON_EOF {
            flush_original_stdio();
            return Ok(());
        }
    }
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

fn begin_tracked_request(
    byte_len: usize,
    line_count: usize,
    fallback_prompt: Option<String>,
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
        byte_len,
        line_count,
        fallback_prompt,
        consumed_lines: 0,
        skip_next_hook,
        stdin_write_complete: false,
        repl_turn_finished: false,
    });
    guard.plot_reset_pending = true;
    state.cvar.notify_all();
    Ok(())
}

fn input_hook_prompt(guard: &SessionStateInner, fallback_prompt: Option<&str>) -> String {
    guard
        .current_prompt
        .clone()
        .or_else(|| fallback_prompt.map(str::to_string))
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
        let current_prompt_from_state = guard.current_prompt.clone();
        let idle_prompt = input_hook_prompt(&guard, None);
        if let Some(active) = guard.active_request.as_mut() {
            let current_prompt = current_prompt_from_state
                .clone()
                .or_else(|| active.fallback_prompt.clone())
                .unwrap_or_else(|| ">>> ".to_string());
            if active.skip_next_hook {
                active.skip_next_hook = false;
            } else {
                note_input_hook_consumed_line(active);
            }
            let should_complete = if active.repl_turn_finished {
                request_repl_turn_should_complete(active)
            } else {
                request_prompt_wait_should_complete(active, current_prompt_from_state.as_deref())
            };
            guard.waiting_for_input = true;
            if should_complete {
                prompt = Some(current_prompt);
                completed = guard.active_request.take();
            }
        } else if !guard.waiting_for_input {
            guard.waiting_for_input = true;
            prompt = Some(idle_prompt);
            emit_idle = true;
        }
    }

    if let Some(active) = completed {
        emit_plots();
        flush_original_stdio();
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

fn request_prompt_wait_should_complete(
    active: &ActiveRequest,
    current_prompt: Option<&str>,
) -> bool {
    #[cfg(target_family = "unix")]
    {
        prompt_wait_can_complete(active, current_prompt)
            && (single_line_client_input_prompt(active, current_prompt)
                || request_input_drained(active))
    }
    #[cfg(windows)]
    {
        prompt_can_complete_before_repl_turn(active, current_prompt)
            && active.byte_len > 0
            && stdin_pending_byte_count() == Some(0)
    }
    #[cfg(not(any(target_family = "unix", windows)))]
    {
        active.consumed_lines >= active.line_count
    }
}

#[cfg(target_family = "unix")]
fn prompt_wait_can_complete(active: &ActiveRequest, current_prompt: Option<&str>) -> bool {
    active.consumed_lines >= active.line_count
        || current_prompt.is_some_and(|prompt| prompt != ">>> ")
        || active
            .fallback_prompt
            .as_deref()
            .is_some_and(|prompt| prompt != ">>> ")
}

#[cfg(target_family = "unix")]
fn single_line_client_input_prompt(active: &ActiveRequest, current_prompt: Option<&str>) -> bool {
    active.line_count == 1 && current_prompt.is_some_and(|prompt| prompt != ">>> ")
}

fn request_repl_turn_should_complete(active: &ActiveRequest) -> bool {
    #[cfg(target_family = "unix")]
    {
        request_input_drained(active)
    }
    #[cfg(windows)]
    {
        active.line_count == 1 || (active.byte_len > 0 && stdin_pending_byte_count() == Some(0))
    }
    #[cfg(not(any(target_family = "unix", windows)))]
    {
        active.consumed_lines >= active.line_count
    }
}

#[cfg(windows)]
fn prompt_can_complete_before_repl_turn(
    active: &ActiveRequest,
    current_prompt: Option<&str>,
) -> bool {
    current_prompt.is_some_and(|prompt| prompt != ">>> ")
        || active
            .fallback_prompt
            .as_deref()
            .is_some_and(|prompt| prompt != ">>> ")
}

#[cfg(target_family = "unix")]
fn request_input_drained(active: &ActiveRequest) -> bool {
    if !active.stdin_write_complete || active.byte_len == 0 {
        return false;
    }
    stdin_pending_byte_count() == Some(0)
}

fn finish_repl_turn_request() {
    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    let mut completed = None;
    let mut prompt = None;
    {
        let mut guard = state.inner.lock().unwrap();
        let current_prompt_from_state = guard.current_prompt.clone();
        guard.interrupt_requested = false;
        if guard.active_request.is_some() {
            guard.waiting_for_input = true;
        }
        if let Some(active) = guard.active_request.as_mut() {
            active.repl_turn_finished = true;
            if active.line_count == 1 {
                active.consumed_lines = active.consumed_lines.max(1);
            }
            if request_repl_turn_should_complete(active) {
                prompt = Some(
                    current_prompt_from_state
                        .or_else(|| active.fallback_prompt.clone())
                        .unwrap_or_else(|| ">>> ".to_string()),
                );
                completed = guard.active_request.take();
            }
        }
    }

    if let Some(active) = completed {
        flush_original_stdio();
        ipc::emit_readline_start(prompt.as_deref().unwrap_or(">>> "), true);
        complete_active_request(state, Some(active), false);
    }
}

#[cfg(target_family = "unix")]
fn stdin_pending_byte_count() -> Option<usize> {
    let mut count: libc::c_int = 0;
    let rc = unsafe { libc::ioctl(libc::STDIN_FILENO, libc::FIONREAD, &mut count) };
    if rc == 0 && count >= 0 {
        Some(count as usize)
    } else {
        None
    }
}

#[cfg(windows)]
fn stdin_pending_byte_count() -> Option<usize> {
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

#[cfg(not(any(target_family = "unix", windows)))]
fn stdin_pending_byte_count() -> Option<usize> {
    None
}

unsafe extern "C" fn mcp_repl_readline(
    stdin: *mut libc::FILE,
    stdout: *mut libc::FILE,
    prompt: *const c_char,
) -> *mut c_char {
    let prompt_text = (!prompt.is_null())
        .then(|| {
            unsafe { CStr::from_ptr(prompt) }
                .to_string_lossy()
                .into_owned()
        })
        .filter(|prompt| !prompt.is_empty());
    if let Some(prompt_text) = &prompt_text {
        set_current_readline_prompt(prompt_text);
    }
    handle_input_hook();
    if prompt_text.is_some() {
        unsafe {
            libc::fputs(prompt, stdout);
            libc::fflush(stdout);
        }
    }

    let bytes = read_stdio_line_bytes(stdin);
    note_stdin_line_read(&bytes);
    if prompt_text.is_some() {
        clear_current_readline_prompt();
    }

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

fn read_stdio_line_bytes(stdin: *mut libc::FILE) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        let ch = unsafe { libc::fgetc(stdin) };
        if ch == libc::EOF {
            break;
        }
        bytes.push(ch as u8);
        if ch == b'\n' as i32 {
            break;
        }
    }
    bytes
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

    set_current_readline_prompt(prompt_for_sideband.to_str().unwrap_or(""));
    handle_input_hook();
    emit_output_text(TextStream::Stdout, prompt.as_bytes());
    let bytes = read_stdio_line_bytes(stdin);
    note_stdin_line_read(&bytes);
    clear_current_readline_prompt();
    if take_interrupt_requested() {
        set_callback_error("Python input interrupted");
        return CStdinLine::Error;
    }
    if bytes.is_empty() {
        CStdinLine::Eof
    } else {
        CStdinLine::Line(String::from_utf8_lossy(&bytes).to_string())
    }
}

#[cfg(target_family = "unix")]
fn note_stdin_line_read(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    let mut guard = state.inner.lock().unwrap();
    if let Some(active) = guard.active_request.as_mut() {
        active.consumed_lines = active.consumed_lines.saturating_add(1);
    }
}

#[cfg(not(target_family = "unix"))]
fn note_stdin_line_read(_bytes: &[u8]) {}

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

struct SessionState {
    inner: Mutex<SessionStateInner>,
    cvar: Condvar,
}

struct SessionStateInner {
    active_request: Option<ActiveRequest>,
    current_prompt: Option<String>,
    waiting_for_input: bool,
    exit_requested: bool,
    shutdown: bool,
    session_end_emitted: bool,
    plot_reset_pending: bool,
    interrupt_requested: bool,
}

struct ActiveRequest {
    reply: mpsc::Sender<RequestCompleted>,
    byte_len: usize,
    line_count: usize,
    fallback_prompt: Option<String>,
    consumed_lines: usize,
    skip_next_hook: bool,
    stdin_write_complete: bool,
    repl_turn_finished: bool,
}

impl SessionState {
    fn new() -> Self {
        Self {
            inner: Mutex::new(SessionStateInner {
                active_request: None,
                current_prompt: None,
                waiting_for_input: false,
                exit_requested: false,
                shutdown: false,
                session_end_emitted: false,
                plot_reset_pending: false,
                interrupt_requested: false,
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

fn complete_active_request_with_options(
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
            name: "request_exit",
            function: py_request_exit,
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

unsafe extern "C" fn py_request_exit(_self: *mut PyObject, args: *mut PyObject) -> *mut PyObject {
    let api = PythonApi::global();
    if api.tuple_size(args) != 0 {
        set_callback_error("request_exit expects no arguments");
        return ptr::null_mut();
    }
    request_exit();
    api.none()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime_config_for(path: &str) -> PythonRuntimeConfig {
        PythonRuntimeConfig {
            executable: PathBuf::from(path),
            libpython: PathBuf::from("python.dll"),
        }
    }

    #[cfg(target_family = "unix")]
    fn active_request_for_prompt_wait(
        line_count: usize,
        consumed_lines: usize,
        fallback_prompt: Option<&str>,
    ) -> ActiveRequest {
        let (reply, _rx) = std::sync::mpsc::channel();
        ActiveRequest {
            reply,
            byte_len: 1,
            line_count,
            fallback_prompt: fallback_prompt.map(str::to_string),
            consumed_lines,
            skip_next_hook: false,
            stdin_write_complete: true,
            repl_turn_finished: false,
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

        assert!(prompt_wait_can_complete(&active, Some("input> ")));
    }

    #[test]
    fn python_program_selection_falls_back_after_broken_python3_candidate() {
        let selected = select_python_program(
            vec![PathBuf::from("python3"), PathBuf::from("python")],
            |candidate| candidate == Path::new("python"),
        );

        assert_eq!(selected, PathBuf::from("python"));
    }

    #[test]
    fn python_runtime_config_falls_back_after_broken_python3_candidate() {
        let mut attempts = Vec::new();

        let config = select_python_runtime_config(
            None,
            vec![PathBuf::from("python3"), PathBuf::from("python")],
            |candidate| {
                attempts.push(candidate.to_path_buf());
                if candidate == Path::new("python3") {
                    Err("store alias is not a usable interpreter".to_string())
                } else {
                    Ok(runtime_config_for("python"))
                }
            },
        )
        .expect("python fallback should be used after python3 fails");

        assert_eq!(
            attempts,
            vec![PathBuf::from("python3"), PathBuf::from("python")]
        );
        assert_eq!(config.executable, PathBuf::from("python"));
    }

    #[test]
    fn python_runtime_config_env_override_does_not_fallback() {
        let mut attempts = Vec::new();

        let err = select_python_runtime_config(
            Some(PathBuf::from("custom-python")),
            vec![PathBuf::from("python3"), PathBuf::from("python")],
            |candidate| {
                attempts.push(candidate.to_path_buf());
                Err(format!("{} is not usable", candidate.display()))
            },
        )
        .expect_err("explicit Python override should not fall back");

        assert_eq!(attempts, vec![PathBuf::from("custom-python")]);
        assert!(err.contains("custom-python is not usable"));
    }

    #[cfg(windows)]
    #[test]
    fn resolve_libpython_path_finds_windows_dll_next_to_executable() {
        fn runtime_probe_for(executable: &str) -> PythonRuntimeProbe {
            PythonRuntimeProbe {
                executable: executable.to_string(),
                base_executable: executable.to_string(),
                prefix: String::new(),
                base_prefix: String::new(),
                exec_prefix: String::new(),
                base_exec_prefix: String::new(),
                version: [3, 11],
                ldlibrary: String::new(),
                instsoname: String::new(),
                libdir: String::new(),
                libpl: String::new(),
                bindir: String::new(),
                pythonframeworkprefix: String::new(),
                pythonframeworkinstalldir: String::new(),
            }
        }

        let temp = tempfile::tempdir().expect("tempdir");
        let python = temp.path().join("python.exe");
        let dll = temp.path().join("python311.dll");
        std::fs::write(&python, "").expect("python placeholder");
        std::fs::write(&dll, "").expect("dll placeholder");

        let probe = runtime_probe_for(&python.to_string_lossy());

        assert_eq!(resolve_libpython_path(&probe), Some(dll));
    }
}
