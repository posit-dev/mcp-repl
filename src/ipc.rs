#![cfg_attr(
    not(any(target_family = "unix", target_family = "windows")),
    allow(dead_code)
)]

use std::collections::VecDeque;
#[cfg(target_family = "windows")]
use std::ffi::c_void;
#[cfg(any(target_family = "unix", target_family = "windows"))]
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
#[cfg(target_family = "unix")]
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
#[cfg(target_family = "windows")]
use std::os::windows::ffi::OsStrExt;
#[cfg(target_family = "windows")]
use std::os::windows::io::{AsRawHandle, FromRawHandle};
#[cfg(target_family = "unix")]
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering as AtomicOrdering};
#[cfg(target_family = "windows")]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
#[cfg(target_family = "windows")]
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_SUCCESS,
    HLOCAL, INVALID_HANDLE_VALUE, LocalFree,
};
#[cfg(target_family = "windows")]
use windows_sys::Win32::Security::Authorization::{
    EXPLICIT_ACCESS_W, GRANT_ACCESS, SetEntriesInAclW, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN,
    TRUSTEE_W,
};
#[cfg(target_family = "windows")]
use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
};
#[cfg(target_family = "windows")]
use windows_sys::Win32::Security::{
    ACL, CopySid, GetLengthSid, GetTokenInformation, InitializeSecurityDescriptor,
    SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SetSecurityDescriptorDacl, TOKEN_GROUPS, TOKEN_QUERY,
    TokenLogonSid,
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
const MAX_PROMPT_HISTORY: usize = 16;
#[cfg(target_family = "unix")]
static WORKER_IPC_ALLOWED: AtomicBool = AtomicBool::new(true);
#[cfg(target_family = "unix")]
static WORKER_IPC_FORK_CLOSE_READ_FD: AtomicI32 = AtomicI32::new(-1);
#[cfg(target_family = "unix")]
static WORKER_IPC_FORK_CLOSE_WRITE_FD: AtomicI32 = AtomicI32::new(-1);
#[cfg(target_family = "unix")]
static WORKER_IPC_ATFORK_REGISTER_RESULT: OnceLock<i32> = OnceLock::new();

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerToWorkerIpcMessage {
    StdinWrite { text: String },
    Interrupt,
    SessionEnd,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerToServerIpcMessage {
    BackendInfo {
        language: String,
        #[serde(default)]
        supports_images: bool,
    },
    ReadlineStart {
        prompt: String,
    },
    ReadlineResult {
        prompt: String,
        line: String,
    },
    PlotImage {
        id: String,
        mime_type: String,
        data: String,
        is_new: bool,
    },
    /// Emitted exactly when the backend knows the logical request has consumed all queued input.
    /// No later `ReadlineResult` should follow for that same request.
    RequestEnd,
    SessionEnd,
}

#[derive(Default)]
struct ServerIpcInbox {
    queue: VecDeque<WorkerToServerIpcMessage>,
    last_prompt: Option<String>,
    prompt_history: VecDeque<String>,
    echo_events: VecDeque<IpcEchoEvent>,
    readline_result_count: u64,
    readline_unmatched_starts: usize,
    readline_unmatched_since: Option<Instant>,
    request_end_seen: bool,
    protocol_warnings: VecDeque<String>,
    session_end: bool,
    disconnected: bool,
}

#[derive(Default)]
struct WorkerIpcInbox {
    queue: VecDeque<ServerToWorkerIpcMessage>,
    disconnected: bool,
}

#[derive(Debug, Clone)]
pub struct IpcEchoEvent {
    pub prompt: String,
    pub line: String,
}

#[derive(Clone)]
pub struct IpcPlotImage {
    pub id: String,
    pub mime_type: String,
    pub data: String,
    pub is_new: bool,
}

#[derive(Default, Clone)]
pub struct IpcHandlers {
    pub on_plot_image: Option<Arc<dyn Fn(IpcPlotImage) + Send + Sync>>,
    pub on_readline_start: Option<Arc<dyn Fn(String) + Send + Sync>>,
    pub on_readline_result: Option<Arc<dyn Fn(IpcEchoEvent) + Send + Sync>>,
    pub on_request_end: Option<Arc<dyn Fn() + Send + Sync>>,
    pub on_session_end: Option<Arc<dyn Fn() + Send + Sync>>,
}

#[derive(Clone)]
pub struct ServerIpcConnection {
    sender: mpsc::Sender<ServerToWorkerIpcMessage>,
    inbox: Arc<Mutex<ServerIpcInbox>>,
    cvar: Arc<Condvar>,
    reader_thread: Arc<Mutex<Option<thread::JoinHandle<()>>>>,
}

#[derive(Clone)]
pub struct WorkerIpcConnection {
    sender: mpsc::Sender<WorkerToServerIpcMessage>,
    inbox: Arc<Mutex<WorkerIpcInbox>>,
    cvar: Arc<Condvar>,
}

#[derive(Clone, Default)]
pub struct IpcHandle {
    inner: Arc<Mutex<Option<ServerIpcConnection>>>,
}

impl IpcHandle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, conn: ServerIpcConnection) {
        let mut guard = self.inner.lock().unwrap();
        *guard = Some(conn);
    }

    pub fn get(&self) -> Option<ServerIpcConnection> {
        let guard = self.inner.lock().unwrap();
        guard.clone()
    }
}

