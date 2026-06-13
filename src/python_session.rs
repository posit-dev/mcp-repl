#[cfg(target_family = "unix")]
use std::collections::VecDeque;
use std::ffi::{CStr, CString, c_char, c_int, c_long};
#[cfg(target_family = "unix")]
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::ptr;
#[cfg(target_family = "unix")]
use std::sync::atomic::AtomicI32;
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

pub(crate) fn interrupt_request_generation(request_generation: u64) {
    interrupt_for_request_generation(Some(request_generation));
}

fn interrupt_for_request_generation(request_generation: Option<u64>) {
    if !interrupt_generation_is_current(request_generation) {
        return;
    }
    discard_pending_stdin();
    #[cfg(target_family = "unix")]
    flush_terminal_input();
    #[cfg(not(target_family = "unix"))]
    finish_active_request_at_next_read();
    mark_interrupt_requested();
    request_platform_interrupt();
}

#[cfg(target_family = "unix")]
fn flush_terminal_input() {
    let _ = unsafe { libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH) };
}

fn interrupt_generation_is_current(request_generation: Option<u64>) -> bool {
    let Some(request_generation) = request_generation else {
        return true;
    };
    let Some(state) = SESSION_STATE.get() else {
        return false;
    };
    let guard = state.inner.lock().unwrap();
    // Unix Python receives SIGINT out-of-band from the server and an IPC
    // interrupt message on a separate thread. SIGINT can bring Python back to a
    // prompt before the IPC thread handles that message; if the next MCP
    // request has already started, draining fd 0 here would discard the new
    // request's stdin. Generated Python interrupts are therefore allowed to
    // clean up only while their original request generation is still current.
    // The tradeoff is that a very late interrupt stops cleaning old tail bytes
    // once a later request is accepted; preserving the new request boundary is
    // the stricter REPL contract.
    guard.request_generation == request_generation
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

pub(crate) fn mark_stdin_write_complete() {
    #[cfg(target_family = "unix")]
    let protocol_input_exhausted = protocol_request_input_exhausted();

    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    let mut completed = None;
    let mut prompt = None;
    {
        let mut guard = state.inner.lock().unwrap();
        let current_prompt_from_state = guard.current_prompt.clone();
        let current_readline_state = guard.current_readline_state;
        let primary_prompt = guard.python_primary_prompt.clone();
        let continuation_prompt = guard.python_continuation_prompt.clone();
        let waiting_for_input = guard.waiting_for_input;
        #[cfg(target_family = "unix")]
        if protocol_input_exhausted && guard.active_request.is_none() && waiting_for_input {
            // Unix protocol-mode Python can reach the next prompt before the IPC
            // thread observes StdinWriteComplete. In that case the prompt hook
            // deliberately left the plot gate open because stdin was not yet
            // accounted; close it here once the explicit write-complete signal
            // proves the already-emitted prompt is the request boundary.
            guard.request_active = false;
        }
        if let Some(active) = guard.active_request.as_mut() {
            active.stdin_write_complete = true;
            let continuation_write_complete =
                windows_continuation_prompt_write_should_complete(active, current_readline_state);
            let should_complete = if active.repl_turn_finished {
                request_repl_turn_should_complete(active)
            } else {
                request_prompt_wait_should_complete(active, current_readline_state)
                    || continuation_write_complete
            };
            if (waiting_for_input || continuation_write_complete) && should_complete {
                let fallback_prompt = if active.repl_turn_finished {
                    None
                } else {
                    active
                        .fallback_prompt
                        .as_deref()
                        .or_else(|| active.started_after_continuation_prompt.then_some(""))
                };
                prompt = Some(repl_prompt_for(
                    current_prompt_from_state.clone(),
                    fallback_prompt,
                    current_readline_state,
                    &primary_prompt,
                    &continuation_prompt,
                ));
                completed = guard.active_request.take();
            }
        }
    }

    if let Some(active) = completed {
        emit_plots();
        #[cfg(not(target_family = "unix"))]
        mark_stdin_wait_prompt_completed_request();
        // Python object flushes run from handle_input_hook on the Python thread.
        let prompt = prompt.as_deref().unwrap_or(">>> ");
        remember_emitted_prompt(prompt);
        ipc::emit_readline_start(prompt);
        complete_active_request(state, Some(active), false);
    }
}

pub(crate) fn mark_request_started() {
    mark_request_started_with_generation(None, Vec::new());
}

pub(crate) fn mark_request_started_for_generation(request_generation: u64, stdin_bytes: Vec<u8>) {
    mark_request_started_with_generation(Some(request_generation), stdin_bytes);
}

fn mark_request_started_with_generation(request_generation: Option<u64>, stdin_bytes: Vec<u8>) {
    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    let should_record_background_plots = {
        let guard = state.inner.lock().unwrap();
        !guard.request_active || guard.request_completed_at_stdin_wait
    };
    if should_record_background_plots {
        // A stdin-wait prompt closes the MCP request while Python threads can
        // still mutate matplotlib state. Snapshot those inactive plots before
        // reopening the gate so a later stdin answer does not flush stale
        // background figures into its reply. A later explicit plot/show in the
        // new request still forces a fresh image.
        record_background_plots();
    }
    let mut guard = state.inner.lock().unwrap();
    if let Some(request_generation) = request_generation {
        guard.request_generation = request_generation;
    } else {
        guard.request_generation = guard.request_generation.wrapping_add(1);
    }
    guard.interrupt_requested = false;
    guard.request_completed_at_stdin_wait = false;
    guard.request_active = true;
    #[cfg(target_family = "unix")]
    {
        guard.protocol_stdin_bytes = stdin_bytes.into();
    }
    #[cfg(not(target_family = "unix"))]
    {
        let _ = stdin_bytes;
    }
    guard.plot_reset_pending = true;
}

#[cfg(windows)]
fn windows_continuation_prompt_write_should_complete(
    active: &ActiveRequest,
    _current_readline_state: Option<PythonReadlineState>,
) -> bool {
    active.started_after_continuation_prompt && active.line_count == 1
}

#[cfg(not(windows))]
fn windows_continuation_prompt_write_should_complete(
    _active: &ActiveRequest,
    _current_readline_state: Option<PythonReadlineState>,
) -> bool {
    false
}

#[cfg_attr(target_family = "unix", allow(dead_code))]
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
    let mut discarded = Vec::new();
    discarded.extend(drain_process_stdin_pipe());
    if discarded.is_empty() {
        return;
    }
    let discarded = take_protocol_stdin_bytes_for_runtime_read(&discarded);
    ipc::emit_readline_discard_bytes(&discarded);
}

