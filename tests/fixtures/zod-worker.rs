#[cfg(target_family = "unix")]
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
#[cfg(target_family = "unix")]
use std::os::unix::io::FromRawFd;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::{Deserialize, Serialize};

#[cfg(target_family = "unix")]
const IPC_READ_FD_ENV: &str = "MCP_REPL_IPC_READ_FD";
#[cfg(target_family = "unix")]
const IPC_WRITE_FD_ENV: &str = "MCP_REPL_IPC_WRITE_FD";
#[cfg(target_family = "windows")]
const IPC_PIPE_TO_WORKER_ENV: &str = "MCP_REPL_IPC_PIPE_TO_WORKER";
#[cfg(target_family = "windows")]
const IPC_PIPE_FROM_WORKER_ENV: &str = "MCP_REPL_IPC_PIPE_FROM_WORKER";
const STARTUP_PROTOCOL_ERROR_ENV: &str = "MCP_REPL_ZOD_STARTUP_PROTOCOL_ERROR";
const CONTROL_LOG_ENV: &str = "MCP_REPL_ZOD_CONTROL_LOG";
const STALL_CONTROL_READER_ENV: &str = "MCP_REPL_ZOD_STALL_CONTROL_READER";
const INVALID_OUTPUT_TEXT_BASE64: &str =
    r#"{"type":"output_text","stream":"stdout","data_b64":"***"}"#;

#[cfg(target_family = "unix")]
static INTERRUPTED_BY_OS: AtomicBool = AtomicBool::new(false);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_family = "unix")]
    install_signal_handler();

    let transport = IpcTransport::connect_from_env()?;
    run_worker(transport.reader, IpcWriter::new(transport.writer))
}