impl ServerIpcConnection {
    fn new(transport: IpcTransport, handlers: IpcHandlers) -> io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let inbox = Arc::new(Mutex::new(ServerIpcInbox::default()));
        let cvar = Arc::new(Condvar::new());
        let reader_thread = Arc::new(Mutex::new(None));

        let reader_inbox = inbox.clone();
        let reader_cvar = cvar.clone();
        let plot_handler = handlers.on_plot_image.clone();
        let readline_start_handler = handlers.on_readline_start.clone();
        let readline_result_handler = handlers.on_readline_result.clone();
        let request_end_handler = handlers.on_request_end.clone();
        let session_end_handler = handlers.on_session_end.clone();
        let IpcTransport { reader, writer } = transport;
        let handle = thread::spawn(move || {
            let mut reader = BufReader::new(reader);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => {
                        let mut guard = reader_inbox.lock().unwrap();
                        guard.disconnected = true;
                        reader_cvar.notify_all();
                        break;
                    }
                    Ok(_) => {}
                    Err(_) => {
                        let mut guard = reader_inbox.lock().unwrap();
                        guard.disconnected = true;
                        reader_cvar.notify_all();
                        break;
                    }
                }
                let trimmed = line.trim_end_matches(['\n', '\r']);
                if trimmed.is_empty() {
                    continue;
                }
                if let Ok(message) = serde_json::from_str::<WorkerToServerIpcMessage>(trimmed) {
                    match message {
                        WorkerToServerIpcMessage::ReadlineStart { prompt } => {
                            let prompt_for_handler = prompt.clone();
                            let mut guard = reader_inbox.lock().unwrap();
                            if guard.request_end_seen {
                                reset_after_completed_request(&mut guard);
                            }
                            guard.readline_unmatched_starts =
                                guard.readline_unmatched_starts.saturating_add(1);
                            if guard.readline_unmatched_starts == 1 {
                                guard.readline_unmatched_since = Some(Instant::now());
                            }
                            if guard
                                .prompt_history
                                .back()
                                .is_none_or(|last| last != &prompt)
                            {
                                guard.prompt_history.push_back(prompt.clone());
                                if guard.prompt_history.len() > MAX_PROMPT_HISTORY {
                                    guard.prompt_history.pop_front();
                                }
                            }
                            guard.last_prompt = Some(prompt);
                            reader_cvar.notify_all();
                            drop(guard);
                            if let Some(handler) = readline_start_handler.as_ref() {
                                handler(prompt_for_handler);
                            }
                        }
                        WorkerToServerIpcMessage::ReadlineResult { prompt, line } => {
                            let echo_event = IpcEchoEvent {
                                prompt: prompt.clone(),
                                line: line.clone(),
                            };
                            let mut guard = reader_inbox.lock().unwrap();
                            if guard.request_end_seen {
                                guard.protocol_warnings.push_back(
                                    "protocol warning: worker emitted ReadlineResult after RequestEnd"
                                        .to_string(),
                                );
                            }
                            guard.readline_result_count =
                                guard.readline_result_count.saturating_add(1);
                            if guard.readline_unmatched_starts > 0 {
                                guard.readline_unmatched_starts -= 1;
                                if guard.readline_unmatched_starts == 0 {
                                    guard.readline_unmatched_since = None;
                                }
                            }
                            guard.echo_events.push_back(echo_event.clone());
                            reader_cvar.notify_all();
                            drop(guard);
                            if let Some(handler) = readline_result_handler.as_ref() {
                                handler(echo_event);
                            }
                        }
                        WorkerToServerIpcMessage::SessionEnd => {
                            let mut guard = reader_inbox.lock().unwrap();
                            guard.session_end = true;
                            guard.queue.push_back(WorkerToServerIpcMessage::SessionEnd);
                            reader_cvar.notify_all();
                            drop(guard);
                            if let Some(handler) = session_end_handler.as_ref() {
                                handler();
                            }
                        }
                        WorkerToServerIpcMessage::PlotImage {
                            id,
                            mime_type,
                            data,
                            is_new,
                        } => {
                            if let Some(handler) = plot_handler.as_ref() {
                                handler(IpcPlotImage {
                                    id,
                                    mime_type,
                                    data,
                                    is_new,
                                });
                            } else {
                                let mut guard = reader_inbox.lock().unwrap();
                                guard.queue.push_back(WorkerToServerIpcMessage::PlotImage {
                                    id,
                                    mime_type,
                                    data,
                                    is_new,
                                });
                                reader_cvar.notify_all();
                            }
                        }
                        other => {
                            let mut guard = reader_inbox.lock().unwrap();
                            let is_request_end =
                                matches!(other, WorkerToServerIpcMessage::RequestEnd);
                            if is_request_end {
                                guard.request_end_seen = true;
                            }
                            guard.queue.push_back(other);
                            reader_cvar.notify_all();
                            drop(guard);
                            if is_request_end && let Some(handler) = request_end_handler.as_ref() {
                                handler();
                            }
                        }
                    }
                }
            }
        });
        *reader_thread.lock().unwrap() = Some(handle);

        spawn_writer(rx, writer);

        Ok(Self {
            sender: tx,
            inbox,
            cvar,
            reader_thread,
        })
    }

    pub fn send(
        &self,
        message: ServerToWorkerIpcMessage,
    ) -> Result<(), mpsc::SendError<ServerToWorkerIpcMessage>> {
        self.sender.send(message)
    }

    pub fn join_reader_thread(&self) -> io::Result<()> {
        let handle = self.reader_thread.lock().unwrap().take();
        let Some(handle) = handle else {
            return Ok(());
        };
        handle
            .join()
            .map_err(|_| io::Error::other("ipc reader thread panicked"))?;
        Ok(())
    }

    pub fn begin_request(&self) {
        let mut guard = self.inbox.lock().unwrap();
        guard
            .queue
            .retain(|msg| !matches!(msg, WorkerToServerIpcMessage::RequestEnd));
        reset_after_completed_request(&mut guard);
        guard.echo_events.clear();
        guard.prompt_history.clear();
        guard.protocol_warnings.clear();
    }

    pub fn waiting_for_next_input(&self, min_wait: Duration) -> bool {
        let guard = self.inbox.lock().unwrap();
        if guard.readline_result_count == 0 || guard.readline_unmatched_starts == 0 {
            return false;
        }
        let Some(since) = guard.readline_unmatched_since else {
            return false;
        };
        since.elapsed() >= min_wait
    }

    pub fn take_prompt_history(&self) -> Vec<String> {
        let mut guard = self.inbox.lock().unwrap();
        guard.prompt_history.drain(..).collect()
    }

    pub fn take_echo_events(&self) -> Vec<IpcEchoEvent> {
        let mut guard = self.inbox.lock().unwrap();
        guard.echo_events.drain(..).collect()
    }

    pub fn pending_echo_event_count(&self) -> usize {
        let guard = self.inbox.lock().unwrap();
        guard.echo_events.len()
    }

    pub fn take_protocol_warnings(&self) -> Vec<String> {
        let mut guard = self.inbox.lock().unwrap();
        guard.protocol_warnings.drain(..).collect()
    }

    pub fn wait_for_request_end(&self, timeout: Duration) -> Result<(), IpcWaitError> {
        let deadline = Instant::now() + timeout;
        let mut guard = self.inbox.lock().unwrap();
        loop {
            if take_request_end(&mut guard) {
                if take_session_end(&mut guard) {
                    return Err(IpcWaitError::SessionEnd);
                }
                return Ok(());
            }
            if take_session_end(&mut guard) {
                return Err(IpcWaitError::SessionEnd);
            }
            if guard.disconnected {
                return Err(IpcWaitError::Disconnected);
            }

            let now = Instant::now();
            if now >= deadline {
                return Err(IpcWaitError::Timeout);
            }
            let remaining = deadline.saturating_duration_since(now);
            let (next_guard, timeout_res) = self.cvar.wait_timeout(guard, remaining).unwrap();
            guard = next_guard;
            if timeout_res.timed_out() {
                return Err(IpcWaitError::Timeout);
            }
        }
    }

    pub fn wait_for_prompt(&self, timeout: Duration) -> Result<String, IpcWaitError> {
        let deadline = Instant::now() + timeout;
        let mut guard = self.inbox.lock().unwrap();
        loop {
            if take_session_end(&mut guard) {
                return Err(IpcWaitError::SessionEnd);
            }
            if guard.disconnected {
                return Err(IpcWaitError::Disconnected);
            }
            if let Some(prompt) = guard.last_prompt.take() {
                return Ok(prompt);
            }

            let now = Instant::now();
            if now >= deadline {
                return Err(IpcWaitError::Timeout);
            }
            let remaining = deadline.saturating_duration_since(now);
            let (next_guard, timeout_res) = self.cvar.wait_timeout(guard, remaining).unwrap();
            guard = next_guard;
            if timeout_res.timed_out() {
                return Err(IpcWaitError::Timeout);
            }
        }
    }

    pub fn try_take_prompt(&self) -> Option<String> {
        let mut guard = self.inbox.lock().unwrap();
        guard.last_prompt.take()
    }

    pub fn wait_for_backend_info(
        &self,
        timeout: Duration,
    ) -> Result<WorkerToServerIpcMessage, IpcWaitError> {
        let deadline = Instant::now() + timeout;
        let mut guard = self.inbox.lock().unwrap();
        loop {
            if let Some(info) = take_backend_info(&mut guard) {
                let _ = take_session_end(&mut guard);
                return Ok(info);
            }
            if take_session_end(&mut guard) {
                return Err(IpcWaitError::SessionEnd);
            }
            if guard.disconnected {
                return Err(IpcWaitError::Disconnected);
            }

            let now = Instant::now();
            if now >= deadline {
                return Err(IpcWaitError::Timeout);
            }
            let remaining = deadline.saturating_duration_since(now);
            let (next_guard, timeout_res) = self.cvar.wait_timeout(guard, remaining).unwrap();
            guard = next_guard;
            if timeout_res.timed_out() {
                return Err(IpcWaitError::Timeout);
            }
        }
    }

    pub fn try_take_request_end(&self) -> bool {
        let mut guard = self.inbox.lock().unwrap();
        take_request_end(&mut guard)
    }
}

