use std::collections::{HashMap, VecDeque};
use std::ffi::{CStr, CString};
use std::hash::{Hash, Hasher};
use std::os::raw::{c_char, c_int, c_uchar};
use std::path::{Path, PathBuf};
#[cfg(target_family = "unix")]
use std::sync::atomic::AtomicI32;
#[cfg(windows)]
use std::sync::atomic::AtomicIsize;
#[cfg(any(target_family = "unix", windows))]
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::thread;
#[cfg(not(any(target_family = "unix", windows)))]
use std::time::Duration;

use crate::ipc;
#[cfg(target_family = "unix")]
use crate::sandbox::R_SESSION_TMPDIR_ENV;
use crate::worker_protocol::TextStream;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use harp::command::{r_command, r_command_from_path};
use harp::exec::{RFunction, RFunctionExt};
use harp::library::RLibraries;

#[cfg(target_family = "unix")]
use libr::{
    R_Consolefile, R_Outputfile, ptr_R_Busy, ptr_R_ReadConsole, ptr_R_ShowMessage, ptr_R_Suicide,
    ptr_R_WriteConsole, ptr_R_WriteConsoleEx,
};
#[cfg(target_family = "windows")]
use libr::{
    R_DefParamsEx, R_SetParams, R_common_command_line, Rboolean_FALSE, Rboolean_TRUE, Rstart,
    UImode_RTerm, cmdlineoptions, get_R_HOME, getRUser, readconsolecfg,
};
#[cfg(target_family = "windows")]
use std::mem::MaybeUninit;
#[cfg(target_family = "windows")]
use windows_sys::Win32::Globalization::{GetACP, MultiByteToWideChar};

const MCP_REPL_R_SCRIPT: &str = include_str!("../r/mcp_repl.R");
#[cfg(not(any(target_family = "unix", windows)))]
const R_READ_CONSOLE_INTERRUPT_POLL_INTERVAL: Duration = Duration::from_millis(25);

#[cfg(target_family = "unix")]
static R_SIGINT_WAKE_WRITE_FD: AtomicI32 = AtomicI32::new(-1);
#[cfg(windows)]
static WINDOWS_R_SIGNAL_WAKE_EVENT: AtomicIsize = AtomicIsize::new(0);

pub struct RSession {
    init: Arc<SessionInit>,
}

