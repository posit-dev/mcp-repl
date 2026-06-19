use std::io::{self, Read, Write};
use std::time::Duration;

#[cfg(target_family = "windows")]
use std::ffi::c_void;
#[cfg(any(target_family = "unix", target_family = "windows"))]
use std::fs::File;
#[cfg(target_family = "unix")]
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
#[cfg(target_family = "windows")]
use std::os::windows::ffi::OsStrExt;
#[cfg(target_family = "windows")]
use std::os::windows::io::{AsRawHandle, FromRawHandle};
#[cfg(target_family = "windows")]
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
#[cfg(target_family = "windows")]
use std::sync::mpsc;
#[cfg(target_family = "windows")]
use std::thread;
#[cfg(target_family = "windows")]
use std::time::Instant;

#[cfg(target_family = "unix")]
use super::emit::register_worker_ipc_fork_contract;
use super::protocol::IpcHandlers;
use super::server_connection::{IpcHandle, ServerIpcConnection};
use super::worker_connection::WorkerIpcConnection;

#[cfg(target_family = "windows")]
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_SUCCESS,
    HLOCAL, INVALID_HANDLE_VALUE, LocalFree,
};
#[cfg(target_family = "windows")]
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSidToSidW, EXPLICIT_ACCESS_W, GRANT_ACCESS, SetEntriesInAclW, TRUSTEE_IS_SID,
    TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
#[cfg(target_family = "windows")]
use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
};
#[cfg(target_family = "windows")]
use windows_sys::Win32::Security::{
    ACL, CopySid, GetLengthSid, GetTokenInformation, InitializeSecurityDescriptor,
    SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SetSecurityDescriptorDacl, TOKEN_GROUPS, TOKEN_QUERY,
    TOKEN_USER, TokenLogonSid, TokenUser,
};
#[cfg(target_family = "windows")]
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_GENERIC_WRITE, OPEN_EXISTING,
    PIPE_ACCESS_INBOUND, PIPE_ACCESS_OUTBOUND,
};
#[cfg(target_family = "windows")]
use windows_sys::Win32::System::IO::CancelIoEx;
#[cfg(target_family = "windows")]
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
};
#[cfg(target_family = "windows")]
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

#[cfg(target_family = "unix")]
pub const IPC_READ_FD_ENV: &str = "MCP_REPL_IPC_READ_FD";
#[cfg(target_family = "unix")]
pub const IPC_WRITE_FD_ENV: &str = "MCP_REPL_IPC_WRITE_FD";
#[cfg(target_family = "windows")]
pub const IPC_PIPE_TO_WORKER_ENV: &str = "MCP_REPL_IPC_PIPE_TO_WORKER";
#[cfg(target_family = "windows")]
pub const IPC_PIPE_FROM_WORKER_ENV: &str = "MCP_REPL_IPC_PIPE_FROM_WORKER";

pub struct IpcServer {
    #[cfg(target_family = "unix")]
    server_read: Option<std::io::PipeReader>,
    #[cfg(target_family = "unix")]
    server_write: Option<std::io::PipeWriter>,
    #[cfg(target_family = "unix")]
    child_fds: Option<IpcChildFds>,
    #[cfg(target_family = "windows")]
    pipe_name_to_worker: Option<String>,
    #[cfg(target_family = "windows")]
    pipe_name_from_worker: Option<String>,
    #[cfg(target_family = "windows")]
    server_pipe_to_worker: Option<File>,
    #[cfg(target_family = "windows")]
    server_pipe_from_worker: Option<File>,
}

#[cfg(target_family = "unix")]
pub(crate) struct IpcChildFds {
    pub(crate) read_fd: RawFd,
    pub(crate) write_fd: RawFd,
}

