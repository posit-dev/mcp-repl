#[cfg(target_family = "unix")]
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
#[cfg(target_family = "unix")]
use std::os::unix::io::FromRawFd;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
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
const SHUTDOWN_LOG_ENV: &str = "MCP_REPL_ZOD_SHUTDOWN_LOG";
const INVALID_OUTPUT_TEXT_BASE64: &str =
    r#"{"type":"output_text","stream":"stdout","data_b64":"***"}"#;
const INVALID_SESSION_END_REASON: &str =
    r#"{"type":"session_end","reason":"not-a-recognized-reason"}"#;

#[cfg(target_family = "unix")]
static INTERRUPTED_BY_OS: AtomicBool = AtomicBool::new(false);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_family = "unix")]
    install_signal_handler();

    let transport = IpcTransport::connect_from_env()?;
    let writer = IpcWriter::new(transport.writer);
    let interrupted = Arc::new(AtomicBool::new(false));
    let control_session_end = Arc::new(AtomicBool::new(false));
    let shutdown_log_path = std::env::var_os(SHUTDOWN_LOG_ENV).map(PathBuf::from);
    start_control_reader(
        transport.reader,
        interrupted.clone(),
        control_session_end.clone(),
        shutdown_log_path.clone(),
    );

    writer.send(&WorkerToServer::WorkerReady {
        protocol: Protocol {
            name: "mcp-repl-worker".to_string(),
            version: 1,
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
    writer.send(&WorkerToServer::ReadlineStart {
        prompt: "zod> ".to_string(),
    })?;

    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut line = String::new();
    let mut command_state = CommandState {
        next_prompt: "zod> ".to_string(),
        shutdown_mode: ShutdownMode::Normal,
    };
    let mut timeline = Timeline::default();
    loop {
        line.clear();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            let shutdown_event = if control_session_end.load(Ordering::SeqCst) {
                "sideband_shutdown"
            } else {
                "stdin_eof"
            };
            append_shutdown_log(shutdown_log_path.as_deref(), shutdown_event)?;
            apply_shutdown_mode(shutdown_log_path.as_deref(), command_state.shutdown_mode)?;
            send_session_end(&writer, &mut timeline, "shutdown")?;
            return Ok(());
        }

        let command = line.trim_end_matches(['\r', '\n']);
        let reported_input = if let Some(text) = command.strip_prefix("misreport-input ") {
            format!("{text}\n")
        } else {
            line.clone()
        };
        writer.send(&WorkerToServer::ReadlineInput {
            text: reported_input,
        })?;
        timeline.run(LifecyclePoint::AfterReadlineInput, &writer)?;
        if command == "exit" {
            append_shutdown_log(shutdown_log_path.as_deref(), "user-stdin:exit")?;
            apply_shutdown_mode(shutdown_log_path.as_deref(), command_state.shutdown_mode)?;
            send_session_end(&writer, &mut timeline, "runtime_exit")?;
            return Ok(());
        }
        if command == "bad-output-after-session-end" {
            send_session_end(&writer, &mut timeline, "runtime_exit")?;
            writer.output_text("stdout", b"late output\n")?;
            return Ok(());
        }
        timeline.run(LifecyclePoint::BeforeCommand, &writer)?;
        run_command(
            &writer,
            &interrupted,
            &mut reader,
            command,
            &line,
            &mut command_state,
            &mut timeline,
        )?;
        timeline.run(LifecyclePoint::AfterCommand, &writer)?;
        send_readline_start(
            &writer,
            &mut timeline,
            std::mem::replace(&mut command_state.next_prompt, "zod> ".to_string()),
        )?;
    }
}

fn run_command(
    writer: &IpcWriter,
    interrupted: &AtomicBool,
    reader: &mut dyn BufRead,
    command: &str,
    raw_line: &str,
    state: &mut CommandState,
    timeline: &mut Timeline,
) -> io::Result<()> {
    if let Some(prompt) = command.strip_prefix("wait ") {
        state.next_prompt = prompt.to_string();
        return Ok(());
    }

    if let Some(text) = command.strip_prefix("stderr ") {
        let mut text = text.as_bytes().to_vec();
        text.push(b'\n');
        writer.output_text("stderr", &text)?;
        return Ok(());
    }

    if let Some(millis) = command.strip_prefix("sleep ") {
        sleep_for(parse_millis(millis)?, interrupted, false);
        return Ok(());
    }

    if let Some(millis) = command.strip_prefix("raw-prompt-then-sleep ") {
        let mut stdout = io::stdout().lock();
        stdout.write_all(b"zod> raw stdout\n")?;
        stdout.flush()?;
        sleep_for(parse_millis(millis)?, interrupted, false);
        return Ok(());
    }

    if let Some(millis) = command.strip_prefix("prompt-then-sleep ") {
        writer.send(&WorkerToServer::ReadlineStart {
            prompt: "buffered> ".to_string(),
        })?;
        sleep_for(parse_millis(millis)?, interrupted, false);
        return Ok(());
    }

    if let Some(millis) = command.strip_prefix("bad-output-while-idle ") {
        schedule_invalid_output_text_base64(
            timeline,
            LifecyclePoint::AfterReadlineStart,
            Duration::from_millis(parse_millis(millis)?),
        );
        return Ok(());
    }

    if let Some(spec) = command.strip_prefix("timeline ") {
        schedule_timeline_command(timeline, spec)?;
        return Ok(());
    }

    if let Some(millis) = command.strip_prefix("bad-output-after-sleep ") {
        sleep_for(parse_millis(millis)?, interrupted, false);
        writer.send_raw_json(INVALID_OUTPUT_TEXT_BASE64)?;
        return Ok(());
    }

    if let Some(millis) = command.strip_prefix("interruptible ") {
        sleep_for(parse_millis(millis)?, interrupted, true);
        return Ok(());
    }

    if let Some(millis) = command.strip_prefix("slow-shutdown ") {
        state.shutdown_mode = ShutdownMode::Delay(Duration::from_millis(parse_millis(millis)?));
        return Ok(());
    }

    if command == "hang-shutdown" {
        state.shutdown_mode = ShutdownMode::Hang;
        return Ok(());
    }

    if let Some(millis) = command.strip_prefix("discard-on-interrupt ") {
        if sleep_for(parse_millis(millis)?, interrupted, true) {
            discard_buffered_stdin(reader, writer)?;
        }
        return Ok(());
    }

    if command == "image" {
        writer.send(&WorkerToServer::OutputImage {
            image_id: "zod-image".to_string(),
            mime_type: "image/png".to_string(),
            data_b64: base64::engine::general_purpose::STANDARD.encode(TINY_PNG),
            update: false,
        })?;
        return Ok(());
    }

    if command == "mixed-output" {
        writer.output_text("stdout", b"stdout-before\n")?;
        writer.output_text("stderr", b"stderr-middle\n")?;
        writer.output_text("stdout", b"stdout-after\n")?;
        return Ok(());
    }

    if command == "bad-output-base64" {
        writer.send_raw_json(INVALID_OUTPUT_TEXT_BASE64)?;
        return Ok(());
    }

    if command == "bad-session-end-reason" {
        writer.send_raw_json(INVALID_SESSION_END_REASON)?;
        return Ok(());
    }

    writer.output_text("stdout", raw_line.as_bytes())
}

struct CommandState {
    next_prompt: String,
    shutdown_mode: ShutdownMode,
}

#[derive(Clone, Copy)]
enum ShutdownMode {
    Normal,
    Delay(Duration),
    Hang,
}

fn apply_shutdown_mode(path: Option<&Path>, mode: ShutdownMode) -> io::Result<()> {
    match mode {
        ShutdownMode::Normal => Ok(()),
        ShutdownMode::Delay(delay) => {
            append_shutdown_log(path, &format!("shutdown:delay-ms:{}", delay.as_millis()))?;
            thread::sleep(delay);
            append_shutdown_log(path, "shutdown:delay-complete")
        }
        ShutdownMode::Hang => {
            append_shutdown_log(path, "shutdown:hang")?;
            loop {
                thread::sleep(Duration::from_secs(60));
            }
        }
    }
}

fn discard_buffered_stdin(reader: &mut dyn BufRead, writer: &IpcWriter) -> io::Result<()> {
    let (text, len) = {
        let buffer = reader.fill_buf()?;
        let text = std::str::from_utf8(buffer)
            .map_err(io::Error::other)?
            .to_string();
        (text, buffer.len())
    };
    if len == 0 {
        return Ok(());
    }
    reader.consume(len);
    writer.send(&WorkerToServer::ReadlineDiscard { text })
}

fn send_readline_start(
    writer: &IpcWriter,
    timeline: &mut Timeline,
    prompt: String,
) -> io::Result<()> {
    timeline.run(LifecyclePoint::BeforeReadlineStart, writer)?;
    writer.send(&WorkerToServer::ReadlineStart { prompt })?;
    timeline.run(LifecyclePoint::AfterReadlineStart, writer)
}

fn send_session_end(writer: &IpcWriter, timeline: &mut Timeline, reason: &str) -> io::Result<()> {
    timeline.run(LifecyclePoint::BeforeSessionEnd, writer)?;
    writer.send(&WorkerToServer::SessionEnd {
        reason: reason.to_string(),
        message_b64: None,
    })?;
    timeline.run(LifecyclePoint::AfterSessionEnd, writer)
}

fn append_shutdown_log(path: Option<&Path>, event: &str) -> io::Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(format!("{event}\n").as_bytes())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LifecyclePoint {
    AfterReadlineInput,
    BeforeCommand,
    AfterCommand,
    BeforeReadlineStart,
    AfterReadlineStart,
    BeforeSessionEnd,
    AfterSessionEnd,
}

#[derive(Default)]
struct Timeline {
    entries: Vec<TimelineEntry>,
}

impl Timeline {
    fn schedule(&mut self, point: LifecyclePoint, delay: Duration, action: TimelineAction) {
        self.entries.push(TimelineEntry {
            point,
            delay,
            action,
        });
    }

    fn run(&mut self, point: LifecyclePoint, writer: &IpcWriter) -> io::Result<()> {
        let mut idx = 0;
        while idx < self.entries.len() {
            if self.entries[idx].point == point {
                let entry = self.entries.remove(idx);
                entry.run(writer)?;
            } else {
                idx += 1;
            }
        }
        Ok(())
    }
}

struct TimelineEntry {
    point: LifecyclePoint,
    delay: Duration,
    action: TimelineAction,
}

impl TimelineEntry {
    fn run(self, writer: &IpcWriter) -> io::Result<()> {
        if self.delay.is_zero() {
            return self.action.run(writer);
        }

        let writer = writer.clone();
        thread::spawn(move || {
            thread::sleep(self.delay);
            let _ = self.action.run(&writer);
        });
        Ok(())
    }
}

enum TimelineAction {
    RawSideband(String),
}

impl TimelineAction {
    fn run(self, writer: &IpcWriter) -> io::Result<()> {
        let TimelineAction::RawSideband(payload) = self;
        writer.send_raw_json(&payload)
    }
}

fn schedule_timeline_command(timeline: &mut Timeline, spec: &str) -> io::Result<()> {
    let mut parts = spec.split_whitespace();
    let point = parse_lifecycle_point(parts.next())?;
    expect_timeline_token(parts.next(), "delay-ms")?;
    let delay = Duration::from_millis(parse_millis_token(parts.next(), "delay-ms value")?);
    let action = parse_timeline_action(parts.next())?;
    if parts.next().is_some() {
        return Err(invalid_timeline("unexpected trailing timeline tokens"));
    }
    timeline.schedule(point, delay, action);
    Ok(())
}

fn schedule_invalid_output_text_base64(
    timeline: &mut Timeline,
    point: LifecyclePoint,
    delay: Duration,
) {
    timeline.schedule(
        point,
        delay,
        TimelineAction::RawSideband(INVALID_OUTPUT_TEXT_BASE64.to_string()),
    );
}

fn parse_lifecycle_point(raw: Option<&str>) -> io::Result<LifecyclePoint> {
    match raw {
        Some("after-readline-input") => Ok(LifecyclePoint::AfterReadlineInput),
        Some("before-command") => Ok(LifecyclePoint::BeforeCommand),
        Some("after-command") => Ok(LifecyclePoint::AfterCommand),
        Some("before-readline-start") => Ok(LifecyclePoint::BeforeReadlineStart),
        Some("after-readline-start") => Ok(LifecyclePoint::AfterReadlineStart),
        Some("before-session-end") => Ok(LifecyclePoint::BeforeSessionEnd),
        Some("after-session-end") => Ok(LifecyclePoint::AfterSessionEnd),
        Some(other) => Err(invalid_timeline(&format!(
            "unknown lifecycle point {other:?}"
        ))),
        None => Err(invalid_timeline("missing lifecycle point")),
    }
}

fn parse_timeline_action(raw: Option<&str>) -> io::Result<TimelineAction> {
    match raw {
        Some("raw-output-text-invalid-base64") => Ok(TimelineAction::RawSideband(
            INVALID_OUTPUT_TEXT_BASE64.to_string(),
        )),
        Some(other) => Err(invalid_timeline(&format!(
            "unknown timeline action {other:?}"
        ))),
        None => Err(invalid_timeline("missing timeline action")),
    }
}

fn expect_timeline_token(raw: Option<&str>, expected: &str) -> io::Result<()> {
    match raw {
        Some(value) if value == expected => Ok(()),
        Some(other) => Err(invalid_timeline(&format!(
            "expected {expected:?}, got {other:?}"
        ))),
        None => Err(invalid_timeline(&format!("missing {expected:?}"))),
    }
}

fn parse_millis_token(raw: Option<&str>, name: &str) -> io::Result<u64> {
    raw.ok_or_else(|| invalid_timeline(&format!("missing {name}")))
        .and_then(parse_millis)
}

fn invalid_timeline(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn parse_millis(raw: &str) -> io::Result<u64> {
    raw.trim()
        .parse::<u64>()
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))
}