impl RSession {
    pub fn global() -> Result<&'static RSession, String> {
        SESSION
            .get()
            .ok_or_else(|| "R session not initialized".to_string())
    }

    pub fn start_on_current_thread() -> Result<(), String> {
        let init = Arc::new(SessionInit::new());
        let session = RSession { init: init.clone() };
        let session_set = SESSION.set(session);
        if session_set.is_err() {
            return Err("R session already initialized".to_string());
        }
        run_session_on_current_thread(init)
    }

    pub fn wait_until_ready(&self) -> Result<(), String> {
        self.init.wait_ready()
    }

    pub fn begin_input(&self, input: String) -> Result<(), String> {
        self.wait_until_ready()?;
        let state = session_state();
        let mut guard = state.inner.lock().unwrap();
        if guard.active_input {
            return Err("input_batch arrived while input is active".to_string());
        }
        guard.active_input = true;
        queue_input(&mut guard.input_queue, &input);
        state.notify_runtime_input_available();
        Ok(())
    }

    pub fn request_shutdown(&self) -> Result<(), String> {
        self.wait_until_ready()?;
        let state = session_state();
        let mut guard = state.inner.lock().unwrap();
        // Preserve already accepted input; reset replies include output produced
        // while the old worker drains to a safe runtime boundary.
        guard.shutdown = true;
        state.notify_runtime_input_closed();
        Ok(())
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

pub(crate) fn clear_pending_input() -> bool {
    let Some(state) = SESSION_STATE.get() else {
        return false;
    };
    let mut guard = state.inner.lock().unwrap();
    let had_pending = !guard.input_queue.is_empty();
    drain_input_queue(&mut guard.input_queue);
    had_pending
}

pub(crate) fn discard_unconsumed_input_for_discard_ack() -> bool {
    let Some(state) = SESSION_STATE.get() else {
        return false;
    };
    let mut guard = state.inner.lock().unwrap();
    let had_pending = !guard.input_queue.is_empty();
    drain_input_queue(&mut guard.input_queue);
    had_pending
}

fn run_session_on_current_thread(init: Arc<SessionInit>) -> Result<(), String> {
    crate::diagnostics::startup_log("r-session: init begin");
    let state = Arc::new(SessionState::new());
    if SESSION_STATE.set(state.clone()).is_err() {
        let message = "R session state already initialized".to_string();
        init.mark_failed(message.clone());
        return Err(message);
    }

    let init_start = std::time::Instant::now();
    let init_result = initialize_r(&init);
    if let Err(err) = init_result {
        init.mark_failed(err.clone());
        return Err(err);
    }
    #[cfg(target_family = "unix")]
    install_r_sigint_wake_handler(&state)?;
    #[cfg(windows)]
    install_windows_r_signal_wake_handler(&state)?;
    crate::diagnostics::startup_log(format!(
        "r-session: init complete ({} ms)",
        crate::diagnostics::elapsed_ms(init_start.elapsed())
    ));

    unsafe {
        libr::run_Rmainloop();
    }

    Ok(())
}

struct SessionState {
    inner: Mutex<SessionStateInner>,
    cvar: Condvar,
    #[cfg(any(target_family = "unix", windows))]
    runtime_input_wake: RuntimeInputWake,
}

struct SessionStateInner {
    active_input: bool,
    input_queue: VecDeque<InputBatchLine>,
    plot_hashes: HashMap<String, u64>,
    last_prompt: Option<String>,
    shutdown: bool,
    session_end_emitted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct InputBatchLine {
    text: String,
}

impl SessionState {
    fn new() -> Self {
        Self {
            inner: Mutex::new(SessionStateInner {
                active_input: false,
                input_queue: VecDeque::new(),
                plot_hashes: HashMap::new(),
                last_prompt: None,
                shutdown: false,
                session_end_emitted: false,
            }),
            cvar: Condvar::new(),
            #[cfg(target_family = "unix")]
            runtime_input_wake: RuntimeInputWake::new()
                .expect("failed to create R runtime input wake pipe"),
            #[cfg(windows)]
            runtime_input_wake: RuntimeInputWake::new()
                .expect("failed to create R runtime input wake event"),
        }
    }

    fn notify_runtime_input_available(&self) {
        self.notify_runtime_input_waiters();
    }

    fn notify_runtime_input_closed(&self) {
        self.notify_runtime_input_waiters();
    }

    fn notify_runtime_input_waiters(&self) {
        self.cvar.notify_all();
        #[cfg(any(target_family = "unix", windows))]
        self.runtime_input_wake.notify();
    }
}

#[cfg(target_family = "unix")]
struct RuntimeInputWake {
    read_fd: libc::c_int,
    write_fd: libc::c_int,
}

#[cfg(target_family = "unix")]
impl RuntimeInputWake {
    fn new() -> std::io::Result<Self> {
        let mut fds = [-1; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if let Err(err) = configure_wake_fd(fds[0]).and_then(|()| configure_wake_fd(fds[1])) {
            close_fd(fds[0]);
            close_fd(fds[1]);
            return Err(err);
        }
        Ok(Self {
            read_fd: fds[0],
            write_fd: fds[1],
        })
    }

    fn notify(&self) {
        write_wake_byte(self.write_fd);
    }

    fn write_fd(&self) -> libc::c_int {
        self.write_fd
    }

    fn wait_interruptibly(&self) -> std::io::Result<()> {
        loop {
            let mut readfds = unsafe { std::mem::zeroed::<libc::fd_set>() };
            unsafe {
                libc::FD_ZERO(&mut readfds);
                libc::FD_SET(self.read_fd, &mut readfds);
            }
            let result = unsafe {
                libc::select(
                    self.read_fd + 1,
                    &mut readfds,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if result < 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EINTR) {
                    drain_fd(self.read_fd);
                    return Ok(());
                }
                return Err(err);
            }
            if result == 0 {
                continue;
            }
            drain_fd(self.read_fd);
            return Ok(());
        }
    }
}

#[cfg(target_family = "unix")]
impl Drop for RuntimeInputWake {
    fn drop(&mut self) {
        close_fd(self.read_fd);
        close_fd(self.write_fd);
    }
}

#[cfg(windows)]
struct RuntimeInputWake {
    queue_event: isize,
    signal_event: isize,
}

#[cfg(windows)]
impl RuntimeInputWake {
    fn new() -> std::io::Result<Self> {
        let queue_event = create_event()?;
        match create_event() {
            Ok(signal_event) => Ok(Self {
                queue_event,
                signal_event,
            }),
            Err(err) => {
                close_handle(queue_event);
                Err(err)
            }
        }
    }

    fn notify(&self) {
        set_event(self.queue_event);
    }

    fn signal_event(&self) -> isize {
        self.signal_event
    }

    fn wait_interruptibly(&self) -> std::io::Result<()> {
        let handles = [
            self.queue_event as windows_sys::Win32::Foundation::HANDLE,
            self.signal_event as windows_sys::Win32::Foundation::HANDLE,
        ];
        loop {
            let result = unsafe {
                windows_sys::Win32::System::Threading::WaitForMultipleObjects(
                    handles.len() as u32,
                    handles.as_ptr(),
                    0,
                    windows_sys::Win32::System::Threading::INFINITE,
                )
            };
            if result == windows_sys::Win32::Foundation::WAIT_OBJECT_0
                || result == windows_sys::Win32::Foundation::WAIT_OBJECT_0 + 1
            {
                return Ok(());
            }
            if result == windows_sys::Win32::Foundation::WAIT_FAILED {
                return Err(std::io::Error::last_os_error());
            }
        }
    }
}

#[cfg(windows)]
impl Drop for RuntimeInputWake {
    fn drop(&mut self) {
        close_handle(self.queue_event);
        close_handle(self.signal_event);
    }
}

#[cfg(windows)]
fn create_event() -> std::io::Result<isize> {
    let handle = unsafe {
        windows_sys::Win32::System::Threading::CreateEventW(
            std::ptr::null(),
            0,
            0,
            std::ptr::null(),
        )
    };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    Ok(handle as isize)
}

#[cfg(windows)]
fn set_event(handle: isize) {
    let _ = unsafe { windows_sys::Win32::System::Threading::SetEvent(handle as _) };
}

#[cfg(windows)]
fn close_handle(handle: isize) {
    if handle != 0 {
        let _ = unsafe { windows_sys::Win32::Foundation::CloseHandle(handle as _) };
    }
}

#[cfg(target_family = "unix")]
fn configure_wake_fd(fd: libc::c_int) -> std::io::Result<()> {
    let fd_flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if fd_flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFD, fd_flags | libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error());
    }

    let status_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if status_flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, status_flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_family = "unix")]
fn write_wake_byte(fd: libc::c_int) {
    loop {
        let rc = unsafe { libc::write(fd, [1u8].as_ptr().cast(), 1) };
        if rc == 1 {
            return;
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EAGAIN) => return,
            _ => return,
        }
    }
}

#[cfg(target_family = "unix")]
fn drain_fd(fd: libc::c_int) {
    let mut buffer = [0u8; 64];
    loop {
        let rc = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
        if rc > 0 {
            continue;
        }
        if rc == 0 {
            return;
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EAGAIN) => return,
            _ => return,
        }
    }
}

#[cfg(target_family = "unix")]
fn close_fd(fd: libc::c_int) {
    if fd >= 0 {
        let _ = unsafe { libc::close(fd) };
    }
}