fn run_worker(
    sideband_reader: Box<dyn Read + Send>,
    writer: IpcWriter,
) -> Result<(), Box<dyn std::error::Error>> {
    let control_log_path = std::env::var_os(CONTROL_LOG_ENV).map(PathBuf::from);
    let sideband_interrupted = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();

    start_stdin_observer(control_log_path.clone());

    writer.send(&WorkerToServer::WorkerReady {
        protocol: Protocol {
            name: "mcp-repl-worker".to_string(),
            version: 4,
        },
        worker: WorkerIdentity {
            name: "zod".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
        capabilities: Capabilities { images: true },
    })?;
    if std::env::var_os(STARTUP_PROTOCOL_ERROR_ENV).is_some() {
        writer.send_raw_json(INVALID_OUTPUT_TEXT_BASE64)?;
    }
    if std::env::var_os(STALL_CONTROL_READER_ENV).is_some() {
        let _sideband_reader = sideband_reader;
        let _turn_tx = tx;
        loop {
            thread::park();
        }
    }

    start_control_reader(
        sideband_reader,
        tx,
        sideband_interrupted.clone(),
        control_log_path.clone(),
    );

    let mut state = CommandState {
        next_prompt: "v4> ".to_string(),
        previous_line_empty: false,
        input_line_after_idle: false,
        session_end_after_idle: false,
        bad_output_after_idle: None,
    };
    while let Ok(message) = rx.recv() {
        match message {
            ControlMessage::TurnStart { turn_id, input } => {
                if run_turn(
                    &writer,
                    &sideband_interrupted,
                    &control_log_path,
                    turn_id,
                    &input,
                    &mut state,
                )? {
                    return Ok(());
                }
            }
            ControlMessage::Interrupt => {}
            ControlMessage::Shutdown => {
                send_session_end(&writer, None, "shutdown")?;
                return Ok(());
            }
        }
    }

    Ok(())
}

fn run_turn(
    writer: &IpcWriter,
    sideband_interrupted: &AtomicBool,
    control_log_path: &Option<PathBuf>,
    turn_id: u64,
    input: &str,
    state: &mut CommandState,
) -> io::Result<bool> {
    for raw_line in runtime_lines(input) {
        let prompt = state.next_prompt.clone();
        writer.send(&WorkerToServer::InputLine {
            turn_id,
            prompt,
            text: raw_line.clone(),
        })?;
        append_control_log(
            control_log_path.as_deref(),
            &format!(
                "input_line turn_id={turn_id} text={}",
                escape_bytes(raw_line.as_bytes())
            ),
        )?;
        let command = raw_line.trim_end_matches(['\r', '\n']);
        if run_command(
            writer,
            sideband_interrupted,
            control_log_path,
            turn_id,
            command,
            state,
        )? {
            return Ok(true);
        }
        state.previous_line_empty = command.is_empty();
    }

    let prompt = std::mem::replace(&mut state.next_prompt, "v4> ".to_string());
    writer.send(&WorkerToServer::InputWait { turn_id, prompt })?;
    append_control_log(
        control_log_path.as_deref(),
        &format!("input_wait turn_id={turn_id}"),
    )?;
    emit_deferred_protocol_faults(writer, control_log_path, turn_id, state)?;
    Ok(false)
}

fn run_command(
    writer: &IpcWriter,
    sideband_interrupted: &AtomicBool,
    control_log_path: &Option<PathBuf>,
    turn_id: u64,
    command: &str,
    state: &mut CommandState,
) -> io::Result<bool> {
    if command == "input-wait-only" {
        return Ok(false);
    }

    if let Some(prompt) = command.strip_prefix("wait ") {
        state.next_prompt = prompt.to_string();
        return Ok(false);
    }

    if let Some(millis) = command.strip_prefix("sleep ") {
        sleep_for(parse_millis(millis)?, sideband_interrupted, false);
        return Ok(false);
    }

    if let Some(millis) = command.strip_prefix("bad-output-after-sleep ") {
        sleep_for(parse_millis(millis)?, sideband_interrupted, false);
        append_control_log(
            control_log_path.as_deref(),
            &format!("bad_output_after_sleep turn_id={turn_id}"),
        )?;
        writer.send_raw_json(INVALID_OUTPUT_TEXT_BASE64)?;
        sleep_for(5_000, sideband_interrupted, false);
        return Ok(false);
    }

    if let Some(millis) = command.strip_prefix("interrupt-report ") {
        let report = observe_interrupts_for(parse_millis(millis)?, sideband_interrupted);
        let text = format!(
            "sideband interrupt: {}\nos interrupt: {}\n",
            if report.sideband {
                "observed"
            } else {
                "missing"
            },
            if report.os { "observed" } else { "missing" },
        );
        output_text(writer, control_log_path, turn_id, text.as_bytes())?;
        return Ok(false);
    }

    if command == "emit-output-after-input" {
        output_text(writer, control_log_path, turn_id, b"after input_line\n")?;
        return Ok(false);
    }

    if command == "late-input-line-after-input-wait" {
        state.input_line_after_idle = true;
        return Ok(false);
    }

    if command == "session-end-after-input-wait" {
        state.session_end_after_idle = true;
        return Ok(false);
    }

    if let Some(millis) = command.strip_prefix("bad-output-after-input-wait ") {
        state.bad_output_after_idle = Some(Duration::from_millis(parse_millis(millis)?));
        return Ok(false);
    }

    if command == "exit" {
        send_session_end(writer, Some(turn_id), "runtime_exit")?;
        return Ok(true);
    }

    let text = format!("v4-output: {command}\n");
    output_text(writer, control_log_path, turn_id, text.as_bytes())?;
    Ok(false)
}

fn emit_deferred_protocol_faults(
    writer: &IpcWriter,
    control_log_path: &Option<PathBuf>,
    turn_id: u64,
    state: &mut CommandState,
) -> io::Result<()> {
    if state.input_line_after_idle {
        state.input_line_after_idle = false;
        append_control_log(
            control_log_path.as_deref(),
            &format!("late_input_line turn_id={turn_id}"),
        )?;
        writer.send(&WorkerToServer::InputLine {
            turn_id,
            prompt: "v4> ".to_string(),
            text: "late\n".to_string(),
        })?;
    }
    if state.session_end_after_idle {
        state.session_end_after_idle = false;
        append_control_log(
            control_log_path.as_deref(),
            &format!("late_session_end turn_id={turn_id}"),
        )?;
        send_session_end(writer, Some(turn_id), "runtime_exit")?;
    }
    if let Some(delay) = state.bad_output_after_idle.take() {
        let writer = writer.clone();
        let control_log_path = control_log_path.clone();
        thread::spawn(move || {
            thread::sleep(delay);
            let _ = append_control_log(
                control_log_path.as_deref(),
                &format!("late_bad_output turn_id={turn_id}"),
            );
            let _ = writer.send_raw_json(INVALID_OUTPUT_TEXT_BASE64);
        });
    }
    Ok(())
}

fn output_text(
    writer: &IpcWriter,
    control_log_path: &Option<PathBuf>,
    turn_id: u64,
    bytes: &[u8],
) -> io::Result<()> {
    append_control_log(
        control_log_path.as_deref(),
        &format!("output_text turn_id={turn_id}"),
    )?;
    writer.output_text("stdout", bytes)
}

fn send_session_end(writer: &IpcWriter, turn_id: Option<u64>, reason: &str) -> io::Result<()> {
    writer.send(&WorkerToServer::SessionEnd {
        reason: reason.to_string(),
        message_b64: None,
        turn_id,
    })
}

fn runtime_lines(input: &str) -> Vec<String> {
    let mut text = input.to_string();
    if !text.ends_with(['\n', '\r']) {
        text.push('\n');
    }
    let mut lines = Vec::new();
    let mut start = 0usize;
    for (idx, ch) in text.char_indices() {
        if ch == '\n' {
            lines.push(text[start..=idx].to_string());
            start = idx + ch.len_utf8();
        }
    }
    if start < text.len() {
        lines.push(text[start..].to_string());
    }
    lines
}

struct CommandState {
    next_prompt: String,
    previous_line_empty: bool,
    input_line_after_idle: bool,
    session_end_after_idle: bool,
    bad_output_after_idle: Option<Duration>,
}

fn start_control_reader(
    reader: Box<dyn Read + Send>,
    turn_tx: mpsc::Sender<ControlMessage>,
    interrupted: Arc<AtomicBool>,
    control_log_path: Option<PathBuf>,
) {
    thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => {
                    let _ = turn_tx.send(ControlMessage::Shutdown);
                    return;
                }
                Ok(_) => {}
            }
            let message = serde_json::from_str::<ServerToWorker>(line.trim_end());
            match message {
                Ok(ServerToWorker::TurnStart { turn_id, input }) => {
                    let _ = append_control_log(
                        control_log_path.as_deref(),
                        &format!(
                            "turn_start turn_id={turn_id} input={}",
                            escape_bytes(input.as_bytes())
                        ),
                    );
                    let _ = turn_tx.send(ControlMessage::TurnStart { turn_id, input });
                }
                Ok(ServerToWorker::Interrupt { turn_id }) => {
                    interrupted.store(true, Ordering::SeqCst);
                    let _ = append_control_log(
                        control_log_path.as_deref(),
                        &format!("interrupt turn_id={}", turn_id.unwrap_or(0)),
                    );
                    let _ = turn_tx.send(ControlMessage::Interrupt);
                }
                Err(_) => {}
            }
        }
    });
}