#[cfg(target_family = "unix")]
fn drain_process_stdin_pipe() -> Vec<u8> {
    let Some(_nonblocking) = NonBlockingFd::new(libc::STDIN_FILENO) else {
        return Vec::new();
    };

    let mut discarded = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let read =
            unsafe { libc::read(libc::STDIN_FILENO, buffer.as_mut_ptr().cast(), buffer.len()) };
        if read > 0 {
            discarded.extend_from_slice(&buffer[..read as usize]);
            continue;
        }
        if read == 0 {
            break;
        }
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        if stdin_read_would_block(&err) {
            break;
        }
        break;
    }
    discarded
}

#[cfg(target_family = "unix")]
struct NonBlockingFd {
    fd: libc::c_int,
    previous_flags: Option<libc::c_int>,
}

#[cfg(target_family = "unix")]
impl NonBlockingFd {
    fn new(fd: libc::c_int) -> Option<Self> {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 {
            return None;
        }
        if flags & libc::O_NONBLOCK != 0 {
            return Some(Self {
                fd,
                previous_flags: None,
            });
        }

        let rc = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
        if rc < 0 {
            return None;
        }
        Some(Self {
            fd,
            previous_flags: Some(flags),
        })
    }
}

#[cfg(target_family = "unix")]
impl Drop for NonBlockingFd {
    fn drop(&mut self) {
        if let Some(flags) = self.previous_flags {
            let _ = unsafe { libc::fcntl(self.fd, libc::F_SETFL, flags) };
        }
    }
}

#[cfg(target_family = "unix")]
fn request_runtime_stdin_line(prompt: &str) -> bool {
    ipc::emit_readline_start(prompt);
    true
}

#[cfg(target_family = "unix")]
fn runtime_stdin_read_in_progress() -> bool {
    runtime_stdin_pending_byte_count().is_some_and(|count| count > 0)
}

#[cfg(target_family = "unix")]
fn runtime_stdin_pending_byte_count() -> Option<usize> {
    let fd = PYTHON_RUNTIME_STDIN_FD.load(Ordering::SeqCst);
    if fd < 0 {
        return None;
    }
    let mut count: libc::c_int = 0;
    let rc = unsafe { libc::ioctl(fd, libc::FIONREAD, &mut count) };
    if rc == 0 && count >= 0 {
        Some(count as usize)
    } else {
        None
    }
}

