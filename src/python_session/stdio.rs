use std::ffi::CStr;
#[cfg(target_family = "unix")]
use std::os::unix::io::RawFd;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};

use crate::python_ffi::{PyThreadState, PythonApi};

#[cfg(target_family = "unix")]
use super::unix_stdin;

pub(super) static PYTHON_STDIN_FILE: AtomicPtr<libc::FILE> = AtomicPtr::new(ptr::null_mut());
pub(super) static PYTHON_STDOUT_FILE: AtomicPtr<libc::FILE> = AtomicPtr::new(ptr::null_mut());

pub(super) struct PythonRuntime {
    #[cfg_attr(windows, allow(dead_code))]
    pub(super) stdin: *mut libc::FILE,
}

pub(super) fn open_python_runtime() -> Result<PythonRuntime, String> {
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
    unix_stdin::set_runtime_stdin_fd(runtime_read_fd);
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

pub(super) struct StdioLineRead {
    pub(super) bytes: Vec<u8>,
    pub(super) interrupted: bool,
}

#[cfg(not(any(target_family = "unix", windows)))]
pub(super) fn read_stdio_line_bytes(stdin: *mut libc::FILE) -> StdioLineRead {
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

#[cfg(not(any(target_family = "unix", windows)))]
unsafe fn clear_stdio_error(stdin: *mut libc::FILE) {
    unsafe { libc::clearerr(stdin) };
}

#[cfg(not(any(target_family = "unix", windows)))]
pub(super) fn read_stdio_line_bytes_allowing_python_threads(
    stdin: *mut libc::FILE,
) -> StdioLineRead {
    // _mcp_repl.readline is called from Python with the GIL held. Release it
    // while stdin blocks so the IPC completion path can flush prompt-time plots.
    let _allow_threads = PythonThreadsAllowed::new();
    read_stdio_line_bytes(stdin)
}

pub(super) struct PythonThreadsAllowed {
    api: &'static PythonApi,
    thread_state: *mut PyThreadState,
}

impl PythonThreadsAllowed {
    pub(super) fn new() -> Self {
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