fn start_stdin_observer(control_log_path: Option<PathBuf>) {
    thread::spawn(move || {
        let stdin = io::stdin();
        let mut reader = BufReader::new(stdin.lock());
        let mut buffer = Vec::new();
        loop {
            buffer.clear();
            match reader.read_until(b'\n', &mut buffer) {
                Ok(0) | Err(_) => return,
                Ok(_) => {
                    let _ = append_control_log(
                        control_log_path.as_deref(),
                        &format!("stdin:{}", escape_bytes(&buffer)),
                    );
                }
            }
        }
    });
}

#[derive(Debug)]
enum ControlMessage {
    TurnStart { turn_id: u64, input: String },
    Interrupt,
    Shutdown,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerToWorker {
    TurnStart { turn_id: u64, input: String },
    Interrupt { turn_id: Option<u64> },
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WorkerToServer {
    WorkerReady {
        protocol: Protocol,
        worker: WorkerIdentity,
        capabilities: Capabilities,
    },
    OutputText {
        stream: String,
        data_b64: String,
    },
    InputLine {
        turn_id: u64,
        prompt: String,
        text: String,
    },
    InputWait {
        turn_id: u64,
        prompt: String,
    },
    SessionEnd {
        reason: String,
        message_b64: Option<String>,
        turn_id: Option<u64>,
    },
}

#[derive(Serialize)]
struct Protocol {
    name: String,
    version: u32,
}

#[derive(Serialize)]
struct WorkerIdentity {
    name: String,
    version: String,
}

#[derive(Serialize)]
struct Capabilities {
    images: bool,
}

#[derive(Clone)]
struct IpcWriter {
    inner: Arc<Mutex<Box<dyn Write + Send>>>,
}

impl IpcWriter {
    fn new(writer: Box<dyn Write + Send>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(writer)),
        }
    }