impl IpcServer {
    pub fn bind() -> io::Result<Self> {
        #[cfg(target_family = "unix")]
        {
            let (server_read, server_write, child_read, child_write) = create_pipe_pair()?;
            Ok(Self {
                server_read: Some(server_read),
                server_write: Some(server_write),
                child_fds: Some(IpcChildFds {
                    read_fd: child_read,
                    write_fd: child_write,
                }),
            })
        }
        #[cfg(target_family = "windows")]
        {
            Self::bind_with_allowed_sids(&[])
        }
        #[cfg(not(any(target_family = "unix", target_family = "windows")))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "IPC sideband is unsupported on this platform",
            ))
        }
    }

    #[cfg(target_family = "windows")]
    pub fn bind_with_allowed_sids(extra_allowed_sids: &[&str]) -> io::Result<Self> {
        let base = next_pipe_name()?;
        let pipe_name_to_worker = format!("{base}-to-worker");
        let pipe_name_from_worker = format!("{base}-from-worker");
        let server_pipe_to_worker = create_named_pipe_server(
            &pipe_name_to_worker,
            PIPE_ACCESS_OUTBOUND,
            extra_allowed_sids,
        )?;
        let server_pipe_from_worker = create_named_pipe_server(
            &pipe_name_from_worker,
            PIPE_ACCESS_INBOUND,
            extra_allowed_sids,
        )?;
        Ok(Self {
            pipe_name_to_worker: Some(pipe_name_to_worker),
            pipe_name_from_worker: Some(pipe_name_from_worker),
            server_pipe_to_worker: Some(server_pipe_to_worker),
            server_pipe_from_worker: Some(server_pipe_from_worker),
        })
    }

    #[cfg(target_family = "unix")]
    pub fn connect(self, handle: IpcHandle, handlers: IpcHandlers) -> io::Result<()> {
        let Some(server_read) = self.server_read else {
            return Err(io::Error::other("missing ipc read pipe"));
        };
        let Some(server_write) = self.server_write else {
            return Err(io::Error::other("missing ipc write pipe"));
        };
        let conn = ServerIpcConnection::new(
            IpcTransport {
                reader: Box::new(server_read),
                writer: Box::new(server_write),
            },
            handlers,
        )?;
        handle.set(conn);
        crate::diagnostics::startup_log("ipc: connected");
        Ok(())
    }

    #[cfg(target_family = "unix")]
    pub fn take_child_fds(&mut self) -> Option<IpcChildFds> {
        self.child_fds.take()
    }

    #[cfg(target_family = "windows")]
    pub fn connect(
        self,
        handle: IpcHandle,
        handlers: IpcHandlers,
        child: &mut std::process::Child,
        max_wait: Duration,
    ) -> io::Result<()> {
        let Some(server_pipe_to_worker) = self.server_pipe_to_worker else {
            return Err(io::Error::other(
                "missing ipc named pipe handle (to-worker)",
            ));
        };
        let Some(server_pipe_from_worker) = self.server_pipe_from_worker else {
            return Err(io::Error::other(
                "missing ipc named pipe handle (from-worker)",
            ));
        };
        let start = Instant::now();
        connect_named_pipe_with_process_retry(&server_pipe_to_worker, child, max_wait)?;
        let remaining = max_wait.saturating_sub(start.elapsed());
        connect_named_pipe_with_process_retry(&server_pipe_from_worker, child, remaining)?;
        let conn = ServerIpcConnection::new(
            IpcTransport {
                reader: Box::new(server_pipe_from_worker),
                writer: Box::new(server_pipe_to_worker),
            },
            handlers,
        )?;
        handle.set(conn);
        crate::diagnostics::startup_log("ipc: connected");
        Ok(())
    }

    #[cfg(target_family = "windows")]
    pub fn take_pipe_names(&mut self) -> Option<(String, String)> {
        let to_worker = self.pipe_name_to_worker.take()?;
        let from_worker = self.pipe_name_from_worker.take()?;
        Some((to_worker, from_worker))
    }
}

pub(crate) struct IpcTransport {
    pub(crate) reader: Box<dyn Read + Send>,
    pub(crate) writer: Box<dyn Write + Send>,
}

#[cfg(target_family = "unix")]
fn set_cloexec(fd: RawFd, enabled: bool) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    let new_flags = if enabled {
        flags | libc::FD_CLOEXEC
    } else {
        flags & !libc::FD_CLOEXEC
    };
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFD, new_flags) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_family = "unix")]
fn create_pipe_pair() -> io::Result<(std::io::PipeReader, std::io::PipeWriter, RawFd, RawFd)> {
    let (server_read, child_write) = std::io::pipe()?;
    let (child_read, server_write) = std::io::pipe()?;

    let child_read_fd = child_read.into_raw_fd();
    let child_write_fd = child_write.into_raw_fd();

    set_cloexec(child_read_fd, false)?;
    set_cloexec(child_write_fd, false)?;
    set_cloexec(server_read.as_raw_fd(), true)?;
    set_cloexec(server_write.as_raw_fd(), true)?;

    Ok((server_read, server_write, child_read_fd, child_write_fd))
}

