#[cfg(windows)]
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::ThreadId;

#[cfg(windows)]
use crate::python_ffi::PythonApi;
use crate::python_input_queue::PythonInputQueue;

pub(super) static SESSION_STATE: OnceLock<Arc<SessionState>> = OnceLock::new();

#[cfg(windows)]
static WINDOWS_SIGNAL_WAKE_EVENT: AtomicIsize = AtomicIsize::new(0);

pub(super) struct SessionState {
    pub(super) inner: Mutex<SessionStateInner>,
    pub(super) cvar: Condvar,
    pub(super) runtime_wake: RuntimeWake,
    pub(super) runtime_thread_id: ThreadId,
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
    pub(super) fn discarded_after_runtime_interrupt(&self) -> bool {
        false
    }
}

pub(super) enum RawStdinReadError {
    Interrupted,
    Runtime(String),
}

impl SessionState {
    pub(super) fn new() -> Result<Self, String> {
        Ok(Self {
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
            runtime_wake: RuntimeWake::new()
                .map_err(|err| format!("failed to create Python runtime wake pipe: {err}"))?,
            runtime_thread_id: std::thread::current().id(),
        })
    }

    pub(super) fn notify_python_input_hook(&self) {
        self.cvar.notify_all();
    }

    pub(super) fn notify_runtime_input_available(&self) {
        self.cvar.notify_all();
        self.runtime_wake.wake_input();
    }

    pub(super) fn notify_runtime_input_closed(&self) {
        self.cvar.notify_all();
        self.runtime_wake.wake_input();
        self.runtime_wake.wake_state();
    }

    pub(super) fn notify_runtime_input_consumer_released(&self) {
        self.cvar.notify_all();
        self.runtime_wake.wake_state();
    }
}

#[cfg(target_family = "unix")]
pub(super) struct RuntimeWake {
    // Keep input, state, and signal wakes separate so non-owner waiters cannot
    // drain the wake needed by the active stdin reader or the runtime thread.
    input_read_fd: libc::c_int,
    input_write_fd: libc::c_int,
    state_read_fd: libc::c_int,
    state_write_fd: libc::c_int,
    signal_read_fd: libc::c_int,
    signal_write_fd: libc::c_int,
}

#[cfg(target_family = "unix")]
impl RuntimeWake {
    fn new() -> std::io::Result<Self> {
        let (input_read_fd, input_write_fd) = create_wake_pipe()?;
        let (state_read_fd, state_write_fd) = match create_wake_pipe() {
            Ok(fds) => fds,
            Err(err) => {
                close_fd(input_read_fd);
                close_fd(input_write_fd);
                return Err(err);
            }
        };
        match create_wake_pipe() {
            Ok((signal_read_fd, signal_write_fd)) => Ok(Self {
                input_read_fd,
                input_write_fd,
                state_read_fd,
                state_write_fd,
                signal_read_fd,
                signal_write_fd,
            }),
            Err(err) => {
                close_fd(input_read_fd);
                close_fd(input_write_fd);
                close_fd(state_read_fd);
                close_fd(state_write_fd);
                Err(err)
            }
        }
    }

    pub(super) fn signal_write_fd(&self) -> libc::c_int {
        self.signal_write_fd
    }

    fn wake_input(&self) {
        write_wake_byte(self.input_write_fd);
    }

    fn wake_state(&self) {
        write_wake_byte(self.state_write_fd);
    }

    pub(super) fn wait_input_or_signal(&self, include_signal: bool) -> std::io::Result<()> {
        self.wait_for_fds(true, false, include_signal)
    }

    pub(super) fn wait_state_or_signal(&self, include_signal: bool) -> std::io::Result<()> {
        self.wait_for_fds(false, true, include_signal)
    }

    fn wait_for_fds(
        &self,
        include_input: bool,
        include_state: bool,
        include_signal: bool,
    ) -> std::io::Result<()> {
        let mut fds = Vec::with_capacity(3);
        if include_input {
            fds.push(libc::pollfd {
                fd: self.input_read_fd,
                events: libc::POLLIN,
                revents: 0,
            });
        }
        if include_state {
            fds.push(libc::pollfd {
                fd: self.state_read_fd,
                events: libc::POLLIN,
                revents: 0,
            });
        }
        if include_signal {
            fds.push(libc::pollfd {
                fd: self.signal_read_fd,
                events: libc::POLLIN,
                revents: 0,
            });
        }
        loop {
            let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
            if rc < 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EINTR) {
                    return Ok(());
                }
                return Err(err);
            }
            if rc == 0 {
                continue;
            }
            for fd in &fds {
                if fd.revents != 0 {
                    drain_fd(fd.fd);
                }
            }
            return Ok(());
        }
    }
}