fn sleep_for(millis: u64, interrupted: &AtomicBool, interruptible: bool) -> bool {
    let deadline = Instant::now() + Duration::from_millis(millis);
    while Instant::now() < deadline {
        if interruptible
            && (interrupted.swap(false, Ordering::SeqCst) || os_interrupted().unwrap_or(false))
        {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    false
}

#[cfg(target_family = "unix")]
fn os_interrupted() -> Option<bool> {
    Some(INTERRUPTED_BY_OS.swap(false, Ordering::SeqCst))
}

#[cfg(not(target_family = "unix"))]
fn os_interrupted() -> Option<bool> {
    None
}

#[cfg(target_family = "unix")]
fn install_signal_handler() {
    unsafe extern "C" fn handle_sigint(_: i32) {
        INTERRUPTED_BY_OS.store(true, Ordering::SeqCst);
    }
    unsafe {
        libc::signal(libc::SIGINT, handle_sigint as *const () as usize);
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerToWorker {
    Interrupt,
    SessionEnd,
    #[serde(other)]
    Other,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WorkerToServer {
    WorkerReady {
        protocol: Protocol,
        worker: WorkerIdentity,
        capabilities: Capabilities,
    },
    ReadlineStart {
        prompt: String,
    },
    ReadlineInput {
        text: String,
    },
    ReadlineDiscard {
        text: String,
    },
    OutputText {
        stream: String,
        data_b64: String,
    },
    OutputImage {
        image_id: String,
        mime_type: String,
        data_b64: String,
        update: bool,
    },
    SessionEnd {
        reason: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        message_b64: Option<String>,
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
    writer: Arc<std::sync::Mutex<Box<dyn Write + Send>>>,
}

impl IpcWriter {
    fn new(writer: Box<dyn Write + Send>) -> Self {
        Self {
            writer: Arc::new(std::sync::Mutex::new(writer)),
        }
    }

    fn send<T: Serialize>(&self, message: &T) -> io::Result<()> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| io::Error::other("ipc writer mutex poisoned"))?;
        let payload = serde_json::to_string(message).map_err(io::Error::other)?;
        writer.write_all(payload.as_bytes())?;
        writer.write_all(b"\n")?;
        writer.flush()
    }

    fn output_text(&self, stream: &str, bytes: &[u8]) -> io::Result<()> {
        self.send(&WorkerToServer::OutputText {
            stream: stream.to_string(),
            data_b64: base64::engine::general_purpose::STANDARD.encode(bytes),
        })
    }

    fn send_raw_json(&self, payload: &str) -> io::Result<()> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| io::Error::other("ipc writer mutex poisoned"))?;
        writer.write_all(payload.as_bytes())?;
        writer.write_all(b"\n")?;
        writer.flush()
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
            let read_fd = env_fd(IPC_READ_FD_ENV)?;
            let write_fd = env_fd(IPC_WRITE_FD_ENV)?;
            let reader = unsafe { File::from_raw_fd(read_fd) };
            let writer = unsafe { File::from_raw_fd(write_fd) };
            Ok(Self {
                reader: Box::new(reader),
                writer: Box::new(writer),
            })
        }

        #[cfg(target_family = "windows")]
        {
            let to_worker = std::env::var(IPC_PIPE_TO_WORKER_ENV).map_err(io::Error::other)?;
            let from_worker = std::env::var(IPC_PIPE_FROM_WORKER_ENV).map_err(io::Error::other)?;
            let reader = std::fs::OpenOptions::new().read(true).open(to_worker)?;
            let writer = std::fs::OpenOptions::new().write(true).open(from_worker)?;
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

#[cfg(target_family = "unix")]
fn env_fd(name: &str) -> io::Result<i32> {
    std::env::var(name)
        .map_err(io::Error::other)?
        .parse::<i32>()
        .map_err(io::Error::other)
}

fn start_control_reader(
    reader: Box<dyn Read + Send>,
    interrupted: Arc<AtomicBool>,
    control_session_end: Arc<AtomicBool>,
    shutdown_log_path: Option<PathBuf>,
) {
    thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            let message = serde_json::from_str::<ServerToWorker>(line.trim_end());
            match message {
                Ok(ServerToWorker::Interrupt) => {
                    interrupted.store(true, Ordering::SeqCst);
                }
                Ok(ServerToWorker::SessionEnd) => {
                    control_session_end.store(true, Ordering::SeqCst);
                    let _ =
                        append_shutdown_log(shutdown_log_path.as_deref(), "control_session_end");
                    return;
                }
                Ok(ServerToWorker::Other) | Err(_) => {}
            }
        }
    });
}

const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x60, 0x00, 0x00, 0x00,
    0x02, 0x00, 0x01, 0xe2, 0x21, 0xbc, 0x33, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae,
    0x42, 0x60, 0x82,
];