#[cfg(target_family = "windows")]
static PIPE_COUNTER: AtomicU64 = AtomicU64::new(1);
#[cfg(target_family = "windows")]
const IPC_CONNECT_TIMEOUT_MESSAGE: &str = "timed out waiting for IPC named pipe client connection";
#[cfg(target_family = "windows")]
const IPC_CONNECT_TIMEOUT_CONNECTOR_STUCK_MESSAGE: &str = "timed out waiting for IPC named pipe client connection; connector thread did not stop after cancellation";

#[cfg(target_family = "windows")]
fn random_pipe_suffix() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            bytes.as_mut_ptr(),
            bytes.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status < 0 {
        return Err(io::Error::other(format!(
            "BCryptGenRandom failed with NTSTATUS 0x{status:08x}"
        )));
    }
    Ok(bytes.iter().map(|value| format!("{value:02x}")).collect())
}

#[cfg(target_family = "windows")]
fn next_pipe_name() -> io::Result<String> {
    let nonce = PIPE_COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
    let random = random_pipe_suffix()?;
    Ok(format!(
        r"\\.\pipe\mcp-repl-ipc-{}-{nonce}-{random}",
        std::process::id()
    ))
}

#[cfg(target_family = "windows")]
fn to_wide_nul(value: &str) -> Vec<u16> {
    let mut wide: Vec<u16> = std::ffi::OsStr::new(value).encode_wide().collect();
    wide.push(0);
    wide
}

#[cfg(target_family = "windows")]
fn current_logon_sid() -> io::Result<Vec<u8>> {
    let mut token = std::ptr::null_mut();
    let open_ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
    if open_ok == 0 {
        return Err(io::Error::last_os_error());
    }

    struct TokenGuard(*mut c_void);
    impl Drop for TokenGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
    }
    let _guard = TokenGuard(token);

    let mut required_len = 0u32;
    unsafe {
        let _ = GetTokenInformation(
            token,
            TokenLogonSid,
            std::ptr::null_mut(),
            0,
            &mut required_len,
        );
    }
    if required_len == 0 {
        return Err(io::Error::last_os_error());
    }

    let mut info = vec![0u8; required_len as usize];
    let info_ok = unsafe {
        GetTokenInformation(
            token,
            TokenLogonSid,
            info.as_mut_ptr() as *mut c_void,
            required_len,
            &mut required_len,
        )
    };
    if info_ok == 0 {
        return Err(io::Error::last_os_error());
    }

    let groups = unsafe { &*(info.as_ptr() as *const TOKEN_GROUPS) };
    if groups.GroupCount == 0 {
        return Err(io::Error::other("token has no logon SID"));
    }
    let sid = groups.Groups[0].Sid;
    if sid.is_null() {
        return Err(io::Error::other("logon SID pointer was null"));
    }

    let sid_len = unsafe { GetLengthSid(sid) };
    if sid_len == 0 {
        return Err(io::Error::last_os_error());
    }

    let mut sid_copy = vec![0u8; sid_len as usize];
    let copy_ok = unsafe { CopySid(sid_len, sid_copy.as_mut_ptr() as *mut c_void, sid) };
    if copy_ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(sid_copy)
}

