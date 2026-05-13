use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
#[cfg(target_family = "unix")]
use std::os::unix::io::FromRawFd;
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

#[cfg(target_family = "unix")]
static INTERRUPTED_BY_OS: AtomicBool = AtomicBool::new(false);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_family = "unix")]
    install_signal_handler();

    let transport = IpcTransport::connect_from_env()?;
    let writer = IpcWriter::new(transport.writer);
    let interrupted = Arc::new(AtomicBool::new(false));
    start_control_reader(transport.reader, interrupted.clone());

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
        graceful_shutdown: Some(GracefulShutdown {
            stdin: "exit\n".to_string(),
        }),
    })?;
    writer.send(&WorkerToServer::ReadlineStart {
        prompt: "zod> ".to_string(),
    })?;

    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut line = String::new();
    let mut next_prompt = "zod> ".to_string();
    loop {
        line.clear();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            writer.send(&WorkerToServer::SessionEnd {
                reason: "shutdown".to_string(),
                message_b64: None,
            })?;
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
        if command == "exit" {
            writer.send(&WorkerToServer::SessionEnd {
                reason: "runtime_exit".to_string(),
                message_b64: None,
            })?;
            return Ok(());
        }
        if command == "bad-output-after-session-end" {
            writer.send(&WorkerToServer::SessionEnd {
                reason: "runtime_exit".to_string(),
                message_b64: None,
            })?;
            writer.output_text("stdout", b"late output\n")?;
            return Ok(());
        }
        run_command(&writer, &interrupted, command, &line, &mut next_prompt)?;
        writer.send(&WorkerToServer::ReadlineStart {
            prompt: std::mem::replace(&mut next_prompt, "zod> ".to_string()),
        })?;
    }
}

fn run_command(
    writer: &IpcWriter,
    interrupted: &AtomicBool,
    command: &str,
    raw_line: &str,
    next_prompt: &mut String,
) -> io::Result<()> {
    if let Some(prompt) = command.strip_prefix("wait ") {
        *next_prompt = prompt.to_string();
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

    if let Some(millis) = command.strip_prefix("interruptible ") {
        sleep_for(parse_millis(millis)?, interrupted, true);
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
        writer.send_raw_json(r#"{"type":"output_text","stream":"stdout","data_b64":"***"}"#)?;
        return Ok(());
    }

    writer.output_text("stdout", raw_line.as_bytes())
}

fn parse_millis(raw: &str) -> io::Result<u64> {
    raw.trim()
        .parse::<u64>()
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))
}

fn sleep_for(millis: u64, interrupted: &AtomicBool, interruptible: bool) {
    let deadline = Instant::now() + Duration::from_millis(millis);
    while Instant::now() < deadline {
        if interruptible
            && (interrupted.swap(false, Ordering::SeqCst) || os_interrupted().unwrap_or(false))
        {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
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
        #[serde(skip_serializing_if = "Option::is_none")]
        graceful_shutdown: Option<GracefulShutdown>,
    },
    ReadlineStart {
        prompt: String,
    },
    ReadlineInput {
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

#[derive(Serialize)]
struct GracefulShutdown {
    stdin: String,
}

struct IpcWriter {
    writer: std::sync::Mutex<Box<dyn Write + Send>>,
}

impl IpcWriter {
    fn new(writer: Box<dyn Write + Send>) -> Self {
        Self {
            writer: std::sync::Mutex::new(writer),
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

fn start_control_reader(reader: Box<dyn Read + Send>, interrupted: Arc<AtomicBool>) {
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
                Ok(ServerToWorker::SessionEnd) => return,
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