#[cfg(target_family = "unix")]
fn install_r_sigint_wake_handler(state: &SessionState) -> Result<(), String> {
    R_SIGINT_WAKE_WRITE_FD.store(state.runtime_input_wake.write_fd(), Ordering::SeqCst);
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = r_sigint_wake_handler as *const () as libc::sighandler_t;
    action.sa_flags = 0;
    let mask_result = unsafe { libc::sigemptyset(&mut action.sa_mask) };
    if mask_result < 0 {
        return Err(format!(
            "failed to initialize R SIGINT wake handler mask: {}",
            std::io::Error::last_os_error()
        ));
    }
    let result = unsafe { libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut()) };
    if result < 0 {
        return Err(format!(
            "failed to install R SIGINT wake handler: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

#[cfg(target_family = "unix")]
extern "C" fn r_sigint_wake_handler(_signal: libc::c_int) {
    // Match R's default SIGINT handler by marking an interrupt pending, then
    // wake our managed ReadConsole wait so the main R thread can check it.
    unsafe {
        *libr::R_interrupts_pending = 1;
    }
    let fd = R_SIGINT_WAKE_WRITE_FD.load(Ordering::SeqCst);
    if fd >= 0 {
        write_wake_byte(fd);
    }
}

#[cfg(windows)]
fn install_windows_r_signal_wake_handler(state: &SessionState) -> Result<(), String> {
    WINDOWS_R_SIGNAL_WAKE_EVENT.store(state.runtime_input_wake.signal_event(), Ordering::SeqCst);
    let ok = unsafe {
        windows_sys::Win32::System::Console::SetConsoleCtrlHandler(
            Some(windows_r_signal_wake_handler),
            1,
        )
    };
    if ok == 0 {
        return Err(format!(
            "failed to install R Windows signal wake handler: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

#[cfg(windows)]
unsafe extern "system" fn windows_r_signal_wake_handler(event: u32) -> i32 {
    if event == windows_sys::Win32::System::Console::CTRL_C_EVENT {
        unsafe {
            *libr::R_interrupts_pending = 1;
        }
        let handle = WINDOWS_R_SIGNAL_WAKE_EVENT.load(Ordering::SeqCst);
        if handle != 0 {
            set_event(handle);
        }
        return 1;
    }
    0
}

fn initialize_r(init: &SessionInit) -> Result<(), String> {
    let start = std::time::Instant::now();
    prepare_r_home_env();
    let r_home = setup_r_home().map_err(|err| format!("failed to set up R_HOME: {err}"))?;
    crate::diagnostics::startup_log(format!(
        "r-session: r_home_setup {} ms",
        crate::diagnostics::elapsed_ms(start.elapsed())
    ));
    configure_r_env_vars(&r_home);
    #[cfg(target_family = "unix")]
    configure_r_tempdir();
    #[cfg(windows)]
    configure_windows_r_dll_search(&r_home);

    let libs_start = std::time::Instant::now();
    let libraries = RLibraries::from_r_home_path(&r_home);
    libraries.initialize_pre_setup_r();
    crate::diagnostics::startup_log(format!(
        "r-session: libraries pre-setup {} ms",
        crate::diagnostics::elapsed_ms(libs_start.elapsed())
    ));

    // Mirror the default R startup as closely as possible: delegate to R's
    // own initialization logic (Rf_initialize_R + command-line parsing) and
    // avoid disabling user/site startup files.
    //
    // We keep the console quiet and interactive, and preserve the existing
    // behavior of not restoring/saving the workspace automatically.
    let args = vec![
        "--quiet".to_string(),
        "--interactive".to_string(),
        "--no-restore".to_string(),
        "--no-save".to_string(),
    ];
    let setup_start = std::time::Instant::now();
    setup_r(&args, init)?;
    crate::diagnostics::startup_log(format!(
        "r-session: setup_r {} ms",
        crate::diagnostics::elapsed_ms(setup_start.elapsed())
    ));

    let post_start = std::time::Instant::now();
    libraries.initialize_post_setup_r();
    crate::diagnostics::startup_log(format!(
        "r-session: libraries post-setup {} ms",
        crate::diagnostics::elapsed_ms(post_start.elapsed())
    ));

    unsafe {
        harp::CONSOLE_THREAD_ID = Some(thread::current().id());
        harp::routines::r_register_routines();
    }
    harp::initialize();
    let help_start = std::time::Instant::now();
    configure_r_help_output()?;
    crate::diagnostics::startup_log(format!(
        "r-session: help output setup {} ms",
        crate::diagnostics::elapsed_ms(help_start.elapsed())
    ));

    crate::diagnostics::startup_log(format!(
        "r-session: initialize_r total {} ms",
        crate::diagnostics::elapsed_ms(start.elapsed())
    ));
    Ok(())
}

fn setup_r_home() -> Result<PathBuf, String> {
    let home = match std::env::var("R_HOME") {
        Ok(home) => home,
        Err(_) => {
            let output = r_command_from_path(|command| {
                command.arg("RHOME");
            })
            .map_err(|err| format!("Can't find R or `R_HOME`: {err}"))?;

            String::from_utf8(output.stdout)
                .map_err(|err| format!("Invalid UTF-8 from R RHOME output: {err}"))?
                .trim()
                .to_string()
        }
    };

    let path = PathBuf::from(&home);
    match path.try_exists() {
        Ok(true) => {}
        Ok(false) => {
            return Err(format!(
                "The `R_HOME` path '{}' does not exist.",
                path.display()
            ));
        }
        Err(err) => return Err(format!("Can't check if `R_HOME` path exists: {err}")),
    }

    unsafe {
        std::env::set_var("R_HOME", &home);
    }
    r_command(&path, |command| {
        command.arg("RHOME");
    })
    .map_err(|err| format!("Can't run R: {err}"))?;

    Ok(path)
}

#[cfg(windows)]
fn configure_windows_r_dll_search(r_home: &Path) {
    let r_bin = if cfg!(target_arch = "aarch64") {
        r_home.join("bin")
    } else {
        r_home.join("bin").join("x64")
    };
    if r_bin.is_dir() {
        let mut paths = vec![r_bin];
        if let Some(existing) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&existing));
        }
        if let Ok(joined) = std::env::join_paths(paths) {
            unsafe {
                std::env::set_var("PATH", joined);
            }
        }
    }
    harp::sys::library::set_use_standard_dll_search_path(true);
}

fn prepare_r_home_env() {
    normalize_empty_r_home_env();
    #[cfg(windows)]
    prepare_windows_r_user_env();
    #[cfg(windows)]
    if std::env::var_os("R_HOME").is_none()
        && let Some(r_home) = discover_windows_r_home()
    {
        unsafe {
            std::env::set_var("R_HOME", r_home);
        }
    }
}

#[cfg(windows)]
fn prepare_windows_r_user_env() {
    let Some(home) = windows_r_user_home() else {
        return;
    };
    for key in ["R_USER", "HOME"] {
        if std::env::var_os(key).is_none_or(|value| value.is_empty()) {
            unsafe {
                std::env::set_var(key, &home);
            }
        }
    }
}

#[cfg(windows)]
fn windows_r_user_home() -> Option<PathBuf> {
    for key in [crate::sandbox::R_SESSION_TMPDIR_ENV, "TEMP", "TMP"] {
        let Some(path) = std::env::var_os(key).filter(|value| !value.is_empty()) else {
            continue;
        };
        let path = PathBuf::from(path);
        if path.is_absolute() && path.is_dir() {
            return Some(path);
        }
    }
    None
}

fn normalize_empty_r_home_env() {
    let empty_keys: Vec<_> = std::env::vars_os()
        .filter_map(|(key, value)| {
            let is_r_home = key
                .to_str()
                .is_some_and(|key| key.eq_ignore_ascii_case("R_HOME"));
            (is_r_home && value.is_empty()).then_some(key)
        })
        .collect();
    for key in empty_keys {
        unsafe {
            std::env::remove_var(key);
        }
    }
}

#[cfg(windows)]
fn discover_windows_r_home() -> Option<PathBuf> {
    let mut roots = Vec::new();
    for key in ["ProgramW6432", "ProgramFiles"] {
        if let Some(root) = std::env::var_os(key).map(PathBuf::from) {
            roots.push(root.join("R"));
        }
    }
    roots.push(PathBuf::from(r"C:\Program Files\R"));
    roots.sort();
    roots.dedup();

    let mut candidates = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_versioned_r_dir = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("R-"));
            if is_versioned_r_dir && is_windows_r_home_candidate(&path) {
                candidates.push(path);
            }
        }
    }

    candidates.sort();
    candidates.pop()
}

#[cfg(windows)]
fn is_windows_r_home_candidate(path: &Path) -> bool {
    path.join("share").is_dir()
        && path.join("include").is_dir()
        && path.join("doc").is_dir()
        && (path.join("bin").join("x64").join("R.dll").is_file()
            || path.join("bin").join("R.dll").is_file())
}

fn configure_r_help_output() -> Result<(), String> {
    eval_in_global_env(MCP_REPL_R_SCRIPT)
}

fn eval_in_global_env(code: &str) -> Result<(), String> {
    // Parse from explicit lines so CRLF content from include_str! on Windows
    // cannot leak carriage returns into the parser input stream.
    let parse_lines: Vec<String> = code
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .split('\n')
        .map(str::to_string)
        .collect();
    let mut parse = RFunction::from("parse");
    parse.param("text", parse_lines);
    let exprs = parse
        .call()
        .map_err(|err| format!("failed to parse R startup code: {err}"))?;

    let mut globalenv_fn = RFunction::from("globalenv");
    let globalenv = globalenv_fn
        .call()
        .map_err(|err| format!("failed to resolve globalenv(): {err}"))?;

    let mut eval = RFunction::from("eval");
    eval.add(exprs);
    eval.param("envir", globalenv);
    eval.call()
        .map_err(|err| format!("failed to eval R startup code: {err}"))?;
    Ok(())
}

fn configure_r_env_vars(r_home: &Path) {
    unsafe {
        std::env::set_var("R_DISABLE_HTTPD", "1");
    }

    let share = r_home.join("share");
    let include = r_home.join("include");
    let doc = r_home.join("doc");

    let ok = share.try_exists().unwrap_or(false)
        && include.try_exists().unwrap_or(false)
        && doc.try_exists().unwrap_or(false);

    if ok {
        unsafe {
            std::env::set_var("R_SHARE_DIR", share);
            std::env::set_var("R_INCLUDE_DIR", include);
            std::env::set_var("R_DOC_DIR", doc);
        }
        return;
    }

    // Fallback for non-standard R layouts.
    let result = r_command(r_home, |command| {
        command
            .stdin(std::process::Stdio::null())
            .args([
                "--no-restore",
                "--no-save",
                "--no-site-file",
                "--no-init-file",
            ])
            .arg("--slave")
            .arg("-e")
            .arg(r#"cat(paste(R.home("share"), R.home("include"), R.home("doc"), sep=";"))"#);
    });

    if let Ok(output) = result
        && let Ok(vars) = String::from_utf8(output.stdout)
    {
        let vars: Vec<&str> = vars.trim().split(';').collect();
        if vars.len() == 3 {
            unsafe {
                std::env::set_var("R_SHARE_DIR", vars[0]);
                std::env::set_var("R_INCLUDE_DIR", vars[1]);
                std::env::set_var("R_DOC_DIR", vars[2]);
            }
        } else {
            eprintln!("Unexpected output for R env vars");
        }
    } else {
        eprintln!("Failed to discover R env vars");
    }
}

#[cfg(target_family = "unix")]
fn configure_r_tempdir() {
    let Some(tmpdir) = std::env::var_os(R_SESSION_TMPDIR_ENV) else {
        return;
    };
    if tmpdir.is_empty() {
        return;
    }
    let path = PathBuf::from(&tmpdir);
    if !path.is_absolute() {
        eprintln!(
            "Ignoring non-absolute R session temp dir: {}",
            path.to_string_lossy()
        );
        return;
    }
    if path.as_path() == std::path::Path::new("/") {
        eprintln!("Refusing to use '/' as R session temp dir");
        return;
    }

    unsafe {
        std::env::set_var("TMPDIR", &tmpdir);
    }
}

#[cfg(target_family = "unix")]
fn setup_r(args: &[String], init: &SessionInit) -> Result<(), String> {
    unsafe {
        let (owned_args, mut c_args) = build_c_args_owned(args);
        let _ = R_MAIN_ARGS.set(owned_args);
        libr::Rf_initialize_R(c_args.len() as i32, c_args.as_mut_ptr());

        libr::set(libr::R_Interactive, 1);
        libr::set(R_Consolefile, std::ptr::null_mut());
        libr::set(R_Outputfile, std::ptr::null_mut());

        libr::set(ptr_R_WriteConsole, None);
        libr::set(ptr_R_WriteConsoleEx, Some(r_write_console));
        libr::set(ptr_R_ReadConsole, Some(r_read_console));
        libr::set(ptr_R_ShowMessage, Some(r_show_message));
        libr::set(ptr_R_Busy, Some(r_busy));
        libr::set(ptr_R_Suicide, Some(r_suicide));

        init.mark_ready();
        ipc::emit_worker_ready("r", true);
        libr::setup_Rmainloop();
    }

    Ok(())
}

#[cfg(target_family = "windows")]
fn setup_r(args: &[String], init: &SessionInit) -> Result<(), String> {
    unsafe {
        libr::set(libr::R_SignalHandlers, 1);

        let r_home = get_r_home();
        let r_home = CString::new(r_home).map_err(|err| err.to_string())?;
        let r_home = r_home.as_ptr() as *mut c_char;

        let user_home = get_user_home();
        let user_home = CString::new(user_home).map_err(|err| err.to_string())?;
        let user_home = user_home.as_ptr() as *mut c_char;

        let (_tmp_owned, mut c_args) = build_c_args_owned(&[]);
        cmdlineoptions(c_args.len() as i32, c_args.as_mut_ptr());

        let mut params_struct = MaybeUninit::uninit();
        let params: Rstart = params_struct.as_mut_ptr();

        R_DefParamsEx(params, 0);

        let (owned_args, mut c_args) = build_c_args_owned(args);
        let _ = R_MAIN_ARGS.set(owned_args);
        let mut c_args_len = c_args.len() as c_int;
        R_common_command_line(&mut c_args_len, c_args.as_mut_ptr(), params);

        (*params).R_Interactive = 1;
        (*params).CharacterMode = UImode_RTerm;
        // Keep startup behavior aligned with R defaults. R_common_command_line
        // already adjusts these based on the provided command-line arguments.
        (*params).set_NoRenviron(Rboolean_FALSE);

        (*params).WriteConsole = None;
        (*params).WriteConsoleEx = Some(r_write_console);
        (*params).ReadConsole = Some(r_read_console);
        (*params).ShowMessage = Some(r_show_message);
        (*params).YesNoCancel = Some(r_yes_no_cancel);
        (*params).Busy = Some(r_busy);
        (*params).Suicide = Some(r_suicide);
        (*params).CallBack = Some(r_callback);
        // Windows R embeds UTF-8 spans in console output using UTF8in/UTF8out markers.
        // Explicitly enable this (required when using Rstart version 0) so output
        // encoding is deterministic and can be decoded reliably.
        (*params).EmitEmbeddedUTF8 = Rboolean_TRUE;

        (*params).rhome = r_home;
        (*params).home = user_home;

        R_SetParams(params);
        libr::graphapp::GA_initapp(0, std::ptr::null_mut());
        readconsolecfg();
        init.mark_ready();
        ipc::emit_worker_ready("r", true);
        libr::setup_Rmainloop();
    }

    Ok(())
}

fn build_c_args_owned(args: &[String]) -> (Vec<CString>, Vec<*mut c_char>) {
    let mut owned = Vec::with_capacity(args.len() + 1);
    owned.push(CString::new("mcp-repl").expect("argv[0] must not contain NUL"));
    for arg in args {
        owned.push(CString::new(arg.as_str()).expect("argv must not contain NUL"));
    }
    let ptrs = owned
        .iter()
        .map(|arg| arg.as_ptr() as *mut c_char)
        .collect();
    (owned, ptrs)
}

static SESSION_STATE: OnceLock<Arc<SessionState>> = OnceLock::new();
static SESSION: OnceLock<RSession> = OnceLock::new();
static R_MAIN_ARGS: OnceLock<Vec<CString>> = OnceLock::new();

fn session_state() -> &'static Arc<SessionState> {
    SESSION_STATE
        .get()
        .expect("R session state was not initialized")
}

fn queue_input(queue: &mut VecDeque<InputBatchLine>, input: &str) {
    if input.is_empty() {
        return;
    }
    let mut lines: Vec<String> = input.split_inclusive('\n').map(str::to_string).collect();
    if !input.ends_with('\n') {
        if let Some(last) = lines.last_mut() {
            last.push('\n');
        } else {
            lines.push("\n".to_string());
        }
    }
    queue.extend(lines.into_iter().map(|text| InputBatchLine { text }));
}

fn drain_input_queue(queue: &mut VecDeque<InputBatchLine>) -> String {
    let mut drained = String::new();
    while let Some(line) = queue.pop_front() {
        drained.push_str(&line.text);
    }
    drained
}

#[cfg(target_family = "unix")]
fn wait_until_console_input_changes<'a>(
    state: &'a SessionState,
    mut guard: MutexGuard<'a, SessionStateInner>,
) {
    loop {
        if guard.shutdown {
            break;
        }
        if !guard.input_queue.is_empty() {
            drop(guard);
            unsafe {
                libr::R_CheckUserInterrupt();
            }
            guard = state.inner.lock().unwrap();
            if guard.shutdown || !guard.input_queue.is_empty() {
                break;
            }
            continue;
        }
        drop(guard);
        state
            .runtime_input_wake
            .wait_interruptibly()
            .expect("R runtime input wake wait failed");
        unsafe {
            libr::R_CheckUserInterrupt();
        }
        guard = state.inner.lock().unwrap();
    }
}