#[cfg(target_family = "unix")]
impl Drop for RuntimeWake {
    fn drop(&mut self) {
        close_fd(self.input_read_fd);
        close_fd(self.input_write_fd);
        close_fd(self.state_read_fd);
        close_fd(self.state_write_fd);
        close_fd(self.signal_read_fd);
        close_fd(self.signal_write_fd);
    }
}

#[cfg(target_family = "unix")]
fn create_wake_pipe() -> std::io::Result<(libc::c_int, libc::c_int)> {
    let mut fds = [-1; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if let Err(err) = configure_wake_fd(fds[0]).and_then(|()| configure_wake_fd(fds[1])) {
        close_fd(fds[0]);
        close_fd(fds[1]);
        return Err(err);
    }
    Ok((fds[0], fds[1]))
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

#[cfg(windows)]
pub(super) struct RuntimeWake {
    // Keep input, state, and signal wakes separate so non-owner waiters cannot
    // drain the wake needed by the active stdin reader or the runtime thread.
    input_event: isize,
    state_event: isize,
    signal_event: isize,
}

#[cfg(windows)]
impl RuntimeWake {
    fn new() -> std::io::Result<Self> {
        let input_event = create_event()?;
        let state_event = match create_event() {
            Ok(handle) => handle,
            Err(err) => {
                close_handle(input_event);
                return Err(err);
            }
        };
        match create_event() {
            Ok(signal_event) => Ok(Self {
                input_event,
                state_event,
                signal_event,
            }),
            Err(err) => {
                close_handle(input_event);
                close_handle(state_event);
                Err(err)
            }
        }
    }

    pub(super) fn install_signal_wake_handler(&self) -> Result<(), String> {
        WINDOWS_SIGNAL_WAKE_EVENT.store(self.signal_event, Ordering::SeqCst);
        let ok = unsafe {
            windows_sys::Win32::System::Console::SetConsoleCtrlHandler(
                Some(windows_signal_wake_handler),
                1,
            )
        };
        if ok == 0 {
            return Err(format!(
                "failed to install Python Windows signal wake handler: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    fn wake_input(&self) {
        set_event(self.input_event);
    }

    fn wake_state(&self) {
        set_event(self.state_event);
    }

    pub(super) fn wait_input_or_signal(&self, include_signal: bool) -> std::io::Result<()> {
        self.wait_for_events(true, false, include_signal)
    }

    pub(super) fn wait_state_or_signal(&self, include_signal: bool) -> std::io::Result<()> {
        self.wait_for_events(false, true, include_signal)
    }

    fn wait_for_events(
        &self,
        include_input: bool,
        include_state: bool,
        include_signal: bool,
    ) -> std::io::Result<()> {
        let mut handles = Vec::with_capacity(3);
        if include_input {
            handles.push(self.input_event as windows_sys::Win32::Foundation::HANDLE);
        }
        if include_state {
            handles.push(self.state_event as windows_sys::Win32::Foundation::HANDLE);
        }
        if include_signal {
            handles.push(self.signal_event as windows_sys::Win32::Foundation::HANDLE);
        }
        loop {
            let result = unsafe {
                windows_sys::Win32::System::Threading::WaitForMultipleObjects(
                    handles.len() as u32,
                    handles.as_ptr(),
                    0,
                    windows_sys::Win32::System::Threading::INFINITE,
                )
            };
            if result < handles.len() as u32 {
                return Ok(());
            }
            if result == windows_sys::Win32::Foundation::WAIT_FAILED {
                return Err(std::io::Error::last_os_error());
            }
        }
    }
}

#[cfg(windows)]
impl Drop for RuntimeWake {
    fn drop(&mut self) {
        close_handle(self.input_event);
        close_handle(self.state_event);
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

#[cfg(windows)]
unsafe extern "system" fn windows_signal_wake_handler(event: u32) -> i32 {
    let signum = match event {
        windows_sys::Win32::System::Console::CTRL_C_EVENT => libc::SIGINT,
        _ => return 0,
    };
    if let Some(api) = PythonApi::try_global() {
        api.set_interrupt_for_signal(signum);
    }
    let handle = WINDOWS_SIGNAL_WAKE_EVENT.load(Ordering::SeqCst);
    if handle != 0 {
        set_event(handle);
    }
    1
}

#[cfg(not(any(target_family = "unix", windows)))]
pub(super) struct RuntimeWake;

#[cfg(not(any(target_family = "unix", windows)))]
impl RuntimeWake {
    fn new() -> std::io::Result<Self> {
        Ok(Self)
    }

    fn wake_input(&self) {}

    fn wake_state(&self) {}
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