impl WorkerIpcConnection {
    fn new(transport: IpcTransport) -> io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let inbox = Arc::new(Mutex::new(WorkerIpcInbox::default()));
        let cvar = Arc::new(Condvar::new());

        let reader_inbox = inbox.clone();
        let reader_cvar = cvar.clone();
        let IpcTransport { reader, writer } = transport;
        thread::spawn(move || {
            let mut reader = BufReader::new(reader);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => {
                        let mut guard = reader_inbox.lock().unwrap();
                        guard.disconnected = true;
                        reader_cvar.notify_all();
                        break;
                    }
                    Ok(_) => {}
                    Err(_) => {
                        let mut guard = reader_inbox.lock().unwrap();
                        guard.disconnected = true;
                        reader_cvar.notify_all();
                        break;
                    }
                }
                let trimmed = line.trim_end_matches(['\n', '\r']);
                if trimmed.is_empty() {
                    continue;
                }
                if let Ok(message) = serde_json::from_str::<ServerToWorkerIpcMessage>(trimmed) {
                    let mut guard = reader_inbox.lock().unwrap();
                    guard.queue.push_back(message);
                    reader_cvar.notify_all();
                }
            }
        });

        spawn_writer(rx, writer);

        Ok(Self {
            sender: tx,
            inbox,
            cvar,
        })
    }

    pub fn send(
        &self,
        message: WorkerToServerIpcMessage,
    ) -> Result<(), mpsc::SendError<WorkerToServerIpcMessage>> {
        self.sender.send(message)
    }

    pub fn recv(&self, timeout: Option<Duration>) -> Option<ServerToWorkerIpcMessage> {
        let mut guard = self.inbox.lock().unwrap();
        if let Some(message) = guard.queue.pop_front() {
            return Some(message);
        }
        if guard.disconnected {
            return None;
        }

        match timeout {
            None => loop {
                guard = self.cvar.wait(guard).unwrap();
                if let Some(message) = guard.queue.pop_front() {
                    return Some(message);
                }
                if guard.disconnected {
                    return None;
                }
            },
            Some(timeout) => {
                let deadline = Instant::now() + timeout;
                loop {
                    let now = Instant::now();
                    if now >= deadline {
                        return None;
                    }
                    let remaining = deadline.saturating_duration_since(now);
                    let (next_guard, timeout_res) =
                        self.cvar.wait_timeout(guard, remaining).unwrap();
                    guard = next_guard;
                    if let Some(message) = guard.queue.pop_front() {
                        return Some(message);
                    }
                    if guard.disconnected {
                        return None;
                    }
                    if timeout_res.timed_out() {
                        return None;
                    }
                }
            }
        }
    }
}

