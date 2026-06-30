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
    emit_discard_pending_input_ack, emit_input_line, emit_input_wait, emit_output_image,
    emit_output_text, emit_session_end, emit_session_end_with_reason, emit_top_level_input_wait,
    emit_worker_ready, global_ipc, set_global_ipc, worker_ipc_disabled_for_process,
};
pub use protocol::{
    IpcHandlers, IpcInputLineEvent, IpcOutputImage, IpcOutputText, ServerToWorkerIpcMessage,
    WORKER_PROTOCOL_VERSION, WorkerCapabilities, WorkerIdentity, WorkerProtocol,
    WorkerToServerIpcMessage,
};
pub use server_connection::{
    IpcDiscardPendingInputAck, IpcHandle, IpcInputReadiness, IpcWaitError, ServerIpcConnection,
};
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
