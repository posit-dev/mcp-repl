use std::io;

use super::protocol::IpcHandlers;
use super::server_connection::ServerIpcConnection;
use super::transport::IpcTransport;
use super::worker_connection::WorkerIpcConnection;

pub(crate) fn test_connection_pair() -> io::Result<(ServerIpcConnection, WorkerIpcConnection)> {
    test_connection_pair_with_handlers(IpcHandlers::default())
}

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
    server.mark_startup_message_seen_for_tests();
    Ok((server, worker))
}