#[cfg(windows)]
fn wait_until_console_input_changes<'a>(
    state: &'a SessionState,
    mut guard: MutexGuard<'a, SessionStateInner>,
) {
    loop {
        if guard.shutdown {
            break;
        }
        if !guard.input_queue.is_empty() {
            drop(guard);
            unsafe {
                libr::R_CheckUserInterrupt();
            }
            guard = state.inner.lock().unwrap();
            if guard.shutdown || !guard.input_queue.is_empty() {
                break;
            }
            continue;
        }
        drop(guard);
        state
            .runtime_input_wake
            .wait_interruptibly()
            .expect("R runtime input wake wait failed");
        unsafe {
            libr::R_CheckUserInterrupt();
        }
        guard = state.inner.lock().unwrap();
    }
}

#[cfg(not(any(target_family = "unix", windows)))]
fn wait_until_console_input_changes<'a>(
    state: &'a SessionState,
    mut guard: MutexGuard<'a, SessionStateInner>,
) {
    loop {
        if guard.shutdown {
            break;
        }
        if !guard.input_queue.is_empty() {
            drop(guard);
            unsafe {
                libr::R_CheckUserInterrupt();
            }
            guard = state.inner.lock().unwrap();
            if guard.shutdown || !guard.input_queue.is_empty() {
                break;
            }
            continue;
        }

        let (next_guard, _) = state
            .cvar
            .wait_timeout(guard, R_READ_CONSOLE_INTERRUPT_POLL_INTERVAL)
            .unwrap();
        guard = next_guard;
        drop(guard);
        unsafe {
            libr::R_CheckUserInterrupt();
        }
        guard = state.inner.lock().unwrap();
    }
}