#[cfg(target_family = "unix")]
fn protocol_request_input_exhausted() -> bool {
    let Some(state) = SESSION_STATE.get() else {
        return stdin_pending_byte_count() == Some(0);
    };
    let guard = state.inner.lock().unwrap();
    guard.protocol_stdin_bytes.is_empty()
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
    ipc::emit_worker_ready("python", plot_capable());

    let result = run_repl(&runtime);
    let finalize_result = finalize_python(api, thread_state);
    finish_session_end();
    crate::diagnostics::startup_log("python-session: repl exited");
    result?;
    finalize_result?;
    Ok(())
}

fn open_python_runtime() -> Result<PythonRuntime, String> {
    #[cfg(target_family = "unix")]
    {
        open_python_runtime_with_pty_stdio()
    }

    #[cfg(not(target_family = "unix"))]
    {
        let stdin = open_stdio_file(0, c"r")?;
        set_stdio_unbuffered(stdin, 0)?;
        let stdout = open_stdio_file(1, c"w")?;
        PYTHON_STDIN_FILE.store(stdin, Ordering::SeqCst);
        PYTHON_STDOUT_FILE.store(stdout, Ordering::SeqCst);
        Ok(PythonRuntime { stdin })
    }
}

#[cfg(target_family = "unix")]
fn open_python_runtime_with_pty_stdio() -> Result<PythonRuntime, String> {
    ensure_python_pty_stdio()?;
    set_fd_close_on_exec(libc::STDIN_FILENO)?;

    let runtime_read_fd = duplicate_stdio_fd(libc::STDIN_FILENO)?;
    set_fd_close_on_exec(runtime_read_fd)?;
    let stdin = open_stdio_fd(runtime_read_fd, c"r")?;
    set_stdio_unbuffered(stdin, runtime_read_fd)?;
    let stdout = open_stdio_file(1, c"w")?;
    PYTHON_RUNTIME_STDIN_FD.store(runtime_read_fd, Ordering::SeqCst);
    PYTHON_STDIN_FILE.store(stdin, Ordering::SeqCst);
    PYTHON_STDOUT_FILE.store(stdout, Ordering::SeqCst);
    Ok(PythonRuntime { stdin })
}

#[cfg(target_family = "unix")]
fn ensure_python_pty_stdio() -> Result<(), String> {
    let missing = [
        (libc::STDIN_FILENO, "stdin"),
        (libc::STDOUT_FILENO, "stdout"),
        (libc::STDERR_FILENO, "stderr"),
    ]
    .into_iter()
    .filter_map(|(fd, label)| (!stdio_fd_is_tty(fd)).then_some(label))
    .collect::<Vec<_>>();
    if missing.is_empty() {
        return Ok(());
    }
    Err(format!(
        "Python PTY stdin transport requires TTY-backed C stdio; non-TTY fds: {}",
        missing.join(", ")
    ))
}

#[cfg(target_family = "unix")]
fn stdio_fd_is_tty(fd: libc::c_int) -> bool {
    unsafe { libc::isatty(fd) == 1 }
}

#[cfg(target_family = "unix")]
fn duplicate_stdio_fd(fd: libc::c_int) -> Result<RawFd, String> {
    let duplicated = unsafe { libc::dup(fd) };
    if duplicated < 0 {
        Err(format!(
            "failed to duplicate worker fd {fd}: {}",
            std::io::Error::last_os_error()
        ))
    } else {
        Ok(duplicated)
    }
}