#[cfg(target_family = "windows")]
fn current_user_sid() -> io::Result<Vec<u8>> {
    let mut token = std::ptr::null_mut();
    let open_ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
    if open_ok == 0 {
        return Err(io::Error::last_os_error());
    }

    struct TokenGuard(*mut c_void);
    impl Drop for TokenGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
    }
    let _guard = TokenGuard(token);

    let mut required_len = 0u32;
    unsafe {
        let _ = GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut required_len);
    }
    if required_len == 0 {
        return Err(io::Error::last_os_error());
    }

    let mut info = vec![0u8; required_len as usize];
    let info_ok = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            info.as_mut_ptr() as *mut c_void,
            required_len,
            &mut required_len,
        )
    };
    if info_ok == 0 {
        return Err(io::Error::last_os_error());
    }

    let user = unsafe { &*(info.as_ptr() as *const TOKEN_USER) };
    let sid = user.User.Sid;
    if sid.is_null() {
        return Err(io::Error::other("user SID pointer was null"));
    }

    let sid_len = unsafe { GetLengthSid(sid) };
    if sid_len == 0 {
        return Err(io::Error::last_os_error());
    }

    let mut sid_copy = vec![0u8; sid_len as usize];
    let copy_ok = unsafe { CopySid(sid_len, sid_copy.as_mut_ptr() as *mut c_void, sid) };
    if copy_ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(sid_copy)
}

#[cfg(target_family = "windows")]
fn create_named_pipe_server(
    pipe_name: &str,
    access_mode: windows_sys::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES,
    extra_allowed_sids: &[&str],
) -> io::Result<File> {
    let wide = to_wide_nul(pipe_name);
    let mut user_sid = current_user_sid()?;
    let mut logon_sid = current_logon_sid()?;
    let mut entries: Vec<EXPLICIT_ACCESS_W> = Vec::new();
    let mut user_explicit: EXPLICIT_ACCESS_W = unsafe { std::mem::zeroed() };
    user_explicit.grfAccessPermissions = FILE_GENERIC_READ | FILE_GENERIC_WRITE;
    user_explicit.grfAccessMode = GRANT_ACCESS;
    user_explicit.grfInheritance = 0;
    user_explicit.Trustee = TRUSTEE_W {
        pMultipleTrustee: std::ptr::null_mut(),
        MultipleTrusteeOperation: 0,
        TrusteeForm: TRUSTEE_IS_SID,
        TrusteeType: TRUSTEE_IS_UNKNOWN,
        ptstrName: user_sid.as_mut_ptr() as *mut u16,
    };
    entries.push(user_explicit);

    let mut logon_explicit: EXPLICIT_ACCESS_W = unsafe { std::mem::zeroed() };
    logon_explicit.grfAccessPermissions = FILE_GENERIC_READ | FILE_GENERIC_WRITE;
    logon_explicit.grfAccessMode = GRANT_ACCESS;
    logon_explicit.grfInheritance = 0;
    logon_explicit.Trustee = TRUSTEE_W {
        pMultipleTrustee: std::ptr::null_mut(),
        MultipleTrusteeOperation: 0,
        TrusteeForm: TRUSTEE_IS_SID,
        TrusteeType: TRUSTEE_IS_UNKNOWN,
        ptstrName: logon_sid.as_mut_ptr() as *mut u16,
    };
    entries.push(logon_explicit);

    let mut extra_sids: Vec<*mut c_void> = Vec::new();
    for extra_allowed_sid in extra_allowed_sids {
        if extra_allowed_sid.is_empty() {
            continue;
        }
        let ok = unsafe {
            let mut sid: *mut c_void = std::ptr::null_mut();
            let ok = ConvertStringSidToSidW(to_wide_nul(extra_allowed_sid).as_ptr(), &mut sid);
            if ok != 0 {
                extra_sids.push(sid);
            }
            ok
        };
        if ok == 0 {
            for sid in extra_sids {
                unsafe {
                    let _ = LocalFree(sid as HLOCAL);
                }
            }
            return Err(io::Error::last_os_error());
        }
        let mut extra_explicit: EXPLICIT_ACCESS_W = unsafe { std::mem::zeroed() };
        extra_explicit.grfAccessPermissions = FILE_GENERIC_READ | FILE_GENERIC_WRITE;
        extra_explicit.grfAccessMode = GRANT_ACCESS;
        extra_explicit.grfInheritance = 0;
        extra_explicit.Trustee = TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: *extra_sids
                .last()
                .expect("extra SID should have been pushed") as *mut u16,
        };
        entries.push(extra_explicit);
    }

    let mut dacl: *mut ACL = std::ptr::null_mut();
    let acl_status = unsafe {
        SetEntriesInAclW(
            entries.len() as u32,
            entries.as_ptr(),
            std::ptr::null_mut(),
            &mut dacl,
        )
    };
    for sid in extra_sids {
        unsafe {
            let _ = LocalFree(sid as HLOCAL);
        }
    }
    if acl_status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(acl_status as i32));
    }

    let mut security_descriptor: SECURITY_DESCRIPTOR = unsafe { std::mem::zeroed() };
    let init_ok = unsafe {
        InitializeSecurityDescriptor(&mut security_descriptor as *mut _ as *mut c_void, 1)
    };
    if init_ok == 0 {
        if !dacl.is_null() {
            unsafe {
                let _ = LocalFree(dacl as HLOCAL);
            }
        }
        return Err(io::Error::last_os_error());
    }
    let dacl_ok = unsafe {
        SetSecurityDescriptorDacl(
            &mut security_descriptor as *mut _ as *mut c_void,
            1,
            dacl,
            0,
        )
    };
    if dacl_ok == 0 {
        if !dacl.is_null() {
            unsafe {
                let _ = LocalFree(dacl as HLOCAL);
            }
        }
        return Err(io::Error::last_os_error());
    }
    let security_attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: &mut security_descriptor as *mut _ as *mut c_void,
        bInheritHandle: 0,
    };
    let handle = unsafe {
        CreateNamedPipeW(
            wide.as_ptr(),
            access_mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            1,
            64 * 1024,
            64 * 1024,
            0,
            &security_attributes,
        )
    };
    if !dacl.is_null() {
        unsafe {
            let _ = LocalFree(dacl as HLOCAL);
        }
    }
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_handle(handle as _) })
}

