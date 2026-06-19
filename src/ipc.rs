#![cfg_attr(
    not(any(target_family = "unix", target_family = "windows")),
    allow(dead_code)
)]
#![allow(unused_imports)]

mod emit;
mod protocol;
mod server_connection;
#[cfg(test)]
pub(crate) mod test_support;
mod transport;
mod worker_connection;

pub use emit::{
    emit_idle, emit_input_line, emit_output_text, emit_plot_image, emit_readline_discard_bytes,
    emit_readline_input_bytes, emit_readline_result, emit_readline_start, emit_session_end,
    emit_session_end_with_reason, emit_worker_ready, global_ipc, set_global_ipc,
    worker_ipc_disabled_for_process,
};
#[cfg(target_family = "unix")]
pub use emit::{emit_pty_feed, emit_stdin_wait};
pub use protocol::{
    IpcEchoEvent, IpcHandlers, IpcOutputText, IpcPlotImage, IpcPtyFeed, IpcPtyFeedHandler,
    ServerToWorkerIpcMessage, WORKER_PROTOCOL_VERSION, WorkerCapabilities, WorkerIdentity,
    WorkerProtocol, WorkerToServerIpcMessage,
};
pub use server_connection::{IpcHandle, IpcWaitError, ServerIpcConnection};
#[cfg(target_family = "windows")]
pub use transport::{IPC_PIPE_FROM_WORKER_ENV, IPC_PIPE_TO_WORKER_ENV};
#[cfg(target_family = "unix")]
pub use transport::{IPC_READ_FD_ENV, IPC_WRITE_FD_ENV};
pub use transport::{IpcServer, connect_from_env};
pub use worker_connection::{OutputCriticalIpcWriter, WorkerIpcConnection};

#[cfg(test)]
pub(crate) use test_support::{test_connection_pair, test_connection_pair_with_handlers};
#[cfg(target_family = "unix")]
pub(crate) use transport::IpcChildFds;