fn split_console_line(
    mut line: InputBatchLine,
    max: usize,
) -> (InputBatchLine, Option<InputBatchLine>) {
    if line.text.len() <= max {
        return (line, None);
    }
    let mut split = max;
    while split > 0 && !line.text.is_char_boundary(split) {
        split -= 1;
    }
    assert!(split > 0, "R console buffer is too small for UTF-8 input");
    let tail = line.text.split_off(split);
    (line, Some(InputBatchLine { text: tail }))
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
        Err(err) => panic!("failed to send R output over worker IPC: {err}"),
    }
}

#[cfg(target_family = "windows")]
const UTF8_IN_MARKER: &[u8; 3] = b"\x02\xFF\xFE";
#[cfg(target_family = "windows")]
const UTF8_OUT_MARKER: &[u8; 3] = b"\x03\xFF\xFE";
#[cfg(target_family = "windows")]
static WINDOWS_CONSOLE_DECODE_STATE: OnceLock<Mutex<WindowsConsoleDecodeStates>> = OnceLock::new();

#[cfg(target_family = "windows")]
fn find_marker(bytes: &[u8], marker: &[u8; 3]) -> Option<usize> {
    bytes
        .windows(marker.len())
        .position(|window| window == marker)
}

#[cfg(target_family = "windows")]
fn decode_windows_code_page_bytes(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    if bytes.len() > i32::MAX as usize {
        return String::from_utf8_lossy(bytes).into_owned();
    }

    let code_page = unsafe { GetACP() };

    let input_len = bytes.len() as i32;
    let wide_len = unsafe {
        MultiByteToWideChar(
            code_page,
            0,
            bytes.as_ptr(),
            input_len,
            std::ptr::null_mut(),
            0,
        )
    };
    if wide_len <= 0 {
        return String::from_utf8_lossy(bytes).into_owned();
    }

    let mut wide = vec![0u16; wide_len as usize];
    let written = unsafe {
        MultiByteToWideChar(
            code_page,
            0,
            bytes.as_ptr(),
            input_len,
            wide.as_mut_ptr(),
            wide_len,
        )
    };
    if written <= 0 {
        return String::from_utf8_lossy(bytes).into_owned();
    }

    String::from_utf16_lossy(&wide[..written as usize])
}