#[cfg(target_family = "windows")]
fn connect_named_pipe(server_pipe: &File, timeout: Duration) -> io::Result<()> {
    let pipe = server_pipe.as_raw_handle() as usize;
    let (tx, rx) = mpsc::sync_channel(1);
    let connector = thread::spawn(move || {
        let ok = unsafe { ConnectNamedPipe(pipe as *mut c_void, std::ptr::null_mut()) };
        let result = if ok != 0 {
            Ok(())
        } else {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(ERROR_PIPE_CONNECTED as i32) {
                Ok(())
            } else {
                Err(err)
            }
        };
        let _ = tx.send(result);
    });

    wait_for_named_pipe_connect_result(rx, connector, timeout, || unsafe {
        let _ = CancelIoEx(pipe as *mut c_void, std::ptr::null_mut());
    })
}

#[cfg(target_family = "windows")]
fn wait_for_named_pipe_connect_result(
    rx: mpsc::Receiver<io::Result<()>>,
    connector: thread::JoinHandle<()>,
    timeout: Duration,
    on_timeout: impl FnOnce(),
) -> io::Result<()> {
    const CONNECTOR_JOIN_GRACE: Duration = Duration::from_millis(200);

    match rx.recv_timeout(timeout) {
        Ok(result) => {
            let _ = connector.join();
            result
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            on_timeout();
            if !join_connector_with_grace(connector, CONNECTOR_JOIN_GRACE) {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    IPC_CONNECT_TIMEOUT_CONNECTOR_STUCK_MESSAGE,
                ));
            }
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                IPC_CONNECT_TIMEOUT_MESSAGE,
            ))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            let _ = connector.join();
            Err(io::Error::other(
                "ipc named pipe connector thread exited unexpectedly",
            ))
        }
    }
}

#[cfg(target_family = "windows")]
fn join_connector_with_grace(connector: thread::JoinHandle<()>, max_wait: Duration) -> bool {
    let start = Instant::now();
    while !connector.is_finished() {
        if start.elapsed() >= max_wait {
            return false;
        }
        thread::sleep(Duration::from_millis(5));
    }
    let _ = connector.join();
    true
}

#[cfg(target_family = "windows")]
fn connect_named_pipe_with_process_retry(
    server_pipe: &File,
    child: &mut std::process::Child,
    max_wait: Duration,
) -> io::Result<()> {
    connect_named_pipe_with_process_retry_impl(
        |timeout| connect_named_pipe(server_pipe, timeout),
        || child.try_wait().map(|status| status.is_some()),
        max_wait,
    )
}

