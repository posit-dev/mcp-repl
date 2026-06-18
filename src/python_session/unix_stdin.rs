use std::sync::atomic::{AtomicI32, Ordering};

use crate::ipc;
use crate::python_turn_input::{PtyFeed, normalize_pty_turn_payload};
use crate::stdin_payload::prepare_worker_stdin_payload;
use crate::worker_protocol::TextStream;

use super::{
    CStdinLine, PythonReadlineState, PythonThreadsAllowed, RawStdinReadError, SESSION_STATE,
    SessionStateInner, StdinReadAccounting, emit_output_text, emit_plots, flush_original_stdio,
    mark_stdin_wait_prompt_completed_request, record_background_plots, set_callback_error,
};

static PYTHON_RUNTIME_STDIN_FD: AtomicI32 = AtomicI32::new(-1);

pub(super) fn set_runtime_stdin_fd(fd: libc::c_int) {
    PYTHON_RUNTIME_STDIN_FD.store(fd, Ordering::SeqCst);
}

pub(super) fn flush_terminal_input() {
    let _ = unsafe { libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH) };
}

pub(super) fn begin_or_append_turn_input(turn_id: u64, input: &str) {
    let payload = normalize_pty_turn_payload(prepare_worker_stdin_payload(input));
    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    let should_record_background_plots = {
        let guard = state.inner.lock().unwrap();
        !guard.request_active || guard.request_completed_at_stdin_wait
    };
    if should_record_background_plots {
        record_background_plots();
    }

    let mut failure = None;
    let demand = {
        let mut guard = state.inner.lock().unwrap();
        match guard.turn_input.begin_or_append(turn_id, payload) {
            Err(message) => {
                mark_protocol_failure_locked(&mut guard);
                failure = Some(message);
                None
            }
            Ok(()) => {
                guard.interrupt_requested = false;
                guard.request_completed_at_stdin_wait = false;
                guard.request_active = true;
                guard.plot_reset_pending = true;
                if guard.waiting_for_input {
                    let prompt = guard.current_prompt.clone().unwrap_or_default();
                    Some(prepare_readline_demand_locked(&mut guard, &prompt))
                } else {
                    None
                }
            }
        }
    };
    if let Some(message) = failure {
        emit_protocol_failure(&message);
        return;
    }
    if let Some(demand) = demand {
        emit_readline_demand(demand);
    }
}

pub(super) fn discard_pending_stdin() {
    let mut discarded = Vec::new();
    discarded.extend(drain_process_stdin_pipe());
    clear_protocol_stdin_after_interrupt(&discarded);
}

fn clear_protocol_stdin_after_interrupt(runtime_discarded: &[u8]) {
    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    let mut guard = state.inner.lock().unwrap();
    guard.turn_input.clear_after_interrupt(runtime_discarded);
}

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

struct NonBlockingFd {
    fd: libc::c_int,
    previous_flags: Option<libc::c_int>,
}

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

impl Drop for NonBlockingFd {
    fn drop(&mut self) {
        if let Some(flags) = self.previous_flags {
            let _ = unsafe { libc::fcntl(self.fd, libc::F_SETFL, flags) };
        }
    }
}

fn stdin_read_would_block(err: &std::io::Error) -> bool {
    matches!(err.raw_os_error(), Some(code) if code == libc::EAGAIN || code == libc::EWOULDBLOCK)
}

pub(super) fn request_runtime_stdin_line(prompt: &str) -> bool {
    let Some(state) = SESSION_STATE.get() else {
        ipc::emit_readline_start(prompt);
        return true;
    };
    let demand = {
        let mut guard = state.inner.lock().unwrap();
        prepare_readline_demand_locked(&mut guard, prompt)
    };
    emit_readline_demand(demand);
    true
}

fn runtime_stdin_read_in_progress() -> bool {
    runtime_stdin_pending_byte_count().is_some_and(|count| count > 0)
}

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

enum ReadlineDemand {
    Feed(PtyFeed),
    Idle { turn_id: u64, prompt: String },
    StdinWait { turn_id: u64, prompt: String },
    ReadlineStart { prompt: String },
    ProtocolFailure { message: String },
}

fn mark_protocol_failure_locked(guard: &mut SessionStateInner) {
    guard.session_end_emitted = true;
    guard.shutdown = true;
    guard.request_active = false;
    guard.turn_input.clear_for_protocol_failure();
}

pub(super) fn emit_protocol_failure(message: &str) {
    if let Some(state) = SESSION_STATE.get() {
        let mut guard = state.inner.lock().unwrap();
        mark_protocol_failure_locked(&mut guard);
    }
    emit_output_text(TextStream::Stderr, message.as_bytes());
    ipc::emit_session_end();
}

fn prepare_readline_demand_locked(guard: &mut SessionStateInner, prompt: &str) -> ReadlineDemand {
    guard.waiting_for_input = true;
    let feed = match guard
        .turn_input
        .prepare_pty_feed(runtime_stdin_pending_byte_count())
    {
        Ok(feed) => feed,
        Err(message) => {
            mark_protocol_failure_locked(guard);
            return ReadlineDemand::ProtocolFailure { message };
        }
    };
    if let Some(feed) = feed {
        return ReadlineDemand::Feed(feed);
    }
    if guard.turn_input.pty_feed_in_flight() {
        return ReadlineDemand::ReadlineStart {
            prompt: prompt.to_string(),
        };
    }

    let Some(turn_id) = guard.turn_input.take_consumed_turn() else {
        return ReadlineDemand::ReadlineStart {
            prompt: prompt.to_string(),
        };
    };
    let prompt = prompt.to_string();
    if matches!(
        guard.current_readline_state,
        Some(PythonReadlineState::Primary | PythonReadlineState::Continuation)
    ) {
        guard.request_active = false;
        ReadlineDemand::Idle { turn_id, prompt }
    } else {
        ReadlineDemand::StdinWait { turn_id, prompt }
    }
}