#[cfg(target_family = "windows")]
fn decode_windows_embedded_segment(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => decode_windows_code_page_bytes(bytes),
    }
}

#[cfg(target_family = "windows")]
#[derive(Default)]
struct WindowsConsoleDecodeState {
    marker_tail: Vec<u8>,
    utf8_segment: Vec<u8>,
    in_utf8_segment: bool,
}

#[cfg(target_family = "windows")]
#[derive(Default)]
struct WindowsConsoleDecodeStates {
    stdout: WindowsConsoleDecodeState,
    stderr: WindowsConsoleDecodeState,
}

#[cfg(target_family = "windows")]
fn trailing_marker_prefix_len(bytes: &[u8], markers: &[&[u8; 3]]) -> usize {
    let mut keep = 0usize;
    for marker in markers {
        for prefix_len in (1..marker.len()).rev() {
            if bytes.ends_with(&marker[..prefix_len]) {
                keep = keep.max(prefix_len);
                break;
            }
        }
    }
    keep
}

#[cfg(target_family = "windows")]
fn decode_console_bytes_with_state(state: &mut WindowsConsoleDecodeState, bytes: &[u8]) -> Vec<u8> {
    if bytes.is_empty() {
        return Vec::new();
    }

    let mut input = Vec::with_capacity(state.marker_tail.len() + bytes.len());
    input.extend_from_slice(&state.marker_tail);
    state.marker_tail.clear();
    input.extend_from_slice(bytes);

    let mut out = String::new();
    let mut cursor = 0usize;

    while cursor < input.len() {
        let remaining = &input[cursor..];
        if !state.in_utf8_segment {
            if remaining.starts_with(UTF8_IN_MARKER) {
                state.in_utf8_segment = true;
                cursor += UTF8_IN_MARKER.len();
                continue;
            }
            if remaining.starts_with(UTF8_OUT_MARKER) {
                cursor += UTF8_OUT_MARKER.len();
                continue;
            }

            let next_in = find_marker(remaining, UTF8_IN_MARKER);
            let next_out = find_marker(remaining, UTF8_OUT_MARKER);
            let next = match (next_in, next_out) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            };
            if let Some(next) = next {
                if next > 0 {
                    out.push_str(&decode_windows_code_page_bytes(&remaining[..next]));
                    cursor += next;
                }
                continue;
            }

            let keep = trailing_marker_prefix_len(remaining, &[UTF8_IN_MARKER, UTF8_OUT_MARKER]);
            let split = remaining.len().saturating_sub(keep);
            if split > 0 {
                out.push_str(&decode_windows_code_page_bytes(&remaining[..split]));
            }
            if keep > 0 {
                state.marker_tail.extend_from_slice(&remaining[split..]);
            }
            break;
        }

        if let Some(end_rel) = find_marker(remaining, UTF8_OUT_MARKER) {
            let end = cursor + end_rel;
            state.utf8_segment.extend_from_slice(&input[cursor..end]);
            out.push_str(&decode_windows_embedded_segment(&state.utf8_segment));
            state.utf8_segment.clear();
            state.in_utf8_segment = false;
            cursor = end + UTF8_OUT_MARKER.len();
            continue;
        }

        let keep = trailing_marker_prefix_len(remaining, &[UTF8_OUT_MARKER]);
        let split = remaining.len().saturating_sub(keep);
        if split > 0 {
            state.utf8_segment.extend_from_slice(&remaining[..split]);
        }
        if keep > 0 {
            state.marker_tail.extend_from_slice(&remaining[split..]);
        }
        break;
    }

    out.into_bytes()
}