#[cfg(target_family = "windows")]
fn connect_named_pipe_with_process_retry_impl<ConnectAttempt, ChildExited>(
    mut connect_attempt: ConnectAttempt,
    mut child_exited: ChildExited,
    max_wait: Duration,
) -> io::Result<()>
where
    ConnectAttempt: FnMut(Duration) -> io::Result<()>,
    ChildExited: FnMut() -> io::Result<bool>,
{
    const CONNECT_ATTEMPT_TIMEOUT: Duration = Duration::from_millis(500);
    let deadline = Instant::now() + max_wait;
    loop {
        if child_exited()? {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "worker exited before IPC named pipe connection",
            ));
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                IPC_CONNECT_TIMEOUT_MESSAGE,
            ));
        }
        let timeout = CONNECT_ATTEMPT_TIMEOUT.min(deadline.saturating_duration_since(now));
        match connect_attempt(timeout) {
            Ok(()) => return Ok(()),
            Err(err) if is_retryable_connect_timeout(&err) => continue,
            Err(err) => return Err(err),
        }
    }
}

#[cfg(target_family = "windows")]
fn is_retryable_connect_timeout(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::TimedOut
        && !err
            .to_string()
            .contains(IPC_CONNECT_TIMEOUT_CONNECTOR_STUCK_MESSAGE)
}

#[cfg(target_family = "windows")]
fn open_named_pipe_client(pipe_name: &str, access: u32) -> io::Result<File> {
    let wide = to_wide_nul(pipe_name);
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            access,
            0,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_handle(handle as _) })
}

#[cfg(target_family = "windows")]
fn should_retry_pipe_open(err: &io::Error) -> bool {
    matches!(
        err.raw_os_error(),
        Some(code) if code == ERROR_FILE_NOT_FOUND as i32 || code == ERROR_PIPE_BUSY as i32
    )
}

#[cfg(target_family = "windows")]
fn take_pipe_pair_if_ready<Reader, Writer>(
    reader: &mut Option<Reader>,
    writer: &mut Option<Writer>,
) -> Option<(Reader, Writer)> {
    if reader.is_some() && writer.is_some() {
        Some((
            reader.take().expect("reader should be present"),
            writer.take().expect("writer should be present"),
        ))
    } else {
        None
    }
}

pub fn connect_from_env(_timeout: Duration) -> io::Result<WorkerIpcConnection> {
    #[cfg(target_family = "unix")]
    {
        let read_fd = std::env::var(IPC_READ_FD_ENV)
            .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "IPC read fd missing"))?;
        let write_fd = std::env::var(IPC_WRITE_FD_ENV)
            .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "IPC write fd missing"))?;
        let read_fd: RawFd = read_fd
            .parse()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid IPC read fd"))?;
        let write_fd: RawFd = write_fd
            .parse()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid IPC write fd"))?;
        set_cloexec(read_fd, true)?;
        set_cloexec(write_fd, true)?;
        register_worker_ipc_fork_contract(read_fd, write_fd)?;
        // The main worker owns the live sideband fds. Once startup has consumed the bootstrap env
        // vars, user code and descendants must not see or reuse them.
        // SAFETY: worker startup consumes these env vars before any worker-managed threads exist.
        unsafe {
            std::env::remove_var(IPC_READ_FD_ENV);
            std::env::remove_var(IPC_WRITE_FD_ENV);
        }
        let reader = unsafe { File::from_raw_fd(read_fd) };
        let writer = unsafe { File::from_raw_fd(write_fd) };
        WorkerIpcConnection::new(IpcTransport {
            reader: Box::new(reader),
            writer: Box::new(writer),
        })
    }
    #[cfg(target_family = "windows")]
    {
        let timeout = _timeout;
        let pipe_to_worker = std::env::var(IPC_PIPE_TO_WORKER_ENV)
            .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "IPC to-worker pipe missing"))?;
        let pipe_from_worker = std::env::var(IPC_PIPE_FROM_WORKER_ENV)
            .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "IPC from-worker pipe missing"))?;
        let deadline = Instant::now() + timeout;
        let mut reader: Option<File> = None;
        let mut writer: Option<File> = None;
        let mut last_err: Option<io::Error> = None;
        loop {
            if reader.is_none() {
                match open_named_pipe_client(&pipe_to_worker, FILE_GENERIC_READ) {
                    Ok(file) => reader = Some(file),
                    Err(err) => {
                        if !should_retry_pipe_open(&err) {
                            return Err(err);
                        }
                        last_err = Some(err);
                    }
                }
            }
            if writer.is_none() {
                match open_named_pipe_client(&pipe_from_worker, FILE_GENERIC_WRITE) {
                    Ok(file) => writer = Some(file),
                    Err(err) => {
                        if !should_retry_pipe_open(&err) {
                            return Err(err);
                        }
                        last_err = Some(err);
                    }
                }
            }

            if let Some((reader, writer)) = take_pipe_pair_if_ready(&mut reader, &mut writer) {
                return WorkerIpcConnection::new(IpcTransport {
                    reader: Box::new(reader),
                    writer: Box::new(writer),
                });
            }

            if timeout.is_zero() || Instant::now() >= deadline {
                return Err(last_err.unwrap_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        "timed out connecting to IPC named pipes",
                    )
                }));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
    #[cfg(not(any(target_family = "unix", target_family = "windows")))]
    {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "IPC sideband is unsupported on this platform",
        ))
    }
}

