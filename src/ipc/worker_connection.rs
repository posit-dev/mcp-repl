use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Write};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::Serialize;

use crate::worker_protocol::TextStream;

use super::protocol::{ServerToWorkerIpcMessage, WorkerToServerIpcMessage};
use super::transport::IpcTransport;

const OUTPUT_TEXT_IPC_CHUNK_BYTES: usize = 8 * 1024;

#[derive(Default)]
struct WorkerIpcInbox {
    queue: VecDeque<ServerToWorkerIpcMessage>,
    disconnected: bool,
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

    pub fn send_with_timeout<T>(&self, message: T, timeout: Duration) -> io::Result<()>
    where
        T: Serialize + Send + 'static,
    {
        if timeout.is_zero() {
            return Err(ipc_send_timeout_error());
        }

        let writer = self.writer.clone();
        let (tx, rx) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let result = {
                let mut writer = match writer.lock() {
                    Ok(writer) => writer,
                    Err(_) => {
                        let _ = tx.send(Err(io::Error::other("ipc writer mutex poisoned")));
                        return;
                    }
                };
                write_ipc_message(&mut **writer, &message)
            };
            let _ = tx.send(result);
        });

        match rx.recv_timeout(timeout) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => Err(ipc_send_timeout_error()),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(io::Error::other("ipc writer thread exited unexpectedly"))
            }
        }
    }
}

impl WorkerIpcConnection {
    pub(crate) fn new(transport: IpcTransport) -> io::Result<Self> {
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

fn ipc_send_timeout_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "timed out sending IPC message to worker",
    )
}

#[cfg(test)]
mod tests {
    use super::super::protocol::{IpcHandlers, WorkerToServerIpcMessage};
    use super::super::test_support::test_connection_pair_with_handlers;
    use super::{OUTPUT_TEXT_IPC_CHUNK_BYTES, OutputCriticalIpcWriter};
    use crate::worker_protocol::TextStream;
    use base64::Engine as _;
    use std::io::BufRead;
    use std::sync::{Arc, mpsc};
    use std::thread;
    use std::time::Duration;

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
}