#[cfg(target_family = "unix")]
fn set_fd_close_on_exec(fd: RawFd) -> Result<(), String> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(format!(
            "failed to read fd {fd} close-on-exec flags: {}",
            std::io::Error::last_os_error()
        ));
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(format!(
            "failed to set fd {fd} close-on-exec: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

#[cfg(target_family = "unix")]
fn stdin_read_would_block(err: &std::io::Error) -> bool {
    matches!(err.raw_os_error(), Some(code) if code == libc::EAGAIN || code == libc::EWOULDBLOCK)
}

fn open_stdio_file(fd: libc::c_int, mode: &CStr) -> Result<*mut libc::FILE, String> {
    open_stdio_fd(fd, mode)
}

fn open_stdio_fd(fd: libc::c_int, mode: &CStr) -> Result<*mut libc::FILE, String> {
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

fn find_dot_venv_pythons(start: &Path) -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    // Search HOME itself, then stop. Do not ascend to HOME's parent.
    let stop_at_home = home
        .as_ref()
        .filter(|home| start.starts_with(home.as_path()))
        .cloned();
    let mut dir = start.to_path_buf();
    loop {
        let mut candidates = Vec::new();
        for candidate in [
            dir.join(".venv").join("bin").join("python"),
            dir.join(".venv").join("bin").join("python3"),
        ] {
            if candidate.is_file() {
                candidates.push(candidate);
            }
        }
        if !candidates.is_empty() {
            return candidates;
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
    Vec::new()
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
    if let Ok(cwd) = std::env::current_dir() {
        for venv_python in find_dot_venv_pythons(&cwd) {
            push_unique_path(&mut candidates, venv_python);
        }
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

#[cfg(test)]
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

    candidates
        .into_iter()
        .find(|candidate| is_loadable_libpython_candidate(candidate))
}

fn is_loadable_libpython_candidate(candidate: &Path) -> bool {
    if !candidate.is_file() {
        return false;
    }
    !candidate
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("a") || extension.eq_ignore_ascii_case("lib")
        })
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
            begin_repl_turn();
            let status = unsafe {
                (api.py_run_interactive_one_flags)(
                    runtime.stdin,
                    c"<stdin>".as_ptr(),
                    ptr::null_mut(),
                )
            };
            capture_python_prompts(api)?;
            status
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
    let started_after_continuation_prompt = guard.last_prompt_was_continuation;
    guard.waiting_for_input = false;
    guard.request_generation = guard.request_generation.wrapping_add(1);
    guard.request_completed_at_stdin_wait = false;
    guard.active_request = Some(ActiveRequest {
        reply,
        byte_len,
        line_count,
        fallback_prompt,
        consumed_lines: 0,
        skip_next_hook,
        stdin_write_complete: false,
        repl_turn_finished: false,
        started_after_continuation_prompt,
    });
    #[cfg(not(target_family = "unix"))]
    {
        guard.request_active = true;
    }
    guard.plot_reset_pending = true;
    state.cvar.notify_all();
    Ok(())
}

#[cfg(target_family = "unix")]
fn mark_request_input_delivered() {
    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    let mut guard = state.inner.lock().unwrap();
    if !guard.request_active {
        guard.plot_reset_pending = true;
    }
    guard.request_active = true;
    guard.waiting_for_input = false;
}

fn set_python_prompts(primary: String, continuation: String) {
    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    let mut guard = state.inner.lock().unwrap();
    guard.python_primary_prompt = primary;
    guard.python_continuation_prompt = continuation;
}

fn repl_prompt_for(
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
fn input_hook_prompt(guard: &SessionStateInner, fallback_prompt: Option<&str>) -> String {
    repl_prompt_for(
        guard.current_prompt.clone(),
        fallback_prompt,
        guard.current_readline_state,
        &guard.python_primary_prompt,
        &guard.python_continuation_prompt,
    )
}

fn handle_input_hook() {
    #[cfg(target_family = "unix")]
    {
        handle_protocol_input_hook();
    }

    #[cfg(not(target_family = "unix"))]
    {
        let Some(state) = SESSION_STATE.get() else {
            return;
        };
        let mut completed = None;
        let mut prompt = None;
        let mut emit_idle = false;
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
                emit_idle = true;
            }
        }

        if flush_before_wait {
            flush_original_stdio();
        } else if let Some(active) = completed {
            emit_plots();
            #[cfg(not(target_family = "unix"))]
            mark_stdin_wait_prompt_completed_request();
            flush_original_stdio();
            let prompt = prompt.as_deref().unwrap_or(">>> ");
            remember_emitted_prompt(prompt);
            ipc::emit_readline_start(prompt);
            complete_active_request(state, Some(active), false);
        } else if emit_idle {
            let prompt = prompt.as_deref().unwrap_or(">>> ");
            remember_emitted_prompt(prompt);
            ipc::emit_readline_start(prompt);
        }
    }
}

#[cfg(target_family = "unix")]
fn handle_protocol_input_hook() {
    if runtime_stdin_read_in_progress() {
        return;
    }

    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    let input_exhausted = protocol_request_input_exhausted();
    let prompt = {
        let mut guard = state.inner.lock().unwrap();
        if guard.shutdown {
            return;
        }
        if input_exhausted {
            // Unix protocol-mode Python has no worker-local ActiveRequest. The
            // server completes the request when the next prompt arrives after
            // all request stdin is accounted, so clear the Python-side plot gate
            // at that same boundary. If more payload bytes remain, keep it
            // active so multi-line requests can still emit prompt-time plots.
            guard.request_active = false;
        }
        let prompt = if guard.repl_readline_count == 0 {
            guard.python_primary_prompt.clone()
        } else {
            guard.python_continuation_prompt.clone()
        };
        guard.repl_readline_count = guard.repl_readline_count.saturating_add(1);
        guard.current_prompt = Some(prompt.clone());
        guard.current_readline_state = Some(if guard.repl_readline_count == 1 {
            PythonReadlineState::Primary
        } else {
            PythonReadlineState::Continuation
        });
        guard.waiting_for_input = true;
        prompt
    };
    flush_original_stdio();
    request_runtime_stdin_line(&prompt);
}

unsafe extern "C" fn pyos_input_hook() -> c_int {
    handle_input_hook();
    0
}

#[cfg_attr(target_family = "unix", allow(dead_code))]
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
    current_readline_state: Option<PythonReadlineState>,
) -> bool {
    #[cfg(target_family = "unix")]
    {
        let input_drained = request_input_drained(active);
        (prompt_wait_can_complete(active, current_readline_state)
            && (single_line_client_input_prompt(active, current_readline_state) || input_drained))
            || (active.started_after_continuation_prompt && input_drained)
    }
    #[cfg(windows)]
    {
        prompt_can_complete_before_repl_turn(active, current_readline_state)
            && active.byte_len > 0
            && stdin_pending_byte_count() == Some(0)
    }
    #[cfg(not(any(target_family = "unix", windows)))]
    {
        active.consumed_lines >= active.line_count
    }
}

#[cfg(target_family = "unix")]
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

#[cfg(target_family = "unix")]
fn single_line_client_input_prompt(
    active: &ActiveRequest,
    current_readline_state: Option<PythonReadlineState>,
) -> bool {
    active.line_count == 1
        && matches!(
            current_readline_state,
            Some(PythonReadlineState::ClientInput)
        )
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
    current_readline_state: Option<PythonReadlineState>,
) -> bool {
    matches!(
        current_readline_state,
        Some(PythonReadlineState::ClientInput | PythonReadlineState::Continuation)
    ) || active.fallback_prompt.is_some()
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
        let current_readline_state = guard.current_readline_state;
        let primary_prompt = guard.python_primary_prompt.clone();
        let continuation_prompt = guard.python_continuation_prompt.clone();
        guard.interrupt_requested = false;
        if guard.active_request.is_some() {
            guard.waiting_for_input = true;
        } else {
            // Protocol-style Unix Python has no worker-local ActiveRequest; the
            // server owns completion, so RequestStart keeps plot state active
            // across all PyRun_InteractiveOne turns in one MCP request.
            #[cfg(not(target_family = "unix"))]
            {
                guard.request_active = false;
            }
        }
        if let Some(active) = guard.active_request.as_mut() {
            active.repl_turn_finished = true;
            if active.line_count == 1 {
                active.consumed_lines = active.consumed_lines.max(1);
            }
            if request_repl_turn_should_complete(active) {
                prompt = Some(repl_prompt_for(
                    current_prompt_from_state.clone(),
                    None,
                    current_readline_state,
                    &primary_prompt,
                    &continuation_prompt,
                ));
                completed = guard.active_request.take();
                guard.request_active = false;
            }
        }
    }

    if let Some(active) = completed {
        flush_original_stdio();
        let prompt = prompt.as_deref().unwrap_or(">>> ");
        remember_emitted_prompt(prompt);
        ipc::emit_readline_start(prompt);
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
    _stdout: *mut libc::FILE,
    prompt: *const c_char,
) -> *mut c_char {
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
    set_current_repl_readline_prompt(&prompt_text);
    #[cfg(target_family = "unix")]
    let prompt_has_buffered_answer = stdin_pending_byte_count().is_some_and(|count| count > 0);
    #[cfg(target_family = "unix")]
    let prompt_matches_repl = prompt_matches_python_repl_prompt(&prompt_text);
    #[cfg(target_family = "unix")]
    flush_original_stdio();
    #[cfg(target_family = "unix")]
    request_cpython_readline_stdin_line(&prompt_text);
    #[cfg(target_family = "unix")]
    if prompt_has_buffered_answer && !prompt_text.is_empty() && !prompt_matches_repl {
        emit_output_text(TextStream::Stdout, prompt_text.as_bytes());
    }
    #[cfg(not(target_family = "unix"))]
    handle_input_hook();

    let read = read_stdio_line_bytes(stdin);
    if read.interrupted {
        #[cfg(target_family = "unix")]
        flush_terminal_input();
    }
    note_cpython_readline_bytes_read(&read.bytes);
    clear_current_readline_prompt();
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

#[cfg(target_family = "unix")]
fn request_cpython_readline_stdin_line(prompt: &str) {
    ipc::emit_readline_start(prompt);
}

#[cfg(target_family = "unix")]
fn prompt_matches_python_repl_prompt(prompt: &str) -> bool {
    let Some(state) = SESSION_STATE.get() else {
        return false;
    };
    let guard = state.inner.lock().unwrap();
    prompt == guard.python_primary_prompt || prompt == guard.python_continuation_prompt
}

#[cfg(target_family = "unix")]
fn note_cpython_readline_bytes_read(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let protocol_bytes = take_protocol_stdin_bytes_for_runtime_read(bytes);
    emit_readline_input_bytes(&protocol_bytes);
    mark_request_input_delivered();
    note_active_stdin_line_read(&protocol_bytes);
}

#[cfg(not(target_family = "unix"))]
fn note_cpython_readline_bytes_read(bytes: &[u8]) {
    note_stdin_line_read(bytes);
}

struct StdioLineRead {
    bytes: Vec<u8>,
    interrupted: bool,
}

fn read_stdio_line_bytes(stdin: *mut libc::FILE) -> StdioLineRead {
    let mut bytes = Vec::new();
    loop {
        let ch = unsafe { libc::fgetc(stdin) };
        if ch == libc::EOF {
            let interrupted = unsafe { libc::ferror(stdin) != 0 };
            if interrupted {
                unsafe { clear_stdio_error(stdin) };
            }
            return StdioLineRead { bytes, interrupted };
        }
        bytes.push(ch as u8);
        if ch == b'\n' as i32 {
            return StdioLineRead {
                bytes,
                interrupted: false,
            };
        }
    }
}

#[cfg(not(windows))]
unsafe fn clear_stdio_error(stdin: *mut libc::FILE) {
    unsafe { libc::clearerr(stdin) };
}

#[cfg(windows)]
unsafe fn clear_stdio_error(stdin: *mut libc::FILE) {
    unsafe extern "C" {
        fn clearerr(stream: *mut libc::FILE);
    }

    unsafe { clearerr(stdin) };
}

fn read_stdio_line_bytes_allowing_python_threads(stdin: *mut libc::FILE) -> StdioLineRead {
    // _mcp_repl.readline is called from Python with the GIL held. Release it
    // while stdin blocks so the IPC completion path can flush prompt-time plots.
    let _allow_threads = PythonThreadsAllowed::new();
    read_stdio_line_bytes(stdin)
}

struct PythonThreadsAllowed {
    api: &'static PythonApi,
    thread_state: *mut PyThreadState,
}

impl PythonThreadsAllowed {
    fn new() -> Self {
        let api = PythonApi::global();
        let thread_state = unsafe { (api.py_eval_save_thread)() };
        assert!(
            !thread_state.is_null(),
            "PyEval_SaveThread returned a null thread state"
        );
        Self { api, thread_state }
    }
}

impl Drop for PythonThreadsAllowed {
    fn drop(&mut self) {
        unsafe { (self.api.py_eval_restore_thread)(self.thread_state) };
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PythonReadlineState {
    Primary,
    Continuation,
    ClientInput,
}

fn begin_repl_turn() {
    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    let mut guard = state.inner.lock().unwrap();
    guard.repl_readline_count = 0;
}

fn set_current_repl_readline_prompt(prompt: &str) {
    let Some(state) = SESSION_STATE.get() else {
        return;
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
}

fn remember_emitted_prompt(prompt: &str) {
    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    let mut guard = state.inner.lock().unwrap();
    guard.last_prompt_was_continuation = prompt == guard.python_continuation_prompt;
}

fn set_current_readline_prompt(prompt: &str, readline_state: PythonReadlineState) {
    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    let mut guard = state.inner.lock().unwrap();
    guard.current_prompt = Some(prompt.to_string());
    guard.current_readline_state = Some(readline_state);
}

fn clear_current_readline_prompt() {
    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    let mut guard = state.inner.lock().unwrap();
    guard.current_prompt = None;
    guard.current_readline_state = None;
}

enum CStdinLine {
    Line(String),
    Eof,
    Error,
}

fn read_c_stdin_line(prompt: &str) -> CStdinLine {
    #[cfg(target_family = "unix")]
    if ipc::worker_ipc_disabled_for_process() {
        return fork_child_stdin_eof(prompt);
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
    #[cfg(target_family = "unix")]
    let prompt_has_buffered_answer = stdin_pending_byte_count().is_some_and(|count| count > 0);
    #[cfg(target_family = "unix")]
    if !prompt_has_buffered_answer {
        emit_plots();
        mark_stdin_wait_prompt_completed_request();
    }
    #[cfg(target_family = "unix")]
    let prompt_delivered_immediately =
        request_runtime_stdin_line(prompt_for_sideband.to_str().unwrap_or(""));
    #[cfg(target_family = "unix")]
    if !prompt.is_empty() && (prompt_delivered_immediately || prompt_has_buffered_answer) {
        emit_output_text(TextStream::Stdout, prompt.as_bytes());
    }
    #[cfg(not(target_family = "unix"))]
    {
        flush_original_stdio();
        handle_input_hook();
        emit_output_text(TextStream::Stdout, prompt.as_bytes());
    }
    let read = read_stdio_line_bytes_allowing_python_threads(stdin);
    if read.interrupted {
        #[cfg(target_family = "unix")]
        flush_terminal_input();
    }
    note_stdin_line_read(&read.bytes);
    clear_current_readline_prompt();
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
fn fork_child_stdin_eof(prompt: &str) -> CStdinLine {
    // Fork children inherit fd 0/1/2, but mcp-repl sideband IPC is deliberately
    // disabled in the at-fork child handler. Reading fd 0 directly would be
    // closer to vanilla os.fork(), but the parent server could not observe
    // those consumed bytes through sideband and request completion would become
    // ambiguous. Treat mcp-repl-managed stdin as EOF in IPC-disabled children
    // instead. Raw stdout/stderr still fall back to fd writes, and fork+exec
    // children keep the inherited OS fds.
    if !prompt.is_empty() {
        emit_output_text(TextStream::Stdout, prompt.as_bytes());
    }
    CStdinLine::Eof
}

#[cfg(target_family = "unix")]
fn read_raw_stdin_bytes(size: usize) -> Vec<u8> {
    if ipc::worker_ipc_disabled_for_process() {
        return Vec::new();
    }

    let _allow_threads = PythonThreadsAllowed::new();
    let bytes = read_fd_bytes(libc::STDIN_FILENO, size);
    note_stdin_bytes_read(&bytes);
    bytes
}

#[cfg(not(target_family = "unix"))]
fn read_raw_stdin_bytes(_size: usize) -> Vec<u8> {
    Vec::new()
}

#[cfg(target_family = "unix")]
fn read_fd_bytes(fd: libc::c_int, size: usize) -> Vec<u8> {
    if size == 0 {
        return Vec::new();
    }
    let mut bytes = vec![0u8; size];
    loop {
        let read = unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
        if read > 0 {
            bytes.truncate(read as usize);
            return bytes;
        }
        if read == 0 {
            return Vec::new();
        }
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        return Vec::new();
    }
}

#[cfg(target_family = "unix")]
fn note_stdin_bytes_read(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let protocol_bytes = take_protocol_stdin_bytes_for_runtime_read(bytes);
    emit_readline_input_bytes(&protocol_bytes);
    mark_request_input_delivered();
    note_active_stdin_line_read(&protocol_bytes);
}

#[cfg(target_family = "unix")]
fn take_protocol_stdin_bytes_for_runtime_read(runtime_bytes: &[u8]) -> Vec<u8> {
    let Some(state) = SESSION_STATE.get() else {
        return runtime_bytes.to_vec();
    };
    let mut guard = state.inner.lock().unwrap();
    if guard.protocol_stdin_bytes.is_empty() {
        return runtime_bytes.to_vec();
    }

    let mut remaining = guard.protocol_stdin_bytes.clone();
    let mut protocol_bytes = Vec::with_capacity(runtime_bytes.len());
    for &runtime_byte in runtime_bytes {
        let Some(original_byte) = remaining.pop_front() else {
            return runtime_bytes.to_vec();
        };
        protocol_bytes.push(original_byte);
        let normalized_byte = if original_byte == b'\r' {
            b'\n'
        } else {
            original_byte
        };
        if normalized_byte != runtime_byte {
            return runtime_bytes.to_vec();
        }
    }
    guard.protocol_stdin_bytes = remaining;
    protocol_bytes
}

#[cfg(target_family = "unix")]
fn note_active_stdin_line_read(bytes: &[u8]) {
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

#[cfg(target_family = "unix")]
fn note_stdin_line_read(bytes: &[u8]) {
    note_stdin_bytes_read(bytes);
}

#[cfg(target_family = "unix")]
fn emit_readline_input_bytes(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    ipc::emit_readline_input_bytes(bytes);
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

fn request_active() -> bool {
    let Some(state) = SESSION_STATE.get() else {
        return false;
    };
    let guard = state.inner.lock().unwrap();
    guard.request_active && !guard.request_completed_at_stdin_wait
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
    request_generation: u64,
    request_active: bool,
    request_completed_at_stdin_wait: bool,
    current_prompt: Option<String>,
    current_readline_state: Option<PythonReadlineState>,
    python_primary_prompt: String,
    python_continuation_prompt: String,
    repl_readline_count: usize,
    last_prompt_was_continuation: bool,
    waiting_for_input: bool,
    exit_requested: bool,
    shutdown: bool,
    session_end_emitted: bool,
    plot_reset_pending: bool,
    interrupt_requested: bool,
    #[cfg(target_family = "unix")]
    protocol_stdin_bytes: VecDeque<u8>,
}

#[allow(dead_code)]
struct ActiveRequest {
    reply: mpsc::Sender<RequestCompleted>,
    byte_len: usize,
    line_count: usize,
    fallback_prompt: Option<String>,
    consumed_lines: usize,
    skip_next_hook: bool,
    stdin_write_complete: bool,
    repl_turn_finished: bool,
    started_after_continuation_prompt: bool,
}

impl SessionState {
    fn new() -> Self {
        Self {
            inner: Mutex::new(SessionStateInner {
                active_request: None,
                request_generation: 0,
                request_active: false,
                request_completed_at_stdin_wait: false,
                current_prompt: None,
                current_readline_state: None,
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
                #[cfg(target_family = "unix")]
                protocol_stdin_bytes: VecDeque::new(),
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

fn mark_stdin_wait_prompt_completed_request() {
    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    let mut guard = state.inner.lock().unwrap();
    // An input()/sys.stdin.readline() prompt with no buffered answer is the
    // response boundary for the current MCP request. The Python read can then
    // block while background Python threads keep running. Clear the plot gate at
    // this boundary to prevent those background updates from being attributed to
    // the request that already completed. On Unix this also happens before the
    // prompt sideband is emitted because the server owns that completion path.
    guard.request_active = false;
    guard.request_completed_at_stdin_wait = true;
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
            function: py_emit_plot_image,
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
    let bytes = read_raw_stdin_bytes(size);
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

static SESSION_STATE: OnceLock<Arc<SessionState>> = OnceLock::new();
static SESSION: OnceLock<PythonSession> = OnceLock::new();
static RUNTIME_ERROR: AtomicPtr<PyObject> = AtomicPtr::new(ptr::null_mut());
static PYTHON_STDIN_FILE: AtomicPtr<libc::FILE> = AtomicPtr::new(ptr::null_mut());
static PYTHON_STDOUT_FILE: AtomicPtr<libc::FILE> = AtomicPtr::new(ptr::null_mut());
#[cfg(target_family = "unix")]
static PYTHON_RUNTIME_STDIN_FD: AtomicI32 = AtomicI32::new(-1);

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime_config_for(path: &str) -> PythonRuntimeConfig {
        PythonRuntimeConfig {
            executable: PathBuf::from(path),
            libpython: PathBuf::from("python.dll"),
        }
    }

    fn runtime_probe_for_libpython(
        executable: &Path,
        version: [u64; 2],
        ldlibrary: &Path,
        libdir: &Path,
        prefix: &Path,
    ) -> PythonRuntimeProbe {
        PythonRuntimeProbe {
            executable: executable.to_string_lossy().into_owned(),
            base_executable: executable.to_string_lossy().into_owned(),
            prefix: prefix.to_string_lossy().into_owned(),
            base_prefix: prefix.to_string_lossy().into_owned(),
            exec_prefix: prefix.to_string_lossy().into_owned(),
            base_exec_prefix: prefix.to_string_lossy().into_owned(),
            version,
            ldlibrary: ldlibrary.to_string_lossy().into_owned(),
            instsoname: ldlibrary.to_string_lossy().into_owned(),
            libdir: libdir.to_string_lossy().into_owned(),
            libpl: libdir.to_string_lossy().into_owned(),
            #[cfg(windows)]
            bindir: String::new(),
            pythonframeworkprefix: String::new(),
            pythonframeworkinstalldir: String::new(),
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

    #[test]
    fn resolve_libpython_path_skips_static_archive_candidate() {
        let temp = tempfile::tempdir().expect("tempdir");
        let bin = temp.path().join("bin");
        let lib = temp.path().join("lib");
        std::fs::create_dir_all(&bin).expect("bin dir");
        std::fs::create_dir_all(&lib).expect("lib dir");
        let executable = bin.join("python3");
        let archive = lib.join("libpython3.11.a");
        let shared = lib.join("libpython3.11.so");
        std::fs::write(&executable, "").expect("python placeholder");
        std::fs::write(&archive, "!<arch>\n").expect("archive placeholder");
        std::fs::write(&shared, "").expect("shared placeholder");

        let probe = runtime_probe_for_libpython(&executable, [3, 11], &archive, &lib, temp.path());

        assert_eq!(resolve_libpython_path(&probe), Some(shared));
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