fn emit_readline_demand(demand: ReadlineDemand) {
    match demand {
        ReadlineDemand::Feed(feed) => {
            ipc::emit_pty_feed(feed.turn_id, feed.seq, &feed.bytes);
        }
        ReadlineDemand::Idle { turn_id, prompt } => {
            ipc::emit_idle(turn_id, &prompt);
        }
        ReadlineDemand::StdinWait { turn_id, prompt } => {
            emit_plots();
            mark_stdin_wait_prompt_completed_request();
            ipc::emit_stdin_wait(turn_id, &prompt);
        }
        ReadlineDemand::ReadlineStart { prompt } => {
            ipc::emit_readline_start(&prompt);
        }
        ReadlineDemand::ProtocolFailure { message } => {
            emit_protocol_failure(&message);
        }
    }
}

fn protocol_request_input_exhausted() -> bool {
    let Some(state) = SESSION_STATE.get() else {
        return stdin_pending_byte_count() == Some(0);
    };
    let guard = state.inner.lock().unwrap();
    guard.turn_input.queued_input_exhausted()
}

pub(super) fn handle_protocol_input_hook() {
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

pub(super) fn request_cpython_readline_stdin_line(prompt: &str) {
    let Some(state) = SESSION_STATE.get() else {
        ipc::emit_readline_start(prompt);
        return;
    };
    let demand = {
        let mut guard = state.inner.lock().unwrap();
        prepare_readline_demand_locked(&mut guard, prompt)
    };
    emit_readline_demand(demand);
}

pub(super) fn note_cpython_readline_bytes_read(
    prompt: &str,
    bytes: &[u8],
) -> Result<StdinReadAccounting, String> {
    if bytes.is_empty() {
        return Ok(StdinReadAccounting::Accounted);
    }
    let Some((turn_id, protocol_bytes)) = consume_protocol_stdin_bytes_for_runtime_read(bytes)?
    else {
        return Ok(StdinReadAccounting::DiscardedAfterInterrupt);
    };
    mark_request_input_delivered();
    note_active_stdin_line_read(&protocol_bytes);
    ipc::emit_input_line(turn_id, prompt, &String::from_utf8_lossy(&protocol_bytes));
    Ok(StdinReadAccounting::Accounted)
}

pub(super) fn fork_child_stdin_eof(prompt: &str) -> CStdinLine {
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

pub(super) fn read_raw_stdin_bytes(size: usize) -> Result<Vec<u8>, RawStdinReadError> {
    if ipc::worker_ipc_disabled_for_process() {
        return Ok(Vec::new());
    }

    request_runtime_stdin_line("");
    let _allow_threads = PythonThreadsAllowed::new();
    let bytes = read_fd_bytes(libc::STDIN_FILENO, size);
    if let Err(err) = note_stdin_bytes_read(&bytes) {
        emit_protocol_failure(&err);
        set_callback_error(&err);
    }
    Ok(bytes)
}

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

fn note_stdin_bytes_read(bytes: &[u8]) -> Result<(), String> {
    if bytes.is_empty() {
        return Ok(());
    }
    if let Some((_turn_id, protocol_bytes)) = consume_protocol_stdin_bytes_for_runtime_read(bytes)?
    {
        mark_request_input_delivered();
        note_active_stdin_line_read(&protocol_bytes);
    }
    Ok(())
}

fn consume_protocol_stdin_bytes_for_runtime_read(
    runtime_bytes: &[u8],
) -> Result<Option<(u64, Vec<u8>)>, String> {
    let Some(state) = SESSION_STATE.get() else {
        return Err("Python session state is unavailable while consuming stdin".to_string());
    };
    let mut guard = state.inner.lock().unwrap();
    Ok(guard
        .turn_input
        .consume_runtime_read(runtime_bytes)?
        .map(|read| (read.turn_id, read.protocol_bytes)))
}

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

pub(super) fn note_stdin_line_read(
    prompt: &str,
    bytes: &[u8],
) -> Result<StdinReadAccounting, String> {
    if bytes.is_empty() {
        return Ok(StdinReadAccounting::Accounted);
    }
    let Some((turn_id, protocol_bytes)) = consume_protocol_stdin_bytes_for_runtime_read(bytes)?
    else {
        return Ok(StdinReadAccounting::DiscardedAfterInterrupt);
    };
    mark_request_input_delivered();
    note_active_stdin_line_read(&protocol_bytes);
    ipc::emit_input_line(turn_id, prompt, &String::from_utf8_lossy(&protocol_bytes));
    Ok(StdinReadAccounting::Accounted)
}

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

fn stdin_pending_byte_count() -> Option<usize> {
    let mut count: libc::c_int = 0;
    let rc = unsafe { libc::ioctl(libc::STDIN_FILENO, libc::FIONREAD, &mut count) };
    if rc == 0 && count >= 0 {
        Some(count as usize)
    } else {
        None
    }
}