    fn send<T: Serialize>(&self, message: &T) -> io::Result<()> {
        let payload = serde_json::to_vec(message).map_err(io::Error::other)?;
        let mut writer = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("ipc writer mutex poisoned"))?;
        writer.write_all(&payload)?;
        writer.write_all(b"\n")?;
        writer.flush()
    }

    fn send_raw_json(&self, json: &str) -> io::Result<()> {
        let mut writer = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("ipc writer mutex poisoned"))?;
        writer.write_all(json.as_bytes())?;
        writer.write_all(b"\n")?;
        writer.flush()
    }

    fn output_text(&self, stream: &str, bytes: &[u8]) -> io::Result<()> {
        self.send(&WorkerToServer::OutputText {
            stream: stream.to_string(),
            data_b64: base64::engine::general_purpose::STANDARD.encode(bytes),
        })
    }
}

struct IpcTransport {
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
}

impl IpcTransport {
    fn connect_from_env() -> io::Result<Self> {
        #[cfg(target_family = "unix")]
        {
            let read_fd = std::env::var(IPC_READ_FD_ENV)
                .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "IPC read fd missing"))?;
            let write_fd = std::env::var(IPC_WRITE_FD_ENV)
                .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "IPC write fd missing"))?;
            let read_fd = read_fd
                .parse()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid IPC read fd"))?;
            let write_fd = write_fd
                .parse()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid IPC write fd"))?;
            let reader = unsafe { File::from_raw_fd(read_fd) };
            let writer = unsafe { File::from_raw_fd(write_fd) };
            Ok(Self {
                reader: Box::new(reader),
                writer: Box::new(writer),
            })
        }

        #[cfg(target_family = "windows")]
        {
            let pipe_to_worker = std::env::var(IPC_PIPE_TO_WORKER_ENV).map_err(|_| {
                io::Error::new(io::ErrorKind::NotFound, "IPC to-worker pipe missing")
            })?;
            let pipe_from_worker = std::env::var(IPC_PIPE_FROM_WORKER_ENV).map_err(|_| {
                io::Error::new(io::ErrorKind::NotFound, "IPC from-worker pipe missing")
            })?;
            let reader = std::fs::OpenOptions::new()
                .read(true)
                .open(pipe_to_worker)?;
            let writer = std::fs::OpenOptions::new()
                .write(true)
                .open(pipe_from_worker)?;
            Ok(Self {
                reader: Box::new(reader),
                writer: Box::new(writer),
            })
        }

        #[cfg(not(any(target_family = "unix", target_family = "windows")))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "zod-worker sideband transport is unsupported on this platform",
            ))
        }
    }
}

struct InterruptReport {
    sideband: bool,
    os: bool,
}

fn observe_interrupts_for(millis: u64, sideband_interrupted: &AtomicBool) -> InterruptReport {
    let deadline = Instant::now() + Duration::from_millis(millis);
    let mut sideband = false;
    let mut os = false;
    while Instant::now() < deadline {
        sideband |= sideband_interrupted.swap(false, Ordering::SeqCst);
        os |= take_os_interrupt();
        if sideband && os {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    InterruptReport { sideband, os }
}

fn sleep_for(millis: u64, sideband_interrupted: &AtomicBool, interruptible: bool) -> bool {
    let deadline = Instant::now() + Duration::from_millis(millis);
    loop {
        let sideband = sideband_interrupted.swap(false, Ordering::SeqCst);
        let os = take_os_interrupt();
        if interruptible && (sideband || os) {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(10).min(deadline.saturating_duration_since(now)));
    }
}

fn parse_millis(value: &str) -> io::Result<u64> {
    value
        .parse::<u64>()
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))
}

#[cfg(target_family = "unix")]
fn install_signal_handler() {
    unsafe {
        libc::signal(libc::SIGINT, handle_sigint as *const () as usize);
    }
}

#[cfg(target_family = "unix")]
extern "C" fn handle_sigint(_signal: libc::c_int) {
    INTERRUPTED_BY_OS.store(true, Ordering::SeqCst);
}

#[cfg(target_family = "unix")]
fn take_os_interrupt() -> bool {
    INTERRUPTED_BY_OS.swap(false, Ordering::SeqCst)
}

#[cfg(not(target_family = "unix"))]
fn take_os_interrupt() -> bool {
    false
}

fn append_control_log(path: Option<&Path>, line: &str) -> io::Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")
}

fn escape_bytes(bytes: &[u8]) -> String {
    let mut escaped = String::new();
    for byte in bytes {
        match byte {
            b'\n' => escaped.push_str("\\n"),
            b'\r' => escaped.push_str("\\r"),
            b'\t' => escaped.push_str("\\t"),
            b'\\' => escaped.push_str("\\\\"),
            b' '..=b'~' => escaped.push(char::from(*byte)),
            _ => escaped.push_str(&format!("\\x{byte:02x}")),
        }
    }
    escaped
}
