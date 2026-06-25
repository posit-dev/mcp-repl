use crate::ipc;
use crate::worker_protocol::TextStream;

use super::CStdinLine;
use super::emit_output_text;
use super::state::StdinReadAccounting;

pub(super) fn set_runtime_stdin_fd(_fd: libc::c_int) {}

pub(super) fn flush_terminal_input() {
    let _ = unsafe { libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH) };
}

pub(super) fn emit_protocol_failure(message: &str) {
    emit_output_text(TextStream::Stderr, message.as_bytes());
    ipc::emit_session_end();
}

pub(super) fn note_cpython_readline_bytes_read(
    _prompt: &str,
    _bytes: &[u8],
) -> Result<StdinReadAccounting, String> {
    Ok(StdinReadAccounting::Accounted)
}

pub(super) fn note_stdin_line_read(
    _prompt: &str,
    _bytes: &[u8],
) -> Result<StdinReadAccounting, String> {
    Ok(StdinReadAccounting::Accounted)
}

pub(super) fn fork_child_stdin_eof(prompt: &str) -> CStdinLine {
    // Fork children inherit fd 0/1/2, but mcp-repl sideband IPC is deliberately
    // disabled in the at-fork child handler. Managed input lives in the parent
    // worker queue, so IPC-disabled children must see EOF for stdin.
    if !prompt.is_empty() {
        emit_output_text(TextStream::Stdout, prompt.as_bytes());
    }
    CStdinLine::Eof
}
