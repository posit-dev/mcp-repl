use std::collections::{HashMap, VecDeque};
use std::io::{self, BufRead, BufReader};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine as _;

use crate::input_state::InputState;
use crate::output_capture::OutputTextSource;

use super::protocol::{
    IpcEchoEvent, IpcHandlers, IpcOutputImage, IpcOutputText, ServerToWorkerIpcMessage,
    WorkerToServerIpcMessage,
};
use super::transport::IpcTransport;
use super::worker_connection::OutputCriticalIpcWriter;

const MAX_PROMPT_HISTORY: usize = 16;
static NEXT_SERVER_IMAGE_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
struct ServerIpcInbox {
    queue: VecDeque<WorkerToServerIpcMessage>,
    startup_message_seen: bool,
    last_prompt: Option<String>,
    last_prompt_observed_at: Option<Instant>,
    prompt_history: VecDeque<String>,
    echo_events: VecDeque<IpcEchoEvent>,
    input_state: InputState,
    readline_result_count: usize,
    current_image_id: Option<String>,
    request_image_id: Option<String>,
    output_source_image_ids: HashMap<String, String>,
    request_output_source_image_ids: HashMap<String, String>,
    protocol_warnings: VecDeque<String>,
    disconnected: bool,
}

#[derive(Clone)]
pub struct ServerIpcConnection {
    writer: OutputCriticalIpcWriter,
    inbox: Arc<Mutex<ServerIpcInbox>>,
    cvar: Arc<Condvar>,
    reader_thread: Arc<Mutex<Option<thread::JoinHandle<()>>>>,
}

