#![cfg_attr(
    not(any(target_family = "unix", target_family = "windows")),
    allow(dead_code)
)]

use std::collections::{HashMap, VecDeque};
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
use std::sync::atomic::{AtomicBool, AtomicI32};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
#[cfg(target_family = "windows")]
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::output_capture::OutputTextSource;
use crate::worker_protocol::TextStream;
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
const OUTPUT_TEXT_IPC_CHUNK_BYTES: usize = 8 * 1024;
#[cfg(target_family = "unix")]
static WORKER_IPC_ALLOWED: AtomicBool = AtomicBool::new(true);
#[cfg(target_family = "unix")]
static WORKER_IPC_FORK_CLOSE_READ_FD: AtomicI32 = AtomicI32::new(-1);
#[cfg(target_family = "unix")]
static WORKER_IPC_FORK_CLOSE_WRITE_FD: AtomicI32 = AtomicI32::new(-1);
static NEXT_SERVER_IMAGE_ID: AtomicU64 = AtomicU64::new(0);
#[cfg(target_family = "unix")]
static WORKER_IPC_ATFORK_REGISTER_RESULT: OnceLock<i32> = OnceLock::new();

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerToWorkerIpcMessage {
    StdinWrite {
        byte_len: usize,
        #[serde(default)]
        line_count: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        final_prompt: Option<String>,
    },
    StdinWriteComplete,
    Interrupt,
    SessionEnd,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerToServerIpcMessage {
    WorkerReady {
        protocol: WorkerProtocol,
        worker: WorkerIdentity,
        capabilities: WorkerCapabilities,
        #[serde(default)]
        graceful_shutdown: Option<WorkerGracefulShutdown>,
    },
    BackendInfo {
        #[serde(default)]
        supports_images: bool,
    },
    StdinWriteAck,
    OutputText {
        stream: TextStream,
        data_b64: String,
        #[serde(default, skip_serializing_if = "is_false")]
        is_continuation: bool,
    },
    ReadlineStart {
        prompt: String,
        #[serde(default = "default_true")]
        client_waiting: bool,
    },
    ReadlineInput {
        text: String,
    },
    ReadlineDiscard {
        text: String,
    },
    ReadlineResult {
        prompt: String,
        line: String,
    },
    PlotImage {
        mime_type: String,
        data: String,
        is_update: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    OutputImage {
        image_id: String,
        mime_type: String,
        data_b64: String,
        update: bool,
    },
    SessionEnd {
        #[serde(default)]
        reason: Option<String>,
        #[serde(default)]
        message_b64: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerProtocol {
    pub name: String,
    pub version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerIdentity {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerCapabilities {
    #[serde(default)]
    pub images: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerGracefulShutdown {
    pub stdin: String,
}

fn default_true() -> bool {
    true
}

#[derive(Default)]
struct ServerIpcInbox {
    queue: VecDeque<WorkerToServerIpcMessage>,
    startup_message_seen: bool,
    last_prompt: Option<String>,
    prompt_history: VecDeque<String>,
    echo_events: VecDeque<IpcEchoEvent>,
    active_stdin: Option<VecDeque<u8>>,
    readline_result_count: u64,
    readline_unmatched_starts: usize,
    readline_unmatched_since: Option<Instant>,
    current_image_id: Option<String>,
    request_image_id: Option<String>,
    plot_source_image_ids: HashMap<String, String>,
    request_plot_source_image_ids: HashMap<String, String>,
    protocol_warnings: VecDeque<String>,
    protocol_error: Option<String>,
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
    pub source: OutputTextSource,
}

#[derive(Clone)]
pub struct IpcOutputText {
    pub stream: TextStream,
    pub bytes: Vec<u8>,
    pub is_continuation: bool,
}

#[derive(Clone)]
pub struct IpcPlotImage {
    pub id: String,
    pub mime_type: String,
    pub data: String,
    pub is_new: bool,
    pub updates_previous_image: bool,
    pub readline_results_seen: usize,
}

#[derive(Default, Clone)]
pub struct IpcHandlers {
    pub on_output_text: Option<Arc<dyn Fn(IpcOutputText) + Send + Sync>>,
    pub on_plot_image: Option<Arc<dyn Fn(IpcPlotImage) + Send + Sync>>,
    pub on_readline_start: Option<Arc<dyn Fn(String) + Send + Sync>>,
    pub on_readline_result: Option<Arc<dyn Fn(IpcEchoEvent) + Send + Sync>>,
    pub on_session_end: Option<Arc<dyn Fn() + Send + Sync>>,
}

#[derive(Clone)]
pub struct ServerIpcConnection {
    writer: OutputCriticalIpcWriter,
    inbox: Arc<Mutex<ServerIpcInbox>>,
    cvar: Arc<Condvar>,
    reader_thread: Arc<Mutex<Option<thread::JoinHandle<()>>>>,
}

#[derive(Clone)]
pub struct WorkerIpcConnection {
    writer: OutputCriticalIpcWriter,
    inbox: Arc<Mutex<WorkerIpcInbox>>,
    cvar: Arc<Condvar>,
}

#[allow(dead_code)]
#[derive(Clone)]
pub struct OutputCriticalIpcWriter {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
}

#[allow(dead_code)]
impl OutputCriticalIpcWriter {
    pub fn new(writer: Box<dyn Write + Send>) -> Self {
        Self {
            writer: Arc::new(Mutex::new(writer)),
        }
    }

    pub fn send<T: Serialize>(&self, message: T) -> io::Result<()> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| io::Error::other("ipc writer mutex poisoned"))?;
        write_ipc_message(&mut **writer, &message)
    }
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
        let inbox = Arc::new(Mutex::new(ServerIpcInbox::default()));
        let cvar = Arc::new(Condvar::new());
        let reader_thread = Arc::new(Mutex::new(None));

        let reader_inbox = inbox.clone();
        let reader_cvar = cvar.clone();
        let output_text_handler = handlers.on_output_text.clone();
        let plot_handler = handlers.on_plot_image.clone();
        let readline_start_handler = handlers.on_readline_start.clone();
        let readline_result_handler = handlers.on_readline_result.clone();
        let session_end_handler = handlers.on_session_end.clone();
        let IpcTransport { reader, writer } = transport;
        let writer = OutputCriticalIpcWriter::new(writer);
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
                let message = match serde_json::from_str::<WorkerToServerIpcMessage>(trimmed) {
                    Ok(message) => message,
                    Err(err) => {
                        let mut guard = reader_inbox.lock().unwrap();
                        guard.protocol_error = Some(format!("invalid worker sideband JSON: {err}"));
                        reader_cvar.notify_all();
                        break;
                    }
                };
                {
                    let mut guard = reader_inbox.lock().unwrap();
                    if !guard.startup_message_seen {
                        let startup_message = matches!(
                            &message,
                            WorkerToServerIpcMessage::BackendInfo { .. }
                                | WorkerToServerIpcMessage::WorkerReady { .. }
                                | WorkerToServerIpcMessage::SessionEnd { .. }
                        );
                        if !startup_message {
                            guard.protocol_error = Some(
                                "first worker sideband message must be worker_ready or backend_info"
                                    .to_string(),
                            );
                            reader_cvar.notify_all();
                            break;
                        }
                        guard.startup_message_seen = true;
                    }
                }
                match message {
                    WorkerToServerIpcMessage::ReadlineStart {
                        prompt,
                        client_waiting,
                    } => {
                        let prompt_for_handler = prompt.clone();
                        let mut guard = reader_inbox.lock().unwrap();
                        let waiting_for_new_input =
                            if let Some(active_stdin) = guard.active_stdin.as_ref() {
                                active_stdin.is_empty()
                            } else {
                                client_waiting
                            };
                        if waiting_for_new_input {
                            guard.readline_unmatched_starts =
                                guard.readline_unmatched_starts.saturating_add(1);
                            if guard.readline_unmatched_starts == 1 {
                                guard.readline_unmatched_since = Some(Instant::now());
                            }
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
                    WorkerToServerIpcMessage::ReadlineInput { text } => {
                        let mut guard = reader_inbox.lock().unwrap();
                        if let Err(err) = account_active_stdin(&mut guard, &text, "readline_input")
                        {
                            guard.protocol_error = Some(err);
                            reader_cvar.notify_all();
                            break;
                        }
                        reader_cvar.notify_all();
                    }
                    WorkerToServerIpcMessage::ReadlineDiscard { text } => {
                        let mut guard = reader_inbox.lock().unwrap();
                        if let Err(err) =
                            account_active_stdin(&mut guard, &text, "readline_discard")
                        {
                            guard.protocol_error = Some(err);
                            reader_cvar.notify_all();
                            break;
                        }
                        reader_cvar.notify_all();
                    }
                    WorkerToServerIpcMessage::ReadlineResult { prompt, line } => {
                        let echo_event = IpcEchoEvent {
                            prompt: prompt.clone(),
                            line: line.clone(),
                            source: OutputTextSource::Ipc,
                        };
                        let mut guard = reader_inbox.lock().unwrap();
                        guard.readline_result_count = guard.readline_result_count.saturating_add(1);
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
                    WorkerToServerIpcMessage::SessionEnd {
                        reason,
                        message_b64,
                    } => {
                        let mut guard = reader_inbox.lock().unwrap();
                        guard.session_end = true;
                        guard.queue.push_back(WorkerToServerIpcMessage::SessionEnd {
                            reason,
                            message_b64,
                        });
                        reader_cvar.notify_all();
                        drop(guard);
                        if let Some(handler) = session_end_handler.as_ref() {
                            handler();
                        }
                    }
                    WorkerToServerIpcMessage::OutputText {
                        stream,
                        data_b64,
                        is_continuation,
                    } => {
                        let bytes =
                            match base64::engine::general_purpose::STANDARD.decode(&data_b64) {
                                Ok(bytes) => bytes,
                                Err(_) => {
                                    let mut guard = reader_inbox.lock().unwrap();
                                    guard.protocol_error =
                                        Some("invalid output_text base64".to_string());
                                    reader_cvar.notify_all();
                                    break;
                                }
                            };
                        if let Some(handler) = output_text_handler.as_ref() {
                            handler(IpcOutputText {
                                stream,
                                bytes,
                                is_continuation,
                            });
                        } else {
                            let mut guard = reader_inbox.lock().unwrap();
                            guard.queue.push_back(WorkerToServerIpcMessage::OutputText {
                                stream,
                                data_b64,
                                is_continuation,
                            });
                            reader_cvar.notify_all();
                        }
                    }
                    WorkerToServerIpcMessage::PlotImage {
                        mime_type,
                        data,
                        is_update,
                        source,
                    } => {
                        let (id, is_new, updates_previous_image, readline_results_seen) = {
                            let mut guard = reader_inbox.lock().unwrap();
                            let (id, is_new, updates_previous_image) =
                                assign_plot_image_id(&mut guard, source.as_deref(), is_update);
                            (
                                id,
                                is_new,
                                updates_previous_image,
                                guard.readline_result_count as usize,
                            )
                        };
                        if let Some(handler) = plot_handler.as_ref() {
                            handler(IpcPlotImage {
                                id,
                                mime_type,
                                data,
                                is_new,
                                updates_previous_image,
                                readline_results_seen,
                            });
                        } else {
                            let mut guard = reader_inbox.lock().unwrap();
                            guard.queue.push_back(WorkerToServerIpcMessage::PlotImage {
                                mime_type,
                                data,
                                is_update,
                                source,
                            });
                            reader_cvar.notify_all();
                        }
                    }
                    WorkerToServerIpcMessage::OutputImage {
                        image_id,
                        mime_type,
                        data_b64,
                        update,
                    } => {
                        if base64::engine::general_purpose::STANDARD
                            .decode(&data_b64)
                            .is_err()
                        {
                            let mut guard = reader_inbox.lock().unwrap();
                            guard.protocol_error = Some("invalid output_image base64".to_string());
                            reader_cvar.notify_all();
                            break;
                        }
                        let (id, is_new, updates_previous_image, readline_results_seen) = {
                            let mut guard = reader_inbox.lock().unwrap();
                            let (id, is_new, updates_previous_image) =
                                assign_plot_image_id(&mut guard, Some(&image_id), update);
                            (
                                id,
                                is_new,
                                updates_previous_image,
                                guard.readline_result_count as usize,
                            )
                        };
                        if let Some(handler) = plot_handler.as_ref() {
                            handler(IpcPlotImage {
                                id,
                                mime_type,
                                data: data_b64,
                                is_new,
                                updates_previous_image,
                                readline_results_seen,
                            });
                        } else {
                            let mut guard = reader_inbox.lock().unwrap();
                            guard
                                .queue
                                .push_back(WorkerToServerIpcMessage::OutputImage {
                                    image_id,
                                    mime_type,
                                    data_b64,
                                    update,
                                });
                            reader_cvar.notify_all();
                        }
                    }
                    other => {
                        let mut guard = reader_inbox.lock().unwrap();
                        guard.queue.push_back(other);
                        reader_cvar.notify_all();
                    }
                }
            }
        });
        *reader_thread.lock().unwrap() = Some(handle);

        Ok(Self {
            writer,
            inbox,
            cvar,
            reader_thread,
        })
    }

    pub fn send(&self, message: ServerToWorkerIpcMessage) -> io::Result<()> {
        self.writer.send(message)
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
        reset_after_completed_request(&mut guard);
        drop_stdin_write_acks(&mut guard);
        guard.echo_events.clear();
        guard.prompt_history.clear();
        guard.protocol_warnings.clear();
        guard.protocol_error = None;
    }

    pub fn begin_request_with_stdin(&self, payload: &[u8]) {
        let mut guard = self.inbox.lock().unwrap();
        reset_after_completed_request(&mut guard);
        drop_stdin_write_acks(&mut guard);
        guard.active_stdin = Some(payload.iter().copied().collect());
        guard.echo_events.clear();
        guard.prompt_history.clear();
        guard.protocol_warnings.clear();
        guard.protocol_error = None;
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

    pub fn wait_for_request_completion(
        &self,
        timeout: Duration,
        stable_wait: Duration,
    ) -> Result<(), IpcWaitError> {
        let deadline = Instant::now() + timeout;
        let allow_completion_settle_after_deadline = !timeout.is_zero();
        let mut guard = self.inbox.lock().unwrap();
        loop {
            if take_session_end(&mut guard) {
                return Err(IpcWaitError::SessionEnd);
            }
            if let Some(message) = guard.protocol_error.take() {
                return Err(IpcWaitError::Protocol(message));
            }
            if take_request_completion(&mut guard, stable_wait) {
                return Ok(());
            }
            if guard.disconnected {
                return Err(IpcWaitError::Disconnected);
            }

            let now = Instant::now();
            if now >= deadline
                && !request_completion_observed_before_deadline(
                    &guard,
                    deadline,
                    allow_completion_settle_after_deadline,
                )
            {
                return Err(IpcWaitError::Timeout);
            }
            let remaining = completion_wait_duration(
                &guard,
                deadline,
                stable_wait,
                allow_completion_settle_after_deadline,
            );
            let (next_guard, timeout_res) = self.cvar.wait_timeout(guard, remaining).unwrap();
            guard = next_guard;
            if timeout_res.timed_out() {
                if take_session_end(&mut guard) {
                    return Err(IpcWaitError::SessionEnd);
                }
                if let Some(message) = guard.protocol_error.take() {
                    return Err(IpcWaitError::Protocol(message));
                }
                if take_request_completion(&mut guard, stable_wait) {
                    return Ok(());
                }
                if Instant::now() >= deadline
                    && !request_completion_observed_before_deadline(
                        &guard,
                        deadline,
                        allow_completion_settle_after_deadline,
                    )
                {
                    return Err(IpcWaitError::Timeout);
                }
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
            if let Some(message) = guard.protocol_error.take() {
                return Err(IpcWaitError::Protocol(message));
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
            if let Some(message) = guard.protocol_error.take() {
                return Err(IpcWaitError::Protocol(message));
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

    pub fn wait_for_stdin_write_ack(&self, timeout: Duration) -> Result<(), IpcWaitError> {
        let deadline = Instant::now() + timeout;
        let mut guard = self.inbox.lock().unwrap();
        loop {
            if take_stdin_write_ack(&mut guard) {
                return Ok(());
            }
            if let Some(message) = guard.protocol_error.take() {
                return Err(IpcWaitError::Protocol(message));
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
                if take_stdin_write_ack(&mut guard) {
                    return Ok(());
                }
                if let Some(message) = guard.protocol_error.take() {
                    return Err(IpcWaitError::Protocol(message));
                }
                return Err(IpcWaitError::Timeout);
            }
        }
    }
}

impl WorkerIpcConnection {
    fn new(transport: IpcTransport) -> io::Result<Self> {
        let inbox = Arc::new(Mutex::new(WorkerIpcInbox::default()));
        let cvar = Arc::new(Condvar::new());

        let reader_inbox = inbox.clone();
        let reader_cvar = cvar.clone();
        let IpcTransport { reader, writer } = transport;
        let writer = OutputCriticalIpcWriter::new(writer);
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
                let message = match serde_json::from_str::<ServerToWorkerIpcMessage>(trimmed) {
                    Ok(message) => message,
                    Err(_) => {
                        let mut guard = reader_inbox.lock().unwrap();
                        guard.disconnected = true;
                        reader_cvar.notify_all();
                        break;
                    }
                };
                let mut guard = reader_inbox.lock().unwrap();
                guard.queue.push_back(message);
                reader_cvar.notify_all();
            }
        });

        Ok(Self {
            writer,
            inbox,
            cvar,
        })
    }

    pub fn send(&self, message: WorkerToServerIpcMessage) -> io::Result<()> {
        self.writer.send(message)
    }

    pub fn send_output_text(&self, stream: TextStream, bytes: &[u8]) -> io::Result<()> {
        for (idx, chunk) in bytes.chunks(OUTPUT_TEXT_IPC_CHUNK_BYTES).enumerate() {
            self.send(WorkerToServerIpcMessage::OutputText {
                stream,
                data_b64: base64::engine::general_purpose::STANDARD.encode(chunk),
                is_continuation: idx > 0,
            })?;
        }
        Ok(())
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

fn write_ipc_message<T: Serialize>(writer: &mut dyn Write, message: &T) -> io::Result<()> {
    let payload = serde_json::to_string(message).map_err(io::Error::other)?;
    writer.write_all(payload.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()
}

#[derive(Debug)]
pub enum IpcWaitError {
    Timeout,
    SessionEnd,
    Disconnected,
    Protocol(String),
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

pub fn worker_ipc_disabled_for_process() -> bool {
    #[cfg(target_family = "unix")]
    {
        !WORKER_IPC_ALLOWED.load(AtomicOrdering::SeqCst)
    }
    #[cfg(not(target_family = "unix"))]
    {
        false
    }
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

pub fn emit_readline_start(prompt: &str, client_waiting: bool) {
    if let Some(ipc) = global_ipc() {
        let _ = ipc.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: prompt.to_string(),
            client_waiting,
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

pub fn emit_output_text(stream: TextStream, bytes: &[u8]) -> io::Result<()> {
    let ipc = global_ipc().ok_or_else(|| io::Error::other("worker IPC is unavailable"))?;
    ipc.send_output_text(stream, bytes)
}

pub fn emit_plot_image(mime_type: &str, data: &str, is_update: bool, source: Option<&str>) {
    if let Some(ipc) = global_ipc() {
        let _ = ipc.send(WorkerToServerIpcMessage::PlotImage {
            mime_type: mime_type.to_string(),
            data: data.to_string(),
            is_update,
            source: source.map(ToString::to_string),
        });
    }
}

pub fn emit_worker_ready(
    worker_name: &str,
    supports_images: bool,
    graceful_shutdown_stdin: Option<&str>,
) {
    if let Some(ipc) = global_ipc() {
        let _ = ipc.send(WorkerToServerIpcMessage::WorkerReady {
            protocol: WorkerProtocol {
                name: "mcp-repl-worker".to_string(),
                version: 1,
            },
            worker: WorkerIdentity {
                name: worker_name.to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
            capabilities: WorkerCapabilities {
                images: supports_images,
            },
            graceful_shutdown: graceful_shutdown_stdin.map(|stdin| WorkerGracefulShutdown {
                stdin: stdin.to_string(),
            }),
        });
    }
}

pub fn emit_stdin_write_ack() {
    if let Some(ipc) = global_ipc() {
        let _ = ipc.send(WorkerToServerIpcMessage::StdinWriteAck);
    }
}

pub fn emit_session_end() {
    if let Some(ipc) = global_ipc() {
        let _ = ipc.send(WorkerToServerIpcMessage::SessionEnd {
            reason: None,
            message_b64: None,
        });
    }
}

#[cfg(test)]
pub(crate) fn test_connection_pair() -> io::Result<(ServerIpcConnection, WorkerIpcConnection)> {
    test_connection_pair_with_handlers(IpcHandlers::default())
}

#[cfg(test)]
pub(crate) fn test_connection_pair_with_handlers(
    handlers: IpcHandlers,
) -> io::Result<(ServerIpcConnection, WorkerIpcConnection)> {
    let (server_read, worker_write) = std::io::pipe()?;
    let (worker_read, server_write) = std::io::pipe()?;
    let server = ServerIpcConnection::new(
        IpcTransport {
            reader: Box::new(server_read),
            writer: Box::new(server_write),
        },
        handlers,
    )?;
    let worker = WorkerIpcConnection::new(IpcTransport {
        reader: Box::new(worker_read),
        writer: Box::new(worker_write),
    })?;
    server.inbox.lock().unwrap().startup_message_seen = true;
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
        .position(|msg| matches!(msg, WorkerToServerIpcMessage::SessionEnd { .. }))
    {
        guard.queue.remove(idx);
    }
    true
}

fn account_active_stdin(
    guard: &mut ServerIpcInbox,
    text: &str,
    event_type: &str,
) -> Result<(), String> {
    let Some(active_stdin) = guard.active_stdin.as_mut() else {
        if text.is_empty() {
            return Ok(());
        }
        return Err(format!("{event_type} reported input with no active turn"));
    };
    let bytes = text.as_bytes();
    if bytes.len() > active_stdin.len() {
        return Err(format!(
            "{event_type} reported {} bytes but only {} active stdin bytes remain",
            bytes.len(),
            active_stdin.len()
        ));
    }
    for (idx, expected) in bytes.iter().enumerate() {
        if active_stdin.get(idx) != Some(expected) {
            return Err(format!(
                "{event_type} text does not match active stdin at byte {idx}"
            ));
        }
    }
    for _ in bytes {
        active_stdin.pop_front();
    }
    Ok(())
}

fn take_stdin_write_ack(guard: &mut ServerIpcInbox) -> bool {
    if let Some(idx) = guard
        .queue
        .iter()
        .position(|msg| matches!(msg, WorkerToServerIpcMessage::StdinWriteAck))
    {
        guard.queue.remove(idx);
        true
    } else {
        false
    }
}

fn drop_stdin_write_acks(guard: &mut ServerIpcInbox) {
    guard
        .queue
        .retain(|msg| !matches!(msg, WorkerToServerIpcMessage::StdinWriteAck));
}

fn request_completion_ready(guard: &ServerIpcInbox, stable_wait: Duration) -> bool {
    let Some(since) = guard.readline_unmatched_since else {
        return false;
    };
    guard.readline_unmatched_starts > 0 && since.elapsed() >= stable_wait
}

fn take_request_completion(guard: &mut ServerIpcInbox, stable_wait: Duration) -> bool {
    if !request_completion_ready(guard, stable_wait) {
        return false;
    }
    reset_request_progress(guard);
    true
}

fn request_completion_observed_before_deadline(
    guard: &ServerIpcInbox,
    deadline: Instant,
    allow_completion_settle_after_deadline: bool,
) -> bool {
    allow_completion_settle_after_deadline
        && guard.readline_unmatched_starts > 0
        && guard
            .readline_unmatched_since
            .is_some_and(|since| since <= deadline)
}

fn completion_wait_duration(
    guard: &ServerIpcInbox,
    deadline: Instant,
    stable_wait: Duration,
    allow_completion_settle_after_deadline: bool,
) -> Duration {
    let now = Instant::now();
    let until_deadline = deadline.saturating_duration_since(now);
    let Some(since) = guard.readline_unmatched_since else {
        return until_deadline;
    };
    let elapsed = since.elapsed();
    if elapsed >= stable_wait {
        Duration::from_millis(0)
    } else if allow_completion_settle_after_deadline && since <= deadline {
        stable_wait.saturating_sub(elapsed)
    } else {
        until_deadline.min(stable_wait.saturating_sub(elapsed))
    }
}

fn allocate_plot_image_id() -> String {
    let id = NEXT_SERVER_IMAGE_ID
        .fetch_add(1, AtomicOrdering::Relaxed)
        .saturating_add(1);
    format!("image-{id}")
}

fn assign_plot_image_id(
    guard: &mut ServerIpcInbox,
    source: Option<&str>,
    is_update: bool,
) -> (String, bool, bool) {
    if let Some(source) = source {
        if is_update && let Some(id) = guard.request_plot_source_image_ids.get(source) {
            guard.current_image_id = Some(id.clone());
            return (id.clone(), false, false);
        }

        let updates_previous_image = is_update && guard.plot_source_image_ids.contains_key(source);
        let id = allocate_plot_image_id();
        guard
            .plot_source_image_ids
            .insert(source.to_string(), id.clone());
        guard
            .request_plot_source_image_ids
            .insert(source.to_string(), id.clone());
        guard.current_image_id = Some(id.clone());
        return (id, true, updates_previous_image);
    }

    let updates_previous_image =
        is_update && guard.current_image_id.is_some() && guard.request_image_id.is_none();
    let is_new = !is_update || guard.current_image_id.is_none() || updates_previous_image;
    let id = if is_new {
        let id = allocate_plot_image_id();
        guard.request_image_id = Some(id.clone());
        guard.current_image_id = Some(id.clone());
        id
    } else {
        guard
            .request_image_id
            .clone()
            .or_else(|| guard.current_image_id.clone())
            .expect("current image id must exist for updates")
    };
    (id, is_new, updates_previous_image)
}

fn reset_request_progress(guard: &mut ServerIpcInbox) {
    guard.active_stdin = None;
    guard.readline_result_count = 0;
    guard.readline_unmatched_starts = 0;
    guard.readline_unmatched_since = None;
}

fn reset_after_completed_request(guard: &mut ServerIpcInbox) {
    reset_request_progress(guard);
    guard.request_image_id = None;
    guard.request_plot_source_image_ids.clear();
    guard.last_prompt = None;
}

fn take_backend_info(guard: &mut ServerIpcInbox) -> Option<WorkerToServerIpcMessage> {
    let idx = guard.queue.iter().position(|msg| {
        matches!(
            msg,
            WorkerToServerIpcMessage::BackendInfo { .. }
                | WorkerToServerIpcMessage::WorkerReady { .. }
        )
    })?;
    guard.queue.remove(idx)
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[cfg(test)]
mod protocol_tests {
    use super::{
        IpcHandlers, IpcTransport, IpcWaitError, OUTPUT_TEXT_IPC_CHUNK_BYTES,
        OutputCriticalIpcWriter, ServerIpcConnection, ServerToWorkerIpcMessage,
        WorkerToServerIpcMessage, test_connection_pair_with_handlers,
    };
    use crate::worker_protocol::TextStream;
    use base64::Engine as _;
    use serde_json::json;
    use std::io::{BufRead, Write};
    use std::sync::{Arc, Mutex, mpsc};
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn backend_info_protocol_does_not_include_language() {
        let parsed = serde_json::from_value::<WorkerToServerIpcMessage>(json!({
            "type": "backend_info",
            "supports_images": true
        }));

        assert!(parsed.is_ok(), "backend_info should not require language");
    }

    #[test]
    fn backend_info_protocol_rejects_language() {
        let parsed = serde_json::from_value::<WorkerToServerIpcMessage>(json!({
            "type": "backend_info",
            "language": "r",
            "supports_images": true
        }));

        assert!(parsed.is_err(), "backend_info should reject language");
    }

    #[test]
    fn plot_image_protocol_uses_update_flag_without_worker_id() {
        let parsed = serde_json::from_value::<WorkerToServerIpcMessage>(json!({
            "type": "plot_image",
            "mime_type": "image/png",
            "data": "abc",
            "is_update": true
        }));

        assert!(
            parsed.is_ok(),
            "plot_image should not require worker image id"
        );
    }

    #[test]
    fn plot_image_protocol_rejects_worker_id_and_is_new() {
        let parsed = serde_json::from_value::<WorkerToServerIpcMessage>(json!({
            "type": "plot_image",
            "id": "plot-1",
            "mime_type": "image/png",
            "data": "abc",
            "is_new": true,
            "is_update": false
        }));

        assert!(
            parsed.is_err(),
            "plot_image should reject old worker-owned image fields"
        );
    }

    #[test]
    fn output_text_protocol_uses_stream_and_base64_payload() {
        let parsed = serde_json::from_value::<WorkerToServerIpcMessage>(json!({
            "type": "output_text",
            "stream": "stdout",
            "data_b64": "YWxwaGE="
        }));

        let Ok(WorkerToServerIpcMessage::OutputText {
            stream,
            data_b64,
            is_continuation,
        }) = parsed
        else {
            panic!("output_text should deserialize");
        };
        assert_eq!(stream, TextStream::Stdout);
        assert_eq!(data_b64, "YWxwaGE=");
        assert!(
            !is_continuation,
            "output_text continuation should default to false"
        );
    }

    #[test]
    fn output_text_protocol_rejects_plain_data_payload() {
        let parsed = serde_json::from_value::<WorkerToServerIpcMessage>(json!({
            "type": "output_text",
            "stream": "stdout",
            "data": "alpha"
        }));

        assert!(parsed.is_err(), "output_text should require data_b64");
    }

    #[test]
    fn plot_image_protocol_rejects_sequence_ack_handshake() {
        let worker_to_server = serde_json::from_value::<WorkerToServerIpcMessage>(json!({
            "type": "plot_image",
            "mime_type": "image/png",
            "data": "abc",
            "is_update": false,
            "sequence": 1
        }));
        assert!(
            worker_to_server.is_err(),
            "plot_image should not expose worker-side ack sequencing"
        );

        let server_to_worker = serde_json::from_value::<ServerToWorkerIpcMessage>(json!({
            "type": "plot_image_ack",
            "sequence": 1
        }));
        assert!(
            server_to_worker.is_err(),
            "server-to-worker protocol should not include plot_image_ack"
        );
    }

    #[test]
    fn request_end_is_not_part_of_worker_to_server_protocol() {
        let parsed = serde_json::from_value::<WorkerToServerIpcMessage>(json!({
            "type": "request_end"
        }));

        assert!(parsed.is_err(), "request_end should not deserialize");
    }

    #[test]
    fn stdin_write_ack_is_worker_to_server_only() {
        let parsed = serde_json::from_value::<WorkerToServerIpcMessage>(json!({
            "type": "stdin_write_ack"
        }));
        assert!(
            matches!(parsed, Ok(WorkerToServerIpcMessage::StdinWriteAck)),
            "stdin_write_ack should deserialize as the worker-side stdin acceptance signal"
        );

        let parsed = serde_json::from_value::<ServerToWorkerIpcMessage>(json!({
            "type": "stdin_write_ack"
        }));
        assert!(
            parsed.is_err(),
            "stdin_write_ack should not deserialize as a server-to-worker message"
        );
    }

    #[test]
    fn begin_request_drops_stale_stdin_write_acks() {
        let (server, worker) =
            test_connection_pair_with_handlers(IpcHandlers::default()).expect("ipc pair");
        worker
            .send(WorkerToServerIpcMessage::StdinWriteAck)
            .expect("send stale ack");

        let deadline = Instant::now() + Duration::from_secs(1);
        let mut guard = server.inbox.lock().unwrap();
        while !guard
            .queue
            .iter()
            .any(|msg| matches!(msg, WorkerToServerIpcMessage::StdinWriteAck))
        {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "expected stale stdin_write_ack to reach server inbox"
            );
            let (next_guard, timeout_res) = server.cvar.wait_timeout(guard, remaining).unwrap();
            guard = next_guard;
            assert!(
                !timeout_res.timed_out(),
                "expected stale stdin_write_ack to reach server inbox"
            );
        }
        drop(guard);

        server.begin_request();
        assert!(
            matches!(
                server.wait_for_stdin_write_ack(Duration::ZERO),
                Err(IpcWaitError::Timeout)
            ),
            "begin_request should discard stale stdin_write_ack messages"
        );

        worker
            .send(WorkerToServerIpcMessage::StdinWriteAck)
            .expect("send fresh ack");
        server
            .wait_for_stdin_write_ack(Duration::from_secs(1))
            .expect("fresh ack should still be accepted");
    }

    #[test]
    fn invalid_worker_message_disconnects_server_ipc() {
        let (server_read, mut worker_write) = std::io::pipe().expect("server pipe");
        let (_worker_read, server_write) = std::io::pipe().expect("worker pipe");
        let server = ServerIpcConnection::new(
            IpcTransport {
                reader: Box::new(server_read),
                writer: Box::new(server_write),
            },
            IpcHandlers::default(),
        )
        .expect("server connection");

        writeln!(
            worker_write,
            "{}",
            json!({
                "type": "backend_info",
                "language": "r",
                "supports_images": true
            })
        )
        .expect("invalid worker message");

        let result = server.wait_for_backend_info(Duration::from_millis(200));

        assert!(
            matches!(result, Err(super::IpcWaitError::Protocol(ref message)) if message.starts_with("invalid worker sideband JSON:")),
            "invalid worker message should report a protocol error, got: {result:?}"
        );
    }

    #[test]
    fn output_critical_writer_flushes_before_returning() {
        let (server_read, worker_write) = std::io::pipe().expect("server pipe");
        let writer = OutputCriticalIpcWriter::new(Box::new(worker_write));
        let (line_tx, line_rx) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = std::io::BufReader::new(server_read);
            let mut line = String::new();
            reader.read_line(&mut line).expect("read IPC line");
            line_tx.send(line).expect("send IPC line");
        });

        writer
            .send(WorkerToServerIpcMessage::OutputText {
                stream: TextStream::Stdout,
                data_b64: base64::engine::general_purpose::STANDARD.encode(b"alpha"),
                is_continuation: false,
            })
            .expect("send output_text");

        let line = line_rx
            .recv_timeout(Duration::from_millis(200))
            .expect("IPC line flushed before send returned");
        assert!(line.contains(r#""type":"output_text""#), "{line}");
        assert!(line.contains(r#""data_b64":"YWxwaGE=""#), "{line}");
    }

    #[test]
    fn output_critical_writer_serializes_shared_writes() {
        let (server_read, worker_write) = std::io::pipe().expect("server pipe");
        let writer = OutputCriticalIpcWriter::new(Box::new(worker_write));
        let second_writer = writer.clone();
        let (line_tx, line_rx) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = std::io::BufReader::new(server_read);
            for _ in 0..2 {
                let mut line = String::new();
                reader.read_line(&mut line).expect("read IPC line");
                line_tx.send(line).expect("send IPC line");
            }
        });

        writer
            .send(WorkerToServerIpcMessage::OutputText {
                stream: TextStream::Stdout,
                data_b64: base64::engine::general_purpose::STANDARD.encode(b"first"),
                is_continuation: false,
            })
            .expect("send first output_text");
        second_writer
            .send(WorkerToServerIpcMessage::OutputText {
                stream: TextStream::Stderr,
                data_b64: base64::engine::general_purpose::STANDARD.encode(b"second"),
                is_continuation: false,
            })
            .expect("send second output_text");

        let first = line_rx
            .recv_timeout(Duration::from_millis(200))
            .expect("first IPC line");
        let second = line_rx
            .recv_timeout(Duration::from_millis(200))
            .expect("second IPC line");
        assert!(first.contains(r#""data_b64":"Zmlyc3Q=""#), "{first}");
        assert!(second.contains(r#""data_b64":"c2Vjb25k""#), "{second}");
    }

    #[test]
    fn synchronous_worker_output_text_reports_broken_pipe() {
        let (server_read, worker_write) = std::io::pipe().expect("server pipe");
        drop(server_read);
        let writer = OutputCriticalIpcWriter::new(Box::new(worker_write));

        let result = writer.send(WorkerToServerIpcMessage::OutputText {
            stream: TextStream::Stdout,
            data_b64: base64::engine::general_purpose::STANDARD.encode(b"lost"),
            is_continuation: false,
        });

        assert!(result.is_err(), "broken IPC pipe should be reported");
    }

    #[test]
    fn worker_output_text_chunks_large_buffers_before_encoding() {
        let (chunk_tx, chunk_rx) = mpsc::channel();
        let (_server, worker) = test_connection_pair_with_handlers(IpcHandlers {
            on_output_text: Some(Arc::new(move |text| {
                chunk_tx
                    .send((text.bytes, text.is_continuation))
                    .expect("record output text chunk");
            })),
            ..IpcHandlers::default()
        })
        .expect("test IPC pair");
        let payload = vec![b'x'; OUTPUT_TEXT_IPC_CHUNK_BYTES * 2 + 17];

        worker
            .send_output_text(TextStream::Stdout, &payload)
            .expect("send chunked output_text");

        let mut chunks = Vec::new();
        let mut bytes_seen = 0usize;
        while bytes_seen < payload.len() {
            let (chunk, is_continuation) = chunk_rx
                .recv_timeout(Duration::from_millis(200))
                .expect("output_text chunk");
            bytes_seen += chunk.len();
            chunks.push((chunk, is_continuation));
        }
        assert!(
            chunks.len() > 1,
            "large output_text buffer should be split before IPC framing"
        );
        assert!(
            chunks
                .iter()
                .all(|(chunk, _)| chunk.len() <= OUTPUT_TEXT_IPC_CHUNK_BYTES),
            "output_text chunks should be bounded"
        );
        assert!(
            !chunks.first().expect("first chunk").1
                && chunks
                    .iter()
                    .skip(1)
                    .all(|(_, is_continuation)| *is_continuation),
            "only chunks after the first should be marked as continuations"
        );
        let reassembled = chunks
            .iter()
            .flat_map(|(chunk, _)| chunk.iter().copied())
            .collect::<Vec<_>>();
        assert_eq!(reassembled, payload);
    }

    #[test]
    fn plot_image_updates_reuse_current_server_image_id() {
        let images = Arc::new(Mutex::new(Vec::new()));
        let handler_images = images.clone();
        let (_server, worker) = test_connection_pair_with_handlers(IpcHandlers {
            on_plot_image: Some(Arc::new(move |image| {
                handler_images.lock().expect("image mutex").push(image);
            })),
            ..IpcHandlers::default()
        })
        .expect("ipc pair");
        let first = json!({
            "type": "plot_image",
            "mime_type": "image/png",
            "data": "first",
            "is_update": false
        })
        .to_string();
        let second = json!({
            "type": "plot_image",
            "mime_type": "image/png",
            "data": "second",
            "is_update": true
        })
        .to_string();

        worker
            .send(serde_json::from_str(&first).expect("first image message"))
            .expect("send first image");
        worker
            .send(serde_json::from_str(&second).expect("second image message"))
            .expect("send second image");

        let deadline = Instant::now() + Duration::from_millis(200);
        while Instant::now() < deadline {
            if images.lock().expect("image mutex").len() >= 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let images = images.lock().expect("image mutex");
        assert_eq!(images.len(), 2);
        assert_eq!(images[0].id, images[1].id);
        assert!(images[0].is_new);
        assert!(!images[1].is_new);
        assert_eq!(images[0].data, "first");
        assert_eq!(images[1].data, "second");
    }

    #[test]
    fn plot_image_ids_do_not_repeat_across_server_connections() {
        fn next_connection_image_id() -> String {
            let images = Arc::new(Mutex::new(Vec::new()));
            let handler_images = images.clone();
            let (_server, worker) = test_connection_pair_with_handlers(IpcHandlers {
                on_plot_image: Some(Arc::new(move |image| {
                    handler_images.lock().expect("image mutex").push(image);
                })),
                ..IpcHandlers::default()
            })
            .expect("ipc pair");
            let image = json!({
                "type": "plot_image",
                "mime_type": "image/png",
                "data": "image",
                "is_update": false
            })
            .to_string();

            worker
                .send(serde_json::from_str(&image).expect("image message"))
                .expect("send image");

            let deadline = Instant::now() + Duration::from_millis(200);
            while Instant::now() < deadline {
                if let Some(image) = images.lock().expect("image mutex").first() {
                    return image.id.clone();
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            panic!("timed out waiting for image");
        }

        let first = next_connection_image_id();
        let second = next_connection_image_id();

        assert_ne!(
            first, second,
            "server-generated image IDs must stay unique across worker IPC connections"
        );
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