fn spawn_writer<T>(rx: mpsc::Receiver<T>, mut writer: Box<dyn Write + Send>)
where
    T: Serialize + Send + 'static,
{
    thread::spawn(move || {
        for message in rx {
            if let Ok(payload) = serde_json::to_string(&message) {
                if writer.write_all(payload.as_bytes()).is_err() {
                    break;
                }
                if writer.write_all(b"\n").is_err() {
                    break;
                }
                let _ = writer.flush();
            }
        }
    });
}

#[derive(Debug)]
pub enum IpcWaitError {
    Timeout,
    SessionEnd,
    Disconnected,
}

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
            let base = next_pipe_name()?;
            let pipe_name_to_worker = format!("{base}-to-worker");
            let pipe_name_from_worker = format!("{base}-from-worker");
            let server_pipe_to_worker =
                create_named_pipe_server(&pipe_name_to_worker, PIPE_ACCESS_OUTBOUND)?;
            let server_pipe_from_worker =
                create_named_pipe_server(&pipe_name_from_worker, PIPE_ACCESS_INBOUND)?;
            Ok(Self {
                pipe_name_to_worker: Some(pipe_name_to_worker),
                pipe_name_from_worker: Some(pipe_name_from_worker),
                server_pipe_to_worker: Some(server_pipe_to_worker),
                server_pipe_from_worker: Some(server_pipe_from_worker),
            })
        }
        #[cfg(not(any(target_family = "unix", target_family = "windows")))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "IPC sideband is unsupported on this platform",
            ))
        }
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

