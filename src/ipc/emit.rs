use std::io;
use std::sync::OnceLock;

#[cfg(target_family = "unix")]
use std::os::unix::io::RawFd;
#[cfg(target_family = "unix")]
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering as AtomicOrdering};

use base64::Engine as _;

use crate::worker_protocol::TextStream;

use super::protocol::{WorkerToServerIpcMessage, worker_ready_message};
use super::worker_connection::WorkerIpcConnection;

#[cfg(target_family = "unix")]
static WORKER_IPC_ALLOWED: AtomicBool = AtomicBool::new(true);
#[cfg(target_family = "unix")]
static WORKER_IPC_FORK_CLOSE_READ_FD: AtomicI32 = AtomicI32::new(-1);
#[cfg(target_family = "unix")]
static WORKER_IPC_FORK_CLOSE_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

#[cfg(target_family = "unix")]
static WORKER_IPC_ATFORK_REGISTER_RESULT: OnceLock<i32> = OnceLock::new();

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
pub(crate) fn register_worker_ipc_fork_contract(read_fd: RawFd, write_fd: RawFd) -> io::Result<()> {
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

pub fn emit_input_line(turn_id: u64, prompt: &str, text: &str) {
    if let Some(ipc) = global_ipc() {
        let _ = ipc.send(WorkerToServerIpcMessage::InputLine {
            turn_id,
            prompt: prompt.to_string(),
            text: text.to_string(),
        });
    }
}

#[cfg(target_family = "unix")]
pub fn emit_stdin_wait(turn_id: u64, prompt: &str) {
    if let Some(ipc) = global_ipc() {
        let _ = ipc.send(WorkerToServerIpcMessage::StdinWait {
            turn_id,
            prompt: prompt.to_string(),
        });
    }
}

pub fn emit_idle(turn_id: u64, prompt: &str) {
    if let Some(ipc) = global_ipc() {
        let _ = ipc.send(WorkerToServerIpcMessage::Idle {
            turn_id,
            prompt: prompt.to_string(),
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

pub fn emit_worker_ready(worker_name: &str, supports_images: bool) {
    if let Some(ipc) = global_ipc() {
        let _ = ipc.send(worker_ready_message(worker_name, supports_images));
    }
}

pub fn emit_session_end() {
    if let Some(ipc) = global_ipc() {
        let _ = ipc.send(WorkerToServerIpcMessage::SessionEnd {
            reason: None,
            message_b64: None,
            turn_id: None,
        });
    }
}

pub fn emit_session_end_with_reason(reason: &str, turn_id: Option<u64>) {
    if let Some(ipc) = global_ipc() {
        let _ = ipc.send(WorkerToServerIpcMessage::SessionEnd {
            reason: Some(reason.to_string()),
            message_b64: None,
            turn_id,
        });
    }
}