#[derive(Debug)]
pub enum IpcInputReadiness {
    InputWait(String),
    Ready,
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
    pub(crate) fn new(transport: IpcTransport, handlers: IpcHandlers) -> io::Result<Self> {
        let inbox = Arc::new(Mutex::new(ServerIpcInbox::default()));
        let cvar = Arc::new(Condvar::new());
        let reader_thread = Arc::new(Mutex::new(None));

        let reader_inbox = inbox.clone();
        let reader_cvar = cvar.clone();
        let output_text_handler = handlers.on_output_text.clone();
        let output_image_handler = handlers.on_output_image.clone();
        let input_wait_handler = handlers.on_input_wait.clone();
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
                        guard
                            .input_state
                            .latch_protocol_error(format!("invalid worker sideband JSON: {err}"));
                        reader_cvar.notify_all();
                        break;
                    }
                };
                {
                    let mut guard = reader_inbox.lock().unwrap();
                    if guard.input_state.session_end_final() {
                        guard
                            .input_state
                            .latch_protocol_error("worker sideband message after session_end");
                        reader_cvar.notify_all();
                        break;
                    }
                    if !guard.startup_message_seen {
                        let startup_message = matches!(
                            &message,
                            WorkerToServerIpcMessage::WorkerReady { .. }
                                | WorkerToServerIpcMessage::SessionEnd { .. }
                        );
                        if !startup_message {
                            guard.input_state.latch_protocol_error(
                                "first worker sideband message must be worker_ready",
                            );
                            reader_cvar.notify_all();
                            break;
                        }
                        guard.startup_message_seen = true;
                    }
                }
                match message {
                    WorkerToServerIpcMessage::InputLine { prompt, text } => {
                        let echo_event = IpcEchoEvent {
                            prompt: prompt.clone(),
                            line: text.clone(),
                            source: OutputTextSource::Ipc,
                        };
                        let mut guard = reader_inbox.lock().unwrap();
                        if let Err(err) = guard.input_state.validate_active_input("input_line") {
                            guard.input_state.latch_protocol_error(err);
                            reader_cvar.notify_all();
                            break;
                        }
                        guard.readline_result_count = guard.readline_result_count.saturating_add(1);
                        push_prompt_history(&mut guard, prompt);
                        guard.echo_events.push_back(echo_event.clone());
                        reader_cvar.notify_all();
                        drop(guard);
                        if let Some(handler) = readline_result_handler.as_ref() {
                            handler(echo_event);
                        }
                    }
                    WorkerToServerIpcMessage::InputWait { prompt } => {
                        let mut guard = reader_inbox.lock().unwrap();
                        let observed_at = Instant::now();
                        guard.input_state.record_input_wait(observed_at);
                        guard.last_prompt_observed_at = Some(observed_at);
                        push_prompt_history(&mut guard, prompt.clone());
                        guard.last_prompt = Some(prompt.clone());
                        reader_cvar.notify_all();
                        drop(guard);
                        if let Some(handler) = input_wait_handler.as_ref() {
                            handler(prompt);
                        }
                    }
                    WorkerToServerIpcMessage::Ready {} => {
                        let mut guard = reader_inbox.lock().unwrap();
                        guard.input_state.record_ready(Instant::now());
                        guard.last_prompt = None;
                        guard.last_prompt_observed_at = None;
                        guard.prompt_history.clear();
                        reader_cvar.notify_all();
                    }
                    WorkerToServerIpcMessage::SessionEnd { reason, message } => {
                        if let Err(err) = validate_session_end(reason.as_deref()) {
                            let mut guard = reader_inbox.lock().unwrap();
                            guard.input_state.latch_protocol_error(err);
                            reader_cvar.notify_all();
                            break;
                        }
                        let mut guard = reader_inbox.lock().unwrap();
                        guard.input_state.note_session_end();
                        guard
                            .queue
                            .push_back(WorkerToServerIpcMessage::SessionEnd { reason, message });
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
                                    guard
                                        .input_state
                                        .latch_protocol_error("invalid output_text base64");
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
                    WorkerToServerIpcMessage::OutputImage {
                        mime_type,
                        data_b64,
                        is_update,
                        source,
                    } => {
                        if base64::engine::general_purpose::STANDARD
                            .decode(&data_b64)
                            .is_err()
                        {
                            let mut guard = reader_inbox.lock().unwrap();
                            guard
                                .input_state
                                .latch_protocol_error("invalid output_image base64");
                            reader_cvar.notify_all();
                            break;
                        }
                        let (id, is_new, updates_previous_image, readline_results_seen) = {
                            let mut guard = reader_inbox.lock().unwrap();
                            let (id, is_new, updates_previous_image) =
                                assign_output_image_id(&mut guard, source.as_deref(), is_update);
                            (
                                id,
                                is_new,
                                updates_previous_image,
                                guard.readline_result_count,
                            )
                        };
                        if let Some(handler) = output_image_handler.as_ref() {
                            handler(IpcOutputImage {
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
                                    mime_type,
                                    data_b64,
                                    is_update,
                                    source,
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

    pub fn send_with_timeout(
        &self,
        message: ServerToWorkerIpcMessage,
        timeout: Duration,
    ) -> io::Result<()> {
        self.writer.send_with_timeout(message, timeout)
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

    pub fn detach_reader_thread(&self) {
        let _ = self.reader_thread.lock().unwrap().take();
    }

    #[cfg_attr(
        any(target_family = "unix", target_family = "windows"),
        allow(dead_code)
    )]
    pub fn begin_request(&self) {
        let mut guard = self.inbox.lock().unwrap();
        reset_after_completed_request(&mut guard);
        guard.echo_events.clear();
        guard.prompt_history.clear();
        guard.protocol_warnings.clear();
    }

    #[cfg(test)]
    pub fn begin_input(&self) -> Result<(), String> {
        let mut guard = self.inbox.lock().unwrap();
        reset_after_completed_request(&mut guard);
        guard.input_state.begin_input()?;
        guard.echo_events.clear();
        guard.prompt_history.clear();
        guard.protocol_warnings.clear();
        Ok(())
    }

    pub fn begin_input_when_ready(&self, timeout: Duration) -> Result<(), IpcWaitError> {
        let deadline = Instant::now() + timeout;
        let mut guard = self.inbox.lock().unwrap();
        reset_after_completed_request(&mut guard);
        loop {
            if let Some(message) = guard.input_state.take_protocol_error() {
                return Err(IpcWaitError::Protocol(message));
            }
            if take_session_end(&mut guard) {
                return Err(IpcWaitError::SessionEnd);
            }
            if guard.disconnected {
                return Err(IpcWaitError::Disconnected);
            }
            if guard.input_state.ready_for_input() {
                guard.last_prompt = None;
                guard.last_prompt_observed_at = None;
                guard
                    .input_state
                    .begin_input()
                    .map_err(IpcWaitError::Protocol)?;
                guard.echo_events.clear();
                guard.prompt_history.clear();
                guard.protocol_warnings.clear();
                return Ok(());
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

    pub fn note_interrupt_sent(&self) {
        let mut guard = self.inbox.lock().unwrap();
        guard.input_state.note_interrupt_sent();
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

    pub fn take_protocol_error(&self) -> Option<String> {
        let mut guard = self.inbox.lock().unwrap();
        guard.input_state.take_protocol_error()
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
            if take_request_completion_before_latched_protocol_error(&mut guard, stable_wait) {
                return Ok(());
            }
            if let Some(message) = guard.input_state.take_protocol_error() {
                return Err(IpcWaitError::Protocol(message));
            }
            if take_session_end(&mut guard) {
                return Err(IpcWaitError::SessionEnd);
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
                if take_request_completion_before_latched_protocol_error(&mut guard, stable_wait) {
                    return Ok(());
                }
                if let Some(message) = guard.input_state.take_protocol_error() {
                    return Err(IpcWaitError::Protocol(message));
                }
                if take_session_end(&mut guard) {
                    return Err(IpcWaitError::SessionEnd);
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

    #[cfg(test)]
    pub fn wait_for_input_wait(&self, timeout: Duration) -> Result<String, IpcWaitError> {
        let deadline = Instant::now() + timeout;
        let mut guard = self.inbox.lock().unwrap();
        loop {
            if let Some(message) = guard.input_state.take_protocol_error() {
                return Err(IpcWaitError::Protocol(message));
            }
            if take_session_end(&mut guard) {
                return Err(IpcWaitError::SessionEnd);
            }
            if guard.disconnected {
                return Err(IpcWaitError::Disconnected);
            }
            if let Some(prompt) = guard.last_prompt.take() {
                guard.last_prompt_observed_at = None;
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
        guard.last_prompt_observed_at = None;
        guard.last_prompt.take()
    }

    pub fn wait_for_input_readiness(
        &self,
        timeout: Duration,
    ) -> Result<IpcInputReadiness, IpcWaitError> {
        self.wait_for_input_readiness_after(timeout, None, true)
    }

    #[cfg(test)]
    pub fn wait_for_fresh_input_readiness(
        &self,
        timeout: Duration,
        since: Instant,
    ) -> Result<IpcInputReadiness, IpcWaitError> {
        self.wait_for_input_readiness_after(timeout, Some(since), false)
    }

    pub fn wait_for_input_wait_or_fresh_ready(
        &self,
        timeout: Duration,
        since: Instant,
    ) -> Result<IpcInputReadiness, IpcWaitError> {
        self.wait_for_input_readiness_after(timeout, Some(since), true)
    }

    fn wait_for_input_readiness_after(
        &self,
        timeout: Duration,
        since: Option<Instant>,
        accept_stale_prompt: bool,
    ) -> Result<IpcInputReadiness, IpcWaitError> {
        let deadline = Instant::now() + timeout;
        let mut guard = self.inbox.lock().unwrap();
        loop {
            if let Some(message) = guard.input_state.take_protocol_error() {
                return Err(IpcWaitError::Protocol(message));
            }
            if take_session_end(&mut guard) {
                return Err(IpcWaitError::SessionEnd);
            }
            if guard.disconnected {
                return Err(IpcWaitError::Disconnected);
            }
            if let Some(prompt) = guard.last_prompt.as_ref() {
                let prompt_is_fresh = match since {
                    Some(since) => guard
                        .last_prompt_observed_at
                        .is_some_and(|observed_at| observed_at > since),
                    None => true,
                };
                if prompt_is_fresh || accept_stale_prompt {
                    let prompt = prompt.clone();
                    guard.last_prompt = None;
                    guard.last_prompt_observed_at = None;
                    return Ok(IpcInputReadiness::InputWait(prompt));
                }
                guard.last_prompt = None;
                guard.last_prompt_observed_at = None;
            }
            let ready = match since {
                Some(since) => {
                    guard.input_state.readiness_observed_after(since)
                        || (accept_stale_prompt
                            && guard.input_state.input_wait_readiness_available())
                }
                None => guard.input_state.ready_for_input(),
            };
            if ready {
                return Ok(IpcInputReadiness::Ready);
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

    pub fn wait_for_worker_ready(
        &self,
        timeout: Duration,
    ) -> Result<WorkerToServerIpcMessage, IpcWaitError> {
        let deadline = Instant::now() + timeout;
        let mut guard = self.inbox.lock().unwrap();
        loop {
            if let Some(info) = take_worker_ready(&mut guard) {
                let _ = take_session_end(&mut guard);
                return Ok(info);
            }
            if let Some(message) = guard.input_state.take_protocol_error() {
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
}

#[derive(Debug)]
pub enum IpcWaitError {
    Timeout,
    SessionEnd,
    Disconnected,
    Protocol(String),
}

fn take_session_end(guard: &mut ServerIpcInbox) -> bool {
    if !guard.input_state.take_session_end() {
        return false;
    }
    if let Some(idx) = guard
        .queue
        .iter()
        .position(|msg| matches!(msg, WorkerToServerIpcMessage::SessionEnd { .. }))
    {
        guard.queue.remove(idx);
    }
    true
}

fn push_prompt_history(guard: &mut ServerIpcInbox, prompt: String) {
    if guard
        .prompt_history
        .back()
        .is_none_or(|last| last != &prompt)
    {
        guard.prompt_history.push_back(prompt);
        if guard.prompt_history.len() > MAX_PROMPT_HISTORY {
            guard.prompt_history.pop_front();
        }
    }
}

fn request_completion_ready(guard: &ServerIpcInbox, stable_wait: Duration) -> bool {
    let _ = stable_wait;
    guard.input_state.has_active_input() && guard.input_state.request_completion_ready()
}

fn validate_session_end(reason: Option<&str>) -> Result<(), String> {
    if let Some(reason) = reason {
        match reason {
            "shutdown" | "reset" | "runtime_exit" | "crash" | "protocol_error" => {}
            other => return Err(format!("invalid session_end reason: {other}")),
        }
    }
    Ok(())
}

fn take_request_completion_before_latched_protocol_error(
    guard: &mut ServerIpcInbox,
    stable_wait: Duration,
) -> bool {
    if !request_completion_precedes_latched_protocol_error(guard, stable_wait) {
        return false;
    }
    reset_request_progress(guard);
    true
}

fn request_completion_precedes_latched_protocol_error(
    guard: &ServerIpcInbox,
    stable_wait: Duration,
) -> bool {
    let _ = stable_wait;
    guard.input_state.has_active_input()
        && guard
            .input_state
            .request_completion_precedes_protocol_error()
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
    let _ = allow_completion_settle_after_deadline;
    guard.input_state.has_active_input()
        && guard
            .input_state
            .request_completion_observed_before(deadline)
}

fn completion_wait_duration(
    guard: &ServerIpcInbox,
    deadline: Instant,
    stable_wait: Duration,
    allow_completion_settle_after_deadline: bool,
) -> Duration {
    let now = Instant::now();
    let until_deadline = deadline.saturating_duration_since(now);
    let _ = (guard, stable_wait, allow_completion_settle_after_deadline);
    until_deadline
}

fn allocate_output_image_id() -> String {
    let id = NEXT_SERVER_IMAGE_ID
        .fetch_add(1, AtomicOrdering::Relaxed)
        .saturating_add(1);
    format!("image-{id}")
}

fn assign_output_image_id(
    guard: &mut ServerIpcInbox,
    source: Option<&str>,
    is_update: bool,
) -> (String, bool, bool) {
    if let Some(source) = source {
        if is_update && let Some(id) = guard.request_output_source_image_ids.get(source) {
            guard.current_image_id = Some(id.clone());
            return (id.clone(), false, false);
        }

        let updates_previous_image =
            is_update && guard.output_source_image_ids.contains_key(source);
        let id = allocate_output_image_id();
        guard
            .output_source_image_ids
            .insert(source.to_string(), id.clone());
        guard
            .request_output_source_image_ids
            .insert(source.to_string(), id.clone());
        guard.current_image_id = Some(id.clone());
        return (id, true, updates_previous_image);
    }

    let updates_previous_image =
        is_update && guard.current_image_id.is_some() && guard.request_image_id.is_none();
    let is_new = !is_update || guard.current_image_id.is_none() || updates_previous_image;
    let id = if is_new {
        let id = allocate_output_image_id();
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
    guard.input_state.clear_request_progress();
    guard.readline_result_count = 0;
}

fn reset_after_completed_request(guard: &mut ServerIpcInbox) {
    reset_request_progress(guard);
    guard.request_image_id = None;
    guard.request_output_source_image_ids.clear();
    guard.last_prompt = None;
    guard.last_prompt_observed_at = None;
}

fn take_worker_ready(guard: &mut ServerIpcInbox) -> Option<WorkerToServerIpcMessage> {
    let idx = guard
        .queue
        .iter()
        .position(|msg| matches!(msg, WorkerToServerIpcMessage::WorkerReady { .. }))?;
    guard.queue.remove(idx)
}

#[cfg(test)]
impl ServerIpcConnection {
    pub(crate) fn mark_startup_message_seen_for_tests(&self) {
        self.inbox.lock().unwrap().startup_message_seen = true;
    }
}

#[cfg(test)]
mod tests {
    use super::super::protocol::{IpcHandlers, WorkerToServerIpcMessage};
    use super::super::test_support::test_connection_pair_with_handlers;
    use super::super::transport::IpcTransport;
    use super::{IpcWaitError, ServerIpcConnection};
    use crate::worker_protocol::TextStream;
    use serde_json::json;
    use std::io::Write;
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    fn wait_for_protocol_error(server: &ServerIpcConnection) -> Option<String> {
        let deadline = Instant::now() + Duration::from_millis(200);
        loop {
            if let Some(message) = server.take_protocol_error() {
                return Some(message);
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(5));
        }
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
                "type": "worker_ready",
                "protocol": { "name": "mcp-repl-worker", "version": 3, "extra": true },
                "worker": { "name": "r", "version": "0.0.0" },
                "capabilities": { "images": true }
            })
        )
        .expect("invalid worker message");

        let result = server.wait_for_worker_ready(Duration::from_millis(200));

        assert!(
            matches!(result, Err(super::IpcWaitError::Protocol(ref message)) if message.starts_with("invalid worker sideband JSON:")),
            "invalid worker message should report a protocol error, got: {result:?}"
        );
    }

    #[test]
    fn session_end_accepts_plain_utf8_message() {
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
        server.mark_startup_message_seen_for_tests();

        writeln!(
            worker_write,
            "{}",
            json!({
                "type": "session_end",
                "reason": "runtime_exit",
                "message": "runtime exited"
            })
        )
        .expect("session_end message");

        let result = server.wait_for_input_wait(Duration::from_millis(200));

        assert!(
            matches!(result, Err(super::IpcWaitError::SessionEnd)),
            "session_end with plain message should be accepted as session end, got: {result:?}"
        );
    }

    #[test]
    fn request_completion_keeps_protocol_error_latched_after_stable_prompt() {
        let stable_wait = Duration::from_millis(20);
        let (server, worker) =
            test_connection_pair_with_handlers(IpcHandlers::default()).expect("ipc pair");

        worker
            .send(WorkerToServerIpcMessage::InputWait {
                prompt: "zod> ".to_string(),
            })
            .expect("send initial input_wait");
        server
            .wait_for_input_wait(Duration::from_millis(200))
            .expect("server observes initial input_wait");
        server.begin_input().expect("begin input");
        worker
            .send(WorkerToServerIpcMessage::InputLine {
                prompt: "zod> ".to_string(),
                text: "done\n".to_string(),
            })
            .expect("send input_line");
        worker
            .send(WorkerToServerIpcMessage::InputWait {
                prompt: "zod> ".to_string(),
            })
            .expect("send input_wait");
        thread::sleep(stable_wait + Duration::from_millis(5));
        worker
            .send(WorkerToServerIpcMessage::OutputText {
                stream: TextStream::Stdout,
                data_b64: "***".to_string(),
                is_continuation: false,
            })
            .expect("send invalid output_text");
        thread::sleep(Duration::from_millis(5));

        let completion = server.wait_for_request_completion(Duration::from_secs(1), stable_wait);
        assert!(
            completion.is_ok(),
            "stable request completion should win over later idle protocol error, got: {completion:?}"
        );
        let latched = server.take_protocol_error();
        assert_eq!(latched.as_deref(), Some("invalid output_text base64"));
    }

    #[test]
    fn invalid_output_image_base64_latches_protocol_error() {
        let (server, worker) =
            test_connection_pair_with_handlers(IpcHandlers::default()).expect("ipc pair");

        worker
            .send(WorkerToServerIpcMessage::OutputImage {
                mime_type: "image/png".to_string(),
                data_b64: "***".to_string(),
                is_update: false,
                source: None,
            })
            .expect("send invalid output_image");

        let err = wait_for_protocol_error(&server).expect("server should latch output_image error");
        assert_eq!(err, "invalid output_image base64");
    }

    #[test]
    fn begin_input_requires_input_wait_readiness() {
        let (server, _worker) =
            test_connection_pair_with_handlers(IpcHandlers::default()).expect("ipc pair");

        let err = server
            .begin_input()
            .expect_err("input cannot start before input_wait");

        assert_eq!(err, "input_batch sent while worker is not ready for input");
    }

    #[test]
    fn input_wait_without_active_input_updates_readiness_and_prompt() {
        let (server, worker) =
            test_connection_pair_with_handlers(IpcHandlers::default()).expect("ipc pair");

        worker
            .send(WorkerToServerIpcMessage::InputWait {
                prompt: "ready> ".to_string(),
            })
            .expect("send input_wait");

        let prompt = server
            .wait_for_input_wait(Duration::from_millis(200))
            .expect("server observes input_wait");
        assert_eq!(prompt, "ready> ");
        server
            .begin_input()
            .expect("input_wait should make worker ready for input");
    }

    #[test]
    fn interrupt_does_not_clear_input_wait_readiness() {
        let (server, worker) =
            test_connection_pair_with_handlers(IpcHandlers::default()).expect("ipc pair");

        worker
            .send(WorkerToServerIpcMessage::InputWait {
                prompt: "ready> ".to_string(),
            })
            .expect("send input_wait");
        server
            .wait_for_input_wait(Duration::from_millis(200))
            .expect("server observes input_wait");

        server.note_interrupt_sent();

        server
            .begin_input()
            .expect("interrupt must not change input_wait readiness");
    }

    #[test]
    fn fresh_readiness_wait_ignores_cached_ready() {
        let (server, worker) =
            test_connection_pair_with_handlers(IpcHandlers::default()).expect("ipc pair");

        worker
            .send(WorkerToServerIpcMessage::Ready {})
            .expect("send ready");
        assert!(matches!(
            server.wait_for_input_readiness(Duration::from_millis(200)),
            Ok(super::IpcInputReadiness::Ready)
        ));

        let after_cached_ready = Instant::now();
        let stale =
            server.wait_for_fresh_input_readiness(Duration::from_millis(20), after_cached_ready);
        assert!(
            matches!(stale, Err(super::IpcWaitError::Timeout)),
            "fresh readiness wait should ignore cached ready, got: {stale:?}"
        );

        worker
            .send(WorkerToServerIpcMessage::Ready {})
            .expect("send fresh ready");
        assert!(matches!(
            server.wait_for_fresh_input_readiness(Duration::from_millis(200), after_cached_ready),
            Ok(super::IpcInputReadiness::Ready)
        ));

        let observed_input_wait = Arc::new((Mutex::new(false), Condvar::new()));
        let handler_observed_input_wait = observed_input_wait.clone();
        let (server, worker) = test_connection_pair_with_handlers(IpcHandlers {
            on_input_wait: Some(Arc::new(move |_| {
                let (lock, cvar) = &*handler_observed_input_wait;
                *lock.lock().expect("input_wait mutex") = true;
                cvar.notify_all();
            })),
            ..IpcHandlers::default()
        })
        .expect("ipc pair");
        worker
            .send(WorkerToServerIpcMessage::InputWait {
                prompt: "stale> ".to_string(),
            })
            .expect("send stale input_wait");
        let (lock, cvar) = &*observed_input_wait;
        let observed = lock.lock().expect("input_wait mutex");
        let observed = cvar
            .wait_timeout_while(observed, Duration::from_millis(200), |observed| !*observed)
            .expect("input_wait cvar")
            .0;
        assert!(*observed, "server should observe stale input_wait");
        drop(observed);

        let after_cached_input_wait = Instant::now();
        let stale = server
            .wait_for_fresh_input_readiness(Duration::from_millis(20), after_cached_input_wait);
        assert!(
            matches!(stale, Err(super::IpcWaitError::Timeout)),
            "fresh readiness wait should ignore cached input_wait, got: {stale:?}"
        );

        worker
            .send(WorkerToServerIpcMessage::InputWait {
                prompt: "fresh> ".to_string(),
            })
            .expect("send fresh input_wait");
        assert!(matches!(
            server.wait_for_fresh_input_readiness(
                Duration::from_millis(200),
                after_cached_input_wait
            ),
            Ok(super::IpcInputReadiness::InputWait(prompt)) if prompt == "fresh> "
        ));
    }

    #[test]
    fn input_line_without_active_input_is_protocol_error() {
        let (server, worker) =
            test_connection_pair_with_handlers(IpcHandlers::default()).expect("ipc pair");
        worker
            .send(WorkerToServerIpcMessage::InputWait {
                prompt: "> ".to_string(),
            })
            .expect("send input_wait");
        server
            .wait_for_input_wait(Duration::from_millis(200))
            .expect("server observes input_wait");

        worker
            .send(WorkerToServerIpcMessage::InputLine {
                prompt: "> ".to_string(),
                text: "orphan\n".to_string(),
            })
            .expect("send input_line");

        let err = wait_for_protocol_error(&server)
            .expect("server should latch input_line protocol error");
        assert_eq!(err, "input_line reported with no active input");
    }
    #[test]
    fn output_image_updates_reuse_current_server_image_id() {
        let images = Arc::new(Mutex::new(Vec::new()));
        let handler_images = images.clone();
        let (_server, worker) = test_connection_pair_with_handlers(IpcHandlers {
            on_output_image: Some(Arc::new(move |image| {
                handler_images.lock().expect("image mutex").push(image);
            })),
            ..IpcHandlers::default()
        })
        .expect("ipc pair");
        let first = json!({
            "type": "output_image",
            "mime_type": "image/png",
            "data_b64": "Zmlyc3Q=",
            "is_update": false,
            "source": "plot-1"
        })
        .to_string();
        let second = json!({
            "type": "output_image",
            "mime_type": "image/png",
            "data_b64": "c2Vjb25k",
            "is_update": true,
            "source": "plot-1"
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
        assert_eq!(images[0].data, "Zmlyc3Q=");
        assert_eq!(images[1].data, "c2Vjb25k");
    }
    #[test]
    fn output_image_ids_do_not_repeat_across_server_connections() {
        fn next_connection_image_id() -> String {
            let images = Arc::new(Mutex::new(Vec::new()));
            let handler_images = images.clone();
            let (_server, worker) = test_connection_pair_with_handlers(IpcHandlers {
                on_output_image: Some(Arc::new(move |image| {
                    handler_images.lock().expect("image mutex").push(image);
                })),
                ..IpcHandlers::default()
            })
            .expect("ipc pair");
            let image = json!({
                "type": "output_image",
                "mime_type": "image/png",
                "data_b64": "aW1hZ2U=",
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