struct IpcTransport {
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
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
    let nonce = PIPE_COUNTER.fetch_add(1, Ordering::Relaxed);
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
fn create_named_pipe_server(
    pipe_name: &str,
    access_mode: windows_sys::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES,
) -> io::Result<File> {
    let wide = to_wide_nul(pipe_name);
    let mut logon_sid = current_logon_sid()?;
    let mut explicit: EXPLICIT_ACCESS_W = unsafe { std::mem::zeroed() };
    explicit.grfAccessPermissions = FILE_GENERIC_READ | FILE_GENERIC_WRITE;
    explicit.grfAccessMode = GRANT_ACCESS;
    explicit.grfInheritance = 0;
    explicit.Trustee = TRUSTEE_W {
        pMultipleTrustee: std::ptr::null_mut(),
        MultipleTrusteeOperation: 0,
        TrusteeForm: TRUSTEE_IS_SID,
        TrusteeType: TRUSTEE_IS_UNKNOWN,
        ptstrName: logon_sid.as_mut_ptr() as *mut u16,
    };

    let mut dacl: *mut ACL = std::ptr::null_mut();
    let acl_status = unsafe { SetEntriesInAclW(1, &explicit, std::ptr::null_mut(), &mut dacl) };
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

static IPC_GLOBAL: OnceLock<WorkerIpcConnection> = OnceLock::new();

pub fn set_global_ipc(conn: WorkerIpcConnection) {
    let _ = IPC_GLOBAL.set(conn);
}

pub fn global_ipc() -> Option<&'static WorkerIpcConnection> {
    #[cfg(target_family = "unix")]
    if !WORKER_IPC_ALLOWED.load(AtomicOrdering::SeqCst) {
        return None;
    }
    IPC_GLOBAL.get()
}

#[cfg(target_family = "unix")]
extern "C" fn close_worker_ipc_in_fork_child() {
    WORKER_IPC_ALLOWED.store(false, AtomicOrdering::SeqCst);
    let read_fd = WORKER_IPC_FORK_CLOSE_READ_FD.load(AtomicOrdering::SeqCst);
    let write_fd = WORKER_IPC_FORK_CLOSE_WRITE_FD.load(AtomicOrdering::SeqCst);
    unsafe {
        if read_fd >= 0 {
            libc::close(read_fd);
        }
        if write_fd >= 0 {
            libc::close(write_fd);
        }
    }
}

#[cfg(target_family = "unix")]
fn register_worker_ipc_fork_contract(read_fd: RawFd, write_fd: RawFd) -> io::Result<()> {
    let result = *WORKER_IPC_ATFORK_REGISTER_RESULT.get_or_init(|| unsafe {
        libc::pthread_atfork(None, None, Some(close_worker_ipc_in_fork_child))
    });
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result));
    }
    WORKER_IPC_FORK_CLOSE_READ_FD.store(read_fd, AtomicOrdering::SeqCst);
    WORKER_IPC_FORK_CLOSE_WRITE_FD.store(write_fd, AtomicOrdering::SeqCst);
    Ok(())
}