#[cfg(target_family = "windows")]
fn decode_console_bytes_for_channel(otype: c_int, bytes: &[u8]) -> Vec<u8> {
    let state = WINDOWS_CONSOLE_DECODE_STATE.get_or_init(|| Mutex::new(Default::default()));
    let mut guard = state.lock().unwrap();
    if otype == 0 {
        decode_console_bytes_with_state(&mut guard.stdout, bytes)
    } else {
        decode_console_bytes_with_state(&mut guard.stderr, bytes)
    }
}

#[cfg(all(test, target_family = "windows"))]
fn reset_console_decode_state_for_tests() {
    if let Some(state) = WINDOWS_CONSOLE_DECODE_STATE.get() {
        let mut guard = state.lock().unwrap();
        *guard = WindowsConsoleDecodeStates::default();
    }
}

#[cfg(not(target_family = "windows"))]
fn decode_console_bytes_for_channel(_otype: c_int, bytes: &[u8]) -> Vec<u8> {
    bytes.to_vec()
}

fn complete_session_if_needed(emit_session_end: bool) {
    if emit_session_end {
        ipc::emit_session_end();
    }
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn r_write_console(buf: *const c_char, buflen: c_int, otype: c_int) {
    if buf.is_null() || buflen <= 0 {
        return;
    }
    let bytes = unsafe { std::slice::from_raw_parts(buf as *const u8, buflen as usize) };
    let bytes = decode_console_bytes_for_channel(otype, bytes);
    let stream = if otype == 0 {
        TextStream::Stdout
    } else {
        TextStream::Stderr
    };
    emit_output_text(stream, &bytes);
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn r_show_message(buf: *const c_char) {
    if buf.is_null() {
        return;
    }
    let message = unsafe { CStr::from_ptr(buf) }.to_string_lossy();
    let mut bytes = Vec::with_capacity(message.len() + 1);
    bytes.extend_from_slice(message.as_bytes());
    bytes.push(b'\n');
    emit_output_text(TextStream::Stderr, &bytes);
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn r_busy(which: c_int) {
    let _ = which;
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn r_suicide(buf: *const c_char) {
    let message = if buf.is_null() {
        "R requested shutdown."
    } else {
        unsafe { CStr::from_ptr(buf) }
            .to_str()
            .unwrap_or("R requested shutdown.")
    };
    let state = session_state();
    let mut guard = state.inner.lock().unwrap();
    let should_emit = !guard.session_end_emitted;
    guard.session_end_emitted = true;
    drop(guard);
    complete_session_if_needed(should_emit);
    panic!("{message}");
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn r_read_console(
    prompt: *const c_char,
    buf: *mut c_uchar,
    buflen: c_int,
    add_history: c_int,
) -> c_int {
    let _ = add_history;
    if buflen <= 0 {
        return 0;
    }
    let prompt_text = if prompt.is_null() {
        None
    } else {
        Some(
            unsafe { CStr::from_ptr(prompt) }
                .to_string_lossy()
                .to_string(),
        )
    };
    let prompt = prompt_text.as_deref().unwrap_or("");
    let is_save_prompt = prompt_text
        .as_deref()
        .map(|text| text.to_ascii_lowercase().contains("save workspace image"))
        .unwrap_or(false);
    let state = session_state();
    {
        let mut guard = state.inner.lock().unwrap();
        guard.last_prompt = Some(prompt.to_string());
        if guard.input_queue.is_empty() && !guard.active_input {
            guard.plot_hashes.clear();
        }
    }

    loop {
        let mut guard = state.inner.lock().unwrap();

        if is_save_prompt {
            let should_emit = guard.shutdown && !guard.session_end_emitted;
            if guard.shutdown {
                guard.session_end_emitted = true;
            }
            drop(guard);
            complete_session_if_needed(should_emit);
            if !buf.is_null() {
                let response = b"n\n";
                let max = (buflen as usize).saturating_sub(1);
                if max > 0 {
                    let bytes = if response.len() > max {
                        &response[..max]
                    } else {
                        response
                    };
                    unsafe {
                        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf, bytes.len());
                        *buf.add(bytes.len()) = 0;
                    }
                    return 1;
                }
            }
            return 0;
        }

        if let Some(line) = guard.input_queue.pop_front() {
            let max = (buflen as usize).saturating_sub(1);
            let (line_text, tail) = split_console_line(line, max);
            if let Some(remainder) = tail {
                guard.input_queue.push_front(remainder);
            }
            drop(guard);

            let head = line_text.text.as_bytes();
            if !buf.is_null() {
                unsafe {
                    std::ptr::copy_nonoverlapping(head.as_ptr(), buf, head.len());
                    *buf.add(head.len()) = 0;
                }
            }
            ipc::emit_input_line(prompt, &line_text.text);

            return 1;
        }

        if guard.shutdown {
            let should_emit = !guard.session_end_emitted;
            guard.session_end_emitted = true;
            drop(guard);
            complete_session_if_needed(should_emit);
            if !buf.is_null() {
                unsafe { *buf = 0 };
            }
            return 0;
        }

        if guard.active_input {
            guard.active_input = false;
            guard.plot_hashes.clear();
            drop(guard);
            ipc::emit_input_wait(prompt);
            let guard = state.inner.lock().unwrap();
            wait_until_console_input_changes(state, guard);
            continue;
        }

        let prompt = prompt.to_string();
        drop(guard);
        ipc::emit_input_wait(&prompt);
        let guard = state.inner.lock().unwrap();
        wait_until_console_input_changes(state, guard);
    }
}

pub(crate) fn push_plot_image(
    plot_id: String,
    bytes: Vec<u8>,
    mime_type: String,
    is_new: bool,
) -> Result<(), String> {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    let hash = hasher.finish();

    {
        let state = session_state();
        let mut guard = state
            .inner
            .lock()
            .map_err(|_| "session state lock poisoned".to_string())?;

        if guard.plot_hashes.get(&plot_id) == Some(&hash) {
            return Ok(());
        }

        guard.plot_hashes.insert(plot_id.clone(), hash);
    }

    let mime_type = if mime_type.trim().is_empty() {
        "image/png".to_string()
    } else {
        mime_type
    };
    let data = STANDARD.encode(bytes);
    ipc::emit_output_image(&mime_type, &data, !is_new, Some(&plot_id));

    Ok(())
}

#[cfg(test)]
mod input_queue_tests {
    use std::collections::VecDeque;

    use super::{InputBatchLine, queue_input, split_console_line};

    #[test]
    fn queue_input_splits_input_into_console_lines() {
        let mut queue = VecDeque::new();

        queue_input(&mut queue, "alpha\nbeta");

        let queued = queue.into_iter().collect::<Vec<_>>();
        assert_eq!(
            queued,
            vec![
                InputBatchLine {
                    text: "alpha\n".to_string(),
                },
                InputBatchLine {
                    text: "beta\n".to_string(),
                },
            ]
        );
    }

    #[test]
    fn split_console_line_preserves_text_remainder() {
        let line = InputBatchLine {
            text: "abcdef\n".to_string(),
        };

        let (head, tail) = split_console_line(line, 3);

        assert_eq!(
            head,
            InputBatchLine {
                text: "abc".to_string(),
            }
        );
        assert_eq!(
            tail,
            Some(InputBatchLine {
                text: "def\n".to_string(),
            })
        );
    }
}

#[cfg(target_family = "windows")]
#[unsafe(no_mangle)]
pub extern "C-unwind" fn r_yes_no_cancel(_question: *const c_char) -> c_int {
    // In embedded Windows sessions this callback can be reached during cleanup
    // when R asks whether to save the workspace image. Returning -1 requests
    // "no save", which keeps shutdown non-interactive.
    -1
}

#[cfg(target_family = "windows")]
#[unsafe(no_mangle)]
pub extern "C-unwind" fn r_callback() {}

#[cfg(target_family = "windows")]
fn get_r_home() -> String {
    let r_path = unsafe { get_R_HOME() };
    if r_path.is_null() {
        panic!("get_R_HOME failed to report an R home.");
    }
    unsafe { CStr::from_ptr(r_path) }
        .to_string_lossy()
        .to_string()
}

#[cfg(target_family = "windows")]
fn get_user_home() -> String {
    let r_path = unsafe { getRUser() };
    if r_path.is_null() {
        panic!("getRUser failed to report a user home directory.");
    }
    unsafe { CStr::from_ptr(r_path) }
        .to_string_lossy()
        .to_string()
}

#[cfg(all(test, target_family = "windows"))]
mod tests {
    use std::sync::{Mutex, OnceLock};

    use super::{
        UTF8_IN_MARKER, UTF8_OUT_MARKER, decode_console_bytes_for_channel,
        reset_console_decode_state_for_tests,
    };

    fn test_mutex() -> &'static Mutex<()> {
        static TEST_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
        TEST_MUTEX.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn decode_console_bytes_strips_embedded_utf8_markers() {
        let _guard = test_mutex().lock().expect("test mutex");
        reset_console_decode_state_for_tests();

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"[1] \"");
        bytes.extend_from_slice(UTF8_IN_MARKER);
        bytes.extend_from_slice(b"after interrupt");
        bytes.extend_from_slice(UTF8_OUT_MARKER);
        bytes.extend_from_slice(b"\"\n");

        let decoded = decode_console_bytes_for_channel(0, &bytes);
        let text = String::from_utf8(decoded).expect("decoder must produce UTF-8");

        assert_eq!(text, "[1] \"after interrupt\"\n");
    }

    #[test]
    fn decode_console_bytes_preserves_embedded_utf8_text() {
        let _guard = test_mutex().lock().expect("test mutex");
        reset_console_decode_state_for_tests();

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"R Help on ");
        bytes.extend_from_slice(UTF8_IN_MARKER);
        let quoted = format!("{}mean{}", '\u{2018}', '\u{2019}');
        bytes.extend_from_slice(quoted.as_bytes());
        bytes.extend_from_slice(UTF8_OUT_MARKER);
        bytes.extend_from_slice(b"\n");

        let decoded = decode_console_bytes_for_channel(0, &bytes);
        let text = String::from_utf8(decoded).expect("decoder must produce UTF-8");

        assert_eq!(text, format!("R Help on {quoted}\n"));
    }

    #[test]
    fn decode_console_bytes_handles_markers_split_across_callbacks() {
        let _guard = test_mutex().lock().expect("test mutex");
        reset_console_decode_state_for_tests();

        let chunk1 = b"[1] \"\x02\xff";
        let chunk2 = b"\xfeafter interrupt\x03\xff";
        let chunk3 = b"\xfe\"\n";

        let out1 = decode_console_bytes_for_channel(0, chunk1);
        let out2 = decode_console_bytes_for_channel(0, chunk2);
        let out3 = decode_console_bytes_for_channel(0, chunk3);

        let merged = [out1, out2, out3].concat();
        let text = String::from_utf8(merged).expect("decoder must produce UTF-8");
        assert_eq!(text, "[1] \"after interrupt\"\n");
    }

    #[test]
    fn decode_console_bytes_does_not_mix_stdout_stderr_marker_state() {
        let _guard = test_mutex().lock().expect("test mutex");
        reset_console_decode_state_for_tests();

        let out1 = decode_console_bytes_for_channel(0, b"\x02\xff\xfecaf");
        let out2 = decode_console_bytes_for_channel(1, b"ERR\n");
        let out3 = decode_console_bytes_for_channel(0, b"\x03\xff\xfe");

        assert!(
            out1.is_empty(),
            "stdout partial UTF-8 segment should be buffered"
        );
        let stderr = String::from_utf8(out2).expect("stderr output should remain UTF-8");
        assert_eq!(stderr, "ERR\n");
        let stdout_tail = String::from_utf8(out3).expect("stdout output should remain UTF-8");
        assert_eq!(stdout_tail, "caf");
    }
}

#[cfg(all(test, not(target_family = "windows")))]
mod non_windows_tests {
    use super::decode_console_bytes_for_channel;

    #[test]
    fn decode_console_bytes_passthrough_on_non_windows_stdout() {
        let input = b"plain output\n";
        assert_eq!(decode_console_bytes_for_channel(0, input), input);
    }

    #[test]
    fn decode_console_bytes_passthrough_on_non_windows_stderr() {
        let input = b"error output\n";
        assert_eq!(decode_console_bytes_for_channel(1, input), input);
    }
}
