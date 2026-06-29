use std::sync::{Arc, Condvar, Mutex, OnceLock};

use crate::python_input_queue::PythonInputQueue;

pub(super) static SESSION_STATE: OnceLock<Arc<SessionState>> = OnceLock::new();

pub(super) struct SessionState {
    pub(super) inner: Mutex<SessionStateInner>,
    pub(super) cvar: Condvar,
    pub(super) runtime_wake: RuntimeWake,
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
        })
    }

    pub(super) fn notify_all(&self) {
        self.cvar.notify_all();
        self.runtime_wake.wake_queue();
    }
}

#[cfg(target_family = "unix")]
pub(super) struct RuntimeWake {
    queue_read_fd: libc::c_int,
    queue_write_fd: libc::c_int,
    signal_read_fd: libc::c_int,
    signal_write_fd: libc::c_int,
}

#[cfg(target_family = "unix")]
impl RuntimeWake {
    fn new() -> std::io::Result<Self> {
        let (queue_read_fd, queue_write_fd) = create_wake_pipe()?;
        match create_wake_pipe() {
            Ok((signal_read_fd, signal_write_fd)) => Ok(Self {
                queue_read_fd,
                queue_write_fd,
                signal_read_fd,
                signal_write_fd,
            }),
            Err(err) => {
                close_fd(queue_read_fd);
                close_fd(queue_write_fd);
                Err(err)
            }
        }
    }

    pub(super) fn signal_write_fd(&self) -> libc::c_int {
        self.signal_write_fd
    }

    fn wake_queue(&self) {
        write_wake_byte(self.queue_write_fd);
    }

    pub(super) fn wait(&self) -> std::io::Result<()> {
        let mut fds = [
            libc::pollfd {
                fd: self.queue_read_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.signal_read_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
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
            if fds[0].revents != 0 {
                drain_fd(self.queue_read_fd);
            }
            if fds[1].revents != 0 {
                drain_fd(self.signal_read_fd);
            }
            return Ok(());
        }
    }
}

#[cfg(target_family = "unix")]
impl Drop for RuntimeWake {
    fn drop(&mut self) {
        close_fd(self.queue_read_fd);
        close_fd(self.queue_write_fd);
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

#[cfg(not(target_family = "unix"))]
pub(super) struct RuntimeWake;

#[cfg(not(target_family = "unix"))]
impl RuntimeWake {
    fn new() -> std::io::Result<Self> {
        Ok(Self)
    }

    fn wake_queue(&self) {}
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