pub fn emit_readline_start(prompt: &str) {
    if let Some(ipc) = global_ipc() {
        let _ = ipc.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: prompt.to_string(),
        });
    }
}

pub fn emit_readline_result(prompt: &str, line: &str) {
    if let Some(ipc) = global_ipc() {
        let _ = ipc.send(WorkerToServerIpcMessage::ReadlineResult {
            prompt: prompt.to_string(),
            line: line.to_string(),
        });
    }
}

pub fn emit_plot_image(id: &str, mime_type: &str, data: &str, is_new: bool) {
    if let Some(ipc) = global_ipc() {
        let _ = ipc.send(WorkerToServerIpcMessage::PlotImage {
            id: id.to_string(),
            mime_type: mime_type.to_string(),
            data: data.to_string(),
            is_new,
        });
    }
}

pub fn emit_backend_info(language: &str, supports_images: bool) {
    if let Some(ipc) = global_ipc() {
        let _ = ipc.send(WorkerToServerIpcMessage::BackendInfo {
            language: language.to_string(),
            supports_images,
        });
    }
}

pub fn emit_request_end() {
    if let Some(ipc) = global_ipc() {
        let _ = ipc.send(WorkerToServerIpcMessage::RequestEnd);
    }
}

pub fn emit_session_end() {
    if let Some(ipc) = global_ipc() {
        let _ = ipc.send(WorkerToServerIpcMessage::SessionEnd);
    }
}

#[cfg(test)]
pub(crate) fn test_connection_pair() -> io::Result<(ServerIpcConnection, WorkerIpcConnection)> {
    let (server_read, worker_write) = std::io::pipe()?;
    let (worker_read, server_write) = std::io::pipe()?;
    let server = ServerIpcConnection::new(
        IpcTransport {
            reader: Box::new(server_read),
            writer: Box::new(server_write),
        },
        IpcHandlers::default(),
    )?;
    let worker = WorkerIpcConnection::new(IpcTransport {
        reader: Box::new(worker_read),
        writer: Box::new(worker_write),
    })?;
    Ok((server, worker))
}

fn take_session_end(guard: &mut ServerIpcInbox) -> bool {
    if !guard.session_end {
        return false;
    }
    guard.session_end = false;
    if let Some(idx) = guard
        .queue
        .iter()
        .position(|msg| matches!(msg, WorkerToServerIpcMessage::SessionEnd))
    {
        guard.queue.remove(idx);
    }
    true
}

fn take_request_end(guard: &mut ServerIpcInbox) -> bool {
    let idx = guard
        .queue
        .iter()
        .position(|msg| matches!(msg, WorkerToServerIpcMessage::RequestEnd));
    let Some(idx) = idx else {
        return false;
    };
    guard.queue.remove(idx);
    true
}

fn reset_after_completed_request(guard: &mut ServerIpcInbox) {
    guard.request_end_seen = false;
    guard.readline_result_count = 0;
    guard.readline_unmatched_starts = 0;
    guard.readline_unmatched_since = None;
    guard.last_prompt = None;
}

fn take_backend_info(guard: &mut ServerIpcInbox) -> Option<WorkerToServerIpcMessage> {
    let idx = guard
        .queue
        .iter()
        .position(|msg| matches!(msg, WorkerToServerIpcMessage::BackendInfo { .. }))?;
    guard.queue.remove(idx)
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