#[cfg(all(test, target_family = "windows"))]
mod tests {
    use super::{
        connect_named_pipe_with_process_retry_impl, take_pipe_pair_if_ready,
        wait_for_named_pipe_connect_result,
    };
    use std::io;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn named_pipe_timeout_does_not_wait_for_slow_connector_join() {
        let (_tx, rx) = mpsc::sync_channel::<io::Result<()>>(1);
        let (cancel_tx, cancel_rx) = mpsc::sync_channel::<()>(1);
        let connector = thread::spawn(move || {
            let _ = cancel_rx.recv();
            thread::sleep(Duration::from_secs(2));
        });

        let start = Instant::now();
        let result =
            wait_for_named_pipe_connect_result(rx, connector, Duration::from_millis(10), || {
                let _ = cancel_tx.send(());
            });

        assert!(matches!(result, Err(err) if err.kind() == io::ErrorKind::TimedOut));
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "timeout path blocked too long: {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn connect_retry_stops_after_uncancelled_timeout_error() {
        let attempts = AtomicUsize::new(0);
        let result = connect_named_pipe_with_process_retry_impl(
            |_| {
                attempts.fetch_add(1, Ordering::Relaxed);
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out waiting for IPC named pipe client connection; connector thread did not stop after cancellation",
                ))
            },
            || Ok(false),
            Duration::from_millis(10),
        );

        assert!(matches!(result, Err(err) if err.kind() == io::ErrorKind::TimedOut));
        assert_eq!(
            attempts.load(Ordering::Relaxed),
            1,
            "uncancelled timeout should abort retries to avoid stacking connector threads",
        );
    }

    #[test]
    fn take_pipe_pair_if_ready_keeps_reader_until_writer_is_ready() {
        let mut reader = Some("reader".to_string());
        let mut writer: Option<String> = None;

        let pair = take_pipe_pair_if_ready(&mut reader, &mut writer);
        assert!(pair.is_none());
        assert_eq!(reader.as_deref(), Some("reader"));
        assert!(writer.is_none());
    }

    #[test]
    fn take_pipe_pair_if_ready_keeps_writer_until_reader_is_ready() {
        let mut reader: Option<String> = None;
        let mut writer = Some("writer".to_string());

        let pair = take_pipe_pair_if_ready(&mut reader, &mut writer);
        assert!(pair.is_none());
        assert!(reader.is_none());
        assert_eq!(writer.as_deref(), Some("writer"));
    }

    #[test]
    fn take_pipe_pair_if_ready_returns_pair_when_both_present() {
        let mut reader = Some("reader".to_string());
        let mut writer = Some("writer".to_string());

        let pair = take_pipe_pair_if_ready(&mut reader, &mut writer).expect("pair");
        assert_eq!(pair.0, "reader");
        assert_eq!(pair.1, "writer");
        assert!(reader.is_none());
        assert!(writer.is_none());
    }
}
