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
#[cfg(target_family = "windows")]
use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler};

#[cfg(target_family = "unix")]
const IPC_READ_FD_ENV: &str = "MCP_REPL_IPC_READ_FD";
#[cfg(target_family = "unix")]
const IPC_WRITE_FD_ENV: &str = "MCP_REPL_IPC_WRITE_FD";
#[cfg(target_family = "windows")]
const IPC_PIPE_TO_WORKER_ENV: &str = "MCP_REPL_IPC_PIPE_TO_WORKER";
#[cfg(target_family = "windows")]
const IPC_PIPE_FROM_WORKER_ENV: &str = "MCP_REPL_IPC_PIPE_FROM_WORKER";
const STARTUP_PROTOCOL_ERROR_ENV: &str = "MCP_REPL_ZOD_STARTUP_PROTOCOL_ERROR";
const STARTUP_READY_ENV: &str = "MCP_REPL_ZOD_STARTUP_READY";
const CONTROL_LOG_ENV: &str = "MCP_REPL_ZOD_CONTROL_LOG";
const LATE_RAW_MARKER_ENV: &str = "MCP_REPL_ZOD_LATE_RAW_MARKER";
const LATE_STDERR_MARKER_ENV: &str = "MCP_REPL_ZOD_LATE_STDERR_MARKER";
const LATE_SIDEBAND_MARKER_ENV: &str = "MCP_REPL_ZOD_LATE_SIDEBAND_MARKER";
const UTF8_TAIL_RELEASE_ENV: &str = "MCP_REPL_ZOD_UTF8_TAIL_RELEASE";
const STALL_CONTROL_READER_ENV: &str = "MCP_REPL_ZOD_STALL_CONTROL_READER";
const DELAY_READY_AFTER_INTERRUPT_ENV: &str = "MCP_REPL_ZOD_DELAY_READY_AFTER_INTERRUPT_MS";
const DELAY_DISCARD_PENDING_INPUT_ACK_ENV: &str = "MCP_REPL_ZOD_DELAY_DISCARD_PENDING_INPUT_ACK_MS";
const SKIP_DISCARD_PENDING_INPUT_ACK_ENV: &str = "MCP_REPL_ZOD_SKIP_DISCARD_PENDING_INPUT_ACK";
const DISCARD_PENDING_INPUT_PROTOCOL_ERROR_BEFORE_ACK_ENV: &str =
    "MCP_REPL_ZOD_DISCARD_PENDING_INPUT_PROTOCOL_ERROR_BEFORE_ACK";
const PREEMPTIVE_DISCARD_PENDING_INPUT_ACK_MARKER_ENV: &str =
    "MCP_REPL_ZOD_PREEMPTIVE_DISCARD_PENDING_INPUT_ACK_MARKER";
const INVALID_OUTPUT_TEXT_BASE64: &str =
    r#"{"type":"output_text","stream":"stdout","data_b64":"***"}"#;
const LATE_RAW_AFTER_SESSION_END: &[u8] = b"STALE_RAW_AFTER_SESSION_END\n";
const LATE_SIDEBAND_AFTER_SESSION_END: &[u8] = b"STALE_SIDEBAND_AFTER_SESSION_END\n";

#[cfg(any(target_family = "unix", target_family = "windows"))]
static INTERRUPTED_BY_OS: AtomicBool = AtomicBool::new(false);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_family = "unix")]
    install_signal_handler();
    #[cfg(target_family = "windows")]
    install_signal_handler()?;

    let transport = IpcTransport::connect_from_env()?;
    run_worker(transport.reader, IpcWriter::new(transport.writer))
}

fn run_worker(
    sideband_reader: Box<dyn Read + Send>,
    writer: IpcWriter,
) -> Result<(), Box<dyn std::error::Error>> {
    let control_log_path = std::env::var_os(CONTROL_LOG_ENV).map(PathBuf::from);
    let skip_discard_pending_input_ack =
        std::env::var_os(SKIP_DISCARD_PENDING_INPUT_ACK_ENV).is_some();
    let discard_pending_input_protocol_error_before_ack =
        std::env::var_os(DISCARD_PENDING_INPUT_PROTOCOL_ERROR_BEFORE_ACK_ENV).is_some();
    let preemptive_discard_pending_input_ack_marker =
        std::env::var_os(PREEMPTIVE_DISCARD_PENDING_INPUT_ACK_MARKER_ENV).map(PathBuf::from);
    let delay_ready_after_interrupt_ms = std::env::var_os(DELAY_READY_AFTER_INTERRUPT_ENV)
        .map(|value| {
            value
                .to_string_lossy()
                .parse::<u64>()
                .map_err(io::Error::other)
        })
        .transpose()?;
    let delay_discard_pending_input_ack_ms = std::env::var_os(DELAY_DISCARD_PENDING_INPUT_ACK_ENV)
        .map(|value| {
            value
                .to_string_lossy()
                .parse::<u64>()
                .map_err(io::Error::other)
        })
        .transpose()?;
    append_control_log(
        control_log_path.as_deref(),
        &format!("pid {}", std::process::id()),
    )?;
    let sideband_interrupted = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();

    start_stdin_observer(control_log_path.clone());

    writer.send(&WorkerToServer::WorkerReady {
        protocol: Protocol {
            name: "mcp-repl-worker".to_string(),
            version: 9,
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
    if std::env::var_os(STARTUP_READY_ENV).is_some() {
        writer.send(&WorkerToServer::InputWait { prompt: None })?;
        append_control_log(control_log_path.as_deref(), "input_wait prompt=null")?;
    } else {
        writer.send(&WorkerToServer::InputWait {
            prompt: Some("v5> ".to_string()),
        })?;
        append_control_log(control_log_path.as_deref(), "input_wait")?;
    }
    if let Some(stall_millis) = std::env::var_os(STALL_CONTROL_READER_ENV) {
        let stall_millis = stall_millis.to_string_lossy().parse::<u64>()?;
        let _sideband_reader = sideband_reader;
        let _turn_tx = tx;
        append_control_log(control_log_path.as_deref(), "control_reader_stalled")?;
        thread::sleep(Duration::from_millis(stall_millis));
        append_control_log(control_log_path.as_deref(), "control_reader_stall_elapsed")?;
        return Ok(());
    }

    start_control_reader(
        sideband_reader,
        tx,
        writer.clone(),
        sideband_interrupted.clone(),
        ControlReaderConfig {
            control_log_path: control_log_path.clone(),
            skip_discard_pending_input_ack,
            discard_pending_input_protocol_error_before_ack,
            delay_discard_pending_input_ack_ms,
        },
    );

    let mut state = CommandState {
        next_prompt: "v5> ".to_string(),
        previous_line_empty: false,
        input_line_after_input_wait: false,
        session_end_after_input_wait: false,
        bad_output_after_input_wait: None,
        ready_after_turn: false,
        preemptive_discard_pending_input_ack_marker,
    };
    while let Ok(message) = rx.recv() {
        match message {
            ControlMessage::InputBatch { input } => {
                if run_turn(
                    &writer,
                    &sideband_interrupted,
                    &control_log_path,
                    &input,
                    &mut state,
                )? {
                    return Ok(());
                }
            }
            ControlMessage::DiscardPendingInput => {
                if let Some(millis) = delay_ready_after_interrupt_ms {
                    thread::sleep(Duration::from_millis(millis));
                    match rx.try_recv() {
                        Ok(ControlMessage::InputBatch { input }) => {
                            append_control_log(
                                control_log_path.as_deref(),
                                "fresh_input_wait_null_after_discard_suppressed_for_pending_input",
                            )?;
                            if run_turn(
                                &writer,
                                &sideband_interrupted,
                                &control_log_path,
                                &input,
                                &mut state,
                            )? {
                                return Ok(());
                            }
                            continue;
                        }
                        Ok(ControlMessage::Shutdown) => {
                            send_session_end(&writer, "shutdown")?;
                            return Ok(());
                        }
                        Ok(ControlMessage::DiscardPendingInput) => {}
                        Err(mpsc::TryRecvError::Empty) => {}
                        Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
                    }
                    append_control_log(
                        control_log_path.as_deref(),
                        "fresh_input_wait_null_after_discard",
                    )?;
                    writer.send(&WorkerToServer::InputWait { prompt: None })?;
                }
            }
            ControlMessage::Shutdown => {
                send_session_end(&writer, "shutdown")?;
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
    input: &str,
    state: &mut CommandState,
) -> io::Result<bool> {
    for raw_line in runtime_lines(input) {
        let prompt = state.next_prompt.clone();
        writer.send(&WorkerToServer::InputLine {
            prompt: Some(prompt),
            text: raw_line.clone(),
        })?;
        append_control_log(
            control_log_path.as_deref(),
            &format!("input_line text={}", escape_bytes(raw_line.as_bytes())),
        )?;
        let command = raw_line.trim_end_matches(['\r', '\n']);
        if run_command(
            writer,
            sideband_interrupted,
            control_log_path,
            command,
            state,
        )? {
            return Ok(true);
        }
        state.previous_line_empty = command.is_empty();
    }

    if state.ready_after_turn {
        state.ready_after_turn = false;
        writer.send(&WorkerToServer::InputWait { prompt: None })?;
        append_control_log(control_log_path.as_deref(), "input_wait prompt=null")?;
    } else {
        let prompt = std::mem::replace(&mut state.next_prompt, "v5> ".to_string());
        writer.send(&WorkerToServer::InputWait {
            prompt: Some(prompt),
        })?;
        append_control_log(control_log_path.as_deref(), "input_wait")?;
    }
    emit_deferred_protocol_faults(writer, control_log_path, state)?;
    Ok(false)
}

fn run_command(
    writer: &IpcWriter,
    sideband_interrupted: &AtomicBool,
    control_log_path: &Option<PathBuf>,
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
        append_control_log(control_log_path.as_deref(), "bad_output_after_sleep")?;
        writer.send_raw_json(INVALID_OUTPUT_TEXT_BASE64)?;
        sleep_for(5_000, sideband_interrupted, false);
        return Ok(false);
    }

    if let Some(millis) = command.strip_prefix("interrupt-report ") {
        if let Some(marker) = state.preemptive_discard_pending_input_ack_marker.take() {
            let writer = writer.clone();
            let control_log_path = control_log_path.clone();
            thread::spawn(move || {
                let _ = append_control_log(
                    control_log_path.as_deref(),
                    "preemptive_discard_pending_input_ack_waiting",
                );
                if wait_for_marker_path(&marker).is_err() {
                    return;
                }
                let _ = writer.send(&WorkerToServer::DiscardPendingInputAck {
                    discard_id: 1,
                    discarded_input: false,
                });
                let _ = append_control_log(
                    control_log_path.as_deref(),
                    "preemptive_discard_pending_input_ack discard_id=1",
                );
            });
        }
        let report = observe_interrupts_for(parse_millis(millis)?, sideband_interrupted);
        let text = format!(
            "sideband discard: {}\nos interrupt: {}\n",
            if report.sideband {
                "observed"
            } else {
                "missing"
            },
            if report.os { "observed" } else { "missing" },
        );
        output_text(writer, control_log_path, text.as_bytes())?;
        return Ok(false);
    }

    if command == "emit-output-after-input" {
        output_text(writer, control_log_path, b"after input_line\n")?;
        return Ok(false);
    }

    if command == "emit-stderr-after-input" {
        output_stderr_text(writer, control_log_path, b"boom\n")?;
        return Ok(false);
    }

    if command == "partial-stdout" {
        output_text(writer, control_log_path, b"partial")?;
        return Ok(false);
    }

    if command == "partial-stderr" {
        output_stderr_text(writer, control_log_path, b"partial")?;
        return Ok(false);
    }

    if command == "partial-stderr-utf8-then-late-stderr-after-completion" {
        let late_stderr_marker = std::env::var_os(LATE_STDERR_MARKER_ENV)
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, LATE_STDERR_MARKER_ENV))?;
        output_stderr_text_with_continuation(writer, control_log_path, &[0xC3], false)?;
        state.ready_after_turn = true;
        let writer = writer.clone();
        let control_log_path = control_log_path.clone();
        thread::spawn(move || {
            let _ = append_control_log(control_log_path.as_deref(), "waiting_late_stderr_marker");
            if wait_for_marker_path(&late_stderr_marker).is_err() {
                return;
            }
            let _ = output_stderr_text(&writer, &control_log_path, b"after\n");
            let _ = append_control_log(control_log_path.as_deref(), "late_stderr_after_completion");
        });
        return Ok(false);
    }

    if command == "partial-stdout-then-newline-stderr" {
        output_text(writer, control_log_path, b"partial")?;
        output_stderr_text(writer, control_log_path, b"\nerr\n")?;
        return Ok(false);
    }

    if command == "partial-utf8-then-exit" {
        output_text_with_continuation(writer, control_log_path, &[0xC3], false)?;
        send_session_end(writer, "runtime_exit")?;
        return Ok(true);
    }

    if command == "split-utf8-interleaved-stderr" {
        output_text_with_continuation(writer, control_log_path, &[0xC3], false)?;
        output_stderr_text(writer, control_log_path, b"err\n")?;
        output_text_with_continuation(writer, control_log_path, &[0xA9], true)?;
        return Ok(false);
    }

    if command == "split-utf8-before-image" {
        output_text_with_continuation(writer, control_log_path, &[0xC3], false)?;
        output_image(writer, control_log_path, b"img")?;
        output_text_with_continuation(writer, control_log_path, &[0xA9], true)?;
        return Ok(false);
    }

    if command == "split-utf8-before-delayed-image" {
        output_text_with_continuation(writer, control_log_path, &[0xC3], false)?;
        output_image(writer, control_log_path, b"img")?;
        sleep_for(200, sideband_interrupted, false);
        output_text_with_continuation(writer, control_log_path, &[0xA9], true)?;
        return Ok(false);
    }

    if command == "split-utf8-after-completion" {
        output_text_with_continuation(writer, control_log_path, &[0xC3], false)?;
        output_image(writer, control_log_path, b"img")?;
        state.ready_after_turn = true;
        let writer = writer.clone();
        let control_log_path = control_log_path.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(40));
            let _ = output_text_with_continuation(&writer, &control_log_path, &[0xA9, b'\n'], true);
        });
        return Ok(false);
    }

    if command == "split-utf8-then-more-after-completion" {
        output_text_with_continuation(writer, control_log_path, &[0xC3], false)?;
        state.ready_after_turn = true;
        let writer = writer.clone();
        let control_log_path = control_log_path.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            let _ = output_text_with_continuation(&writer, &control_log_path, &[0xA9], true);
            thread::sleep(Duration::from_millis(30));
            let _ = output_text_with_continuation(&writer, &control_log_path, b" after\n", false);
        });
        return Ok(false);
    }

    if command == "split-utf8-then-continuous-output-after-completion" {
        output_text_with_continuation(writer, control_log_path, &[0xC3], false)?;
        state.ready_after_turn = true;
        let writer = writer.clone();
        let control_log_path = control_log_path.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            let _ = output_text_with_continuation(&writer, &control_log_path, &[0xA9], true);
            for _ in 0..150 {
                thread::sleep(Duration::from_millis(10));
                let _ = output_text_with_continuation(&writer, &control_log_path, b".", false);
            }
        });
        return Ok(false);
    }

    if command == "partial-utf8-stderr-then-sleep" {
        output_text_with_continuation(writer, control_log_path, &[0xC3], false)?;
        output_stderr_text(writer, control_log_path, b"tail-visible\n")?;
        sleep_for(1000, sideband_interrupted, false);
        return Ok(false);
    }

    if command == "partial-utf8-then-sleep" {
        output_text_with_continuation(writer, control_log_path, &[0xC3], false)?;
        sleep_for(200, sideband_interrupted, false);
        return Ok(false);
    }

    if command == "partial-utf8-then-wait-for-release" {
        output_text_with_continuation(writer, control_log_path, &[0xC3], false)?;
        append_control_log(control_log_path.as_deref(), "waiting_utf8_tail_release")?;
        wait_for_marker(UTF8_TAIL_RELEASE_ENV)?;
        output_text_with_continuation(writer, control_log_path, &[0xA9], true)?;
        return Ok(false);
    }

    if command == "raw-split-utf8-around-input-wait" {
        io::stdout().write_all(&[0xC3])?;
        io::stdout().flush()?;
        sleep_for(50, sideband_interrupted, false);
        writer.send(&WorkerToServer::InputWait {
            prompt: Some("v5> ".to_string()),
        })?;
        append_control_log(control_log_path.as_deref(), "input_wait")?;
        sleep_for(200, sideband_interrupted, false);
        io::stdout().write_all(&[0xA9, b'\n'])?;
        io::stdout().flush()?;
        return Ok(false);
    }

    if let Some(len) = command.strip_prefix("output-image-bytes ") {
        let len: usize = parse_millis(len)?.try_into().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "output-image-bytes too large")
        })?;
        output_image(writer, control_log_path, &vec![b'i'; len])?;
        return Ok(false);
    }

    if command == "output-source-image" {
        output_source_image(writer, control_log_path, b"img", "zod-source")?;
        return Ok(false);
    }

    if command == "output-image-update-with-tail" {
        output_source_image_update(writer, control_log_path, b"updated-img", "zod-source")?;
        output_text(writer, control_log_path, &vec![b'z'; 2_000])?;
        return Ok(false);
    }

    if let Some(len) = command.strip_prefix("repeat-output ") {
        let len: usize = parse_millis(len)?
            .try_into()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "repeat-output too large"))?;
        let mut text = String::with_capacity(len.saturating_add(32));
        text.push_str("ZOD_BEGIN\n");
        text.push_str(&"z".repeat(len));
        text.push_str("\nZOD_END\n");
        output_text(writer, control_log_path, text.as_bytes())?;
        return Ok(false);
    }

    if command == "pager-refresh-input-echo" {
        let mut text = String::with_capacity(10_032);
        text.push_str("ZOD_REFRESH_BEGIN\n");
        text.push_str(&"z".repeat(10_000));
        text.push_str("\nZOD_REFRESH_FIRST_END\n");
        output_text(writer, control_log_path, text.as_bytes())?;
        sleep_for(200, sideband_interrupted, false);
        writer.send(&WorkerToServer::InputLine {
            prompt: Some("v5> ".to_string()),
            text: "refreshed-hidden-echo\n".to_string(),
        })?;
        append_control_log(control_log_path.as_deref(), "refresh_pager_input_line")?;
        output_text(writer, control_log_path, b"ZOD_REFRESH_TAIL\n")?;
        append_control_log(control_log_path.as_deref(), "refresh_pager_tail")?;
        return Ok(false);
    }

    if command.starts_with("silent ") {
        return Ok(false);
    }

    if command == "output-matching-input-line" {
        output_text(
            writer,
            control_log_path,
            b"v5> output-matching-input-line\nVISIBLE\n",
        )?;
        return Ok(false);
    }

    if command == "late-input-line-after-input-wait" {
        state.input_line_after_input_wait = true;
        return Ok(false);
    }

    if command == "session-end-after-input-wait" {
        state.session_end_after_input_wait = true;
        return Ok(false);
    }

    if command == "session-end-park" {
        send_session_end(writer, "runtime_exit")?;
        append_control_log(control_log_path.as_deref(), "park_after_session_end")?;
        loop {
            thread::park();
        }
    }

    if command == "session-end-raw-after-marker" {
        ignore_sigterm_for_late_raw_test();
        send_session_end(writer, "runtime_exit")?;
        append_control_log(control_log_path.as_deref(), "waiting_late_raw_marker")?;
        wait_for_marker(LATE_RAW_MARKER_ENV)?;
        io::stdout().write_all(LATE_RAW_AFTER_SESSION_END)?;
        io::stdout().flush()?;
        append_control_log(
            control_log_path.as_deref(),
            "late_raw_stdout_after_session_end",
        )?;
        loop {
            thread::park();
        }
    }

    if command == "session-end-sideband-after-marker" {
        ignore_sigterm_for_late_raw_test();
        send_session_end(writer, "runtime_exit")?;
        append_control_log(control_log_path.as_deref(), "waiting_late_sideband_marker")?;
        wait_for_marker(LATE_SIDEBAND_MARKER_ENV)?;
        output_text(writer, control_log_path, LATE_SIDEBAND_AFTER_SESSION_END)?;
        append_control_log(
            control_log_path.as_deref(),
            "late_sideband_output_after_session_end",
        )?;
        loop {
            thread::park();
        }
    }

    if command == "write-session-temp-marker" {
        let session_tmpdir =
            std::env::var("MCP_REPL_R_SESSION_TMPDIR").map_err(io::Error::other)?;
        let marker = PathBuf::from(session_tmpdir).join("respawn-marker.txt");
        std::fs::write(&marker, b"respawned worker marker")?;
        let text = format!("session-temp-marker: {}\n", marker.display());
        output_text(writer, control_log_path, text.as_bytes())?;
        return Ok(false);
    }

    if let Some(millis) = command.strip_prefix("bad-output-after-input-wait ") {
        state.bad_output_after_input_wait = Some(Duration::from_millis(parse_millis(millis)?));
        return Ok(false);
    }

    if command == "exit" {
        send_session_end(writer, "runtime_exit")?;
        return Ok(true);
    }

    let text = format!("v5-output: {command}\n");
    output_text(writer, control_log_path, text.as_bytes())?;
    Ok(false)
}

fn emit_deferred_protocol_faults(
    writer: &IpcWriter,
    control_log_path: &Option<PathBuf>,
    state: &mut CommandState,
) -> io::Result<()> {
    if state.input_line_after_input_wait {
        state.input_line_after_input_wait = false;
        append_control_log(control_log_path.as_deref(), "late_input_line")?;
        writer.send(&WorkerToServer::InputLine {
            prompt: Some("v5> ".to_string()),
            text: "late\n".to_string(),
        })?;
    }
    if state.session_end_after_input_wait {
        state.session_end_after_input_wait = false;
        append_control_log(control_log_path.as_deref(), "late_session_end")?;
        send_session_end(writer, "runtime_exit")?;
    }
    if let Some(delay) = state.bad_output_after_input_wait.take() {
        let writer = writer.clone();
        let control_log_path = control_log_path.clone();
        thread::spawn(move || {
            thread::sleep(delay);
            let _ = append_control_log(control_log_path.as_deref(), "late_bad_output");
            let _ = writer.send_raw_json(INVALID_OUTPUT_TEXT_BASE64);
        });
    }
    Ok(())
}

fn output_text(
    writer: &IpcWriter,
    control_log_path: &Option<PathBuf>,
    bytes: &[u8],
) -> io::Result<()> {
    append_control_log(control_log_path.as_deref(), "output_text")?;
    writer.output_text("stdout", bytes)
}

fn output_text_with_continuation(
    writer: &IpcWriter,
    control_log_path: &Option<PathBuf>,
    bytes: &[u8],
    is_continuation: bool,
) -> io::Result<()> {
    append_control_log(control_log_path.as_deref(), "output_text")?;
    writer.output_text_with_continuation("stdout", bytes, is_continuation)
}

fn output_stderr_text(
    writer: &IpcWriter,
    control_log_path: &Option<PathBuf>,
    bytes: &[u8],
) -> io::Result<()> {
    append_control_log(control_log_path.as_deref(), "output_text stderr")?;
    writer.output_text("stderr", bytes)
}

fn output_stderr_text_with_continuation(
    writer: &IpcWriter,
    control_log_path: &Option<PathBuf>,
    bytes: &[u8],
    is_continuation: bool,
) -> io::Result<()> {
    append_control_log(control_log_path.as_deref(), "output_text stderr")?;
    writer.output_text_with_continuation("stderr", bytes, is_continuation)
}

fn output_image(
    writer: &IpcWriter,
    control_log_path: &Option<PathBuf>,
    bytes: &[u8],
) -> io::Result<()> {
    append_control_log(control_log_path.as_deref(), "output_image")?;
    writer.output_image("image/png", bytes)
}

fn output_source_image(
    writer: &IpcWriter,
    control_log_path: &Option<PathBuf>,
    bytes: &[u8],
    source: &str,
) -> io::Result<()> {
    append_control_log(control_log_path.as_deref(), "output_source_image")?;
    writer.output_image_with_source("image/png", bytes, false, Some(source))
}

fn output_source_image_update(
    writer: &IpcWriter,
    control_log_path: &Option<PathBuf>,
    bytes: &[u8],
    source: &str,
) -> io::Result<()> {
    append_control_log(control_log_path.as_deref(), "output_image_update")?;
    writer.output_image_with_source("image/png", bytes, true, Some(source))
}

fn send_session_end(writer: &IpcWriter, reason: &str) -> io::Result<()> {
    writer.send(&WorkerToServer::SessionEnd {
        reason: reason.to_string(),
        message: None,
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
    input_line_after_input_wait: bool,
    session_end_after_input_wait: bool,
    bad_output_after_input_wait: Option<Duration>,
    ready_after_turn: bool,
    preemptive_discard_pending_input_ack_marker: Option<PathBuf>,
}

struct ControlReaderConfig {
    control_log_path: Option<PathBuf>,
    skip_discard_pending_input_ack: bool,
    discard_pending_input_protocol_error_before_ack: bool,
    delay_discard_pending_input_ack_ms: Option<u64>,
}

fn start_control_reader(
    reader: Box<dyn Read + Send>,
    turn_tx: mpsc::Sender<ControlMessage>,
    writer: IpcWriter,
    interrupted: Arc<AtomicBool>,
    config: ControlReaderConfig,
) {
    thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        let ControlReaderConfig {
            control_log_path,
            skip_discard_pending_input_ack,
            discard_pending_input_protocol_error_before_ack,
            delay_discard_pending_input_ack_ms,
        } = config;
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
                Ok(ServerToWorker::InputBatch { input }) => {
                    let _ = append_control_log(
                        control_log_path.as_deref(),
                        &format!("input_batch input={}", escape_bytes(input.as_bytes())),
                    );
                    let _ = turn_tx.send(ControlMessage::InputBatch { input });
                }
                Ok(ServerToWorker::DiscardPendingInput { discard_id }) => {
                    interrupted.store(true, Ordering::SeqCst);
                    let _ = append_control_log(
                        control_log_path.as_deref(),
                        &format!("discard_pending_input discard_id={discard_id}"),
                    );
                    let notify_before_ack = delay_discard_pending_input_ack_ms.is_some();
                    if notify_before_ack {
                        let _ = turn_tx.send(ControlMessage::DiscardPendingInput);
                    }
                    if discard_pending_input_protocol_error_before_ack {
                        let _ = append_control_log(
                            control_log_path.as_deref(),
                            "discard_pending_input_protocol_error_before_ack",
                        );
                        let _ = writer.send_raw_json(INVALID_OUTPUT_TEXT_BASE64);
                    }
                    if skip_discard_pending_input_ack {
                        let _ = append_control_log(
                            control_log_path.as_deref(),
                            "discard_pending_input_ack_suppressed",
                        );
                    } else {
                        if let Some(millis) = delay_discard_pending_input_ack_ms {
                            thread::sleep(Duration::from_millis(millis));
                        }
                        let _ = writer.send(&WorkerToServer::DiscardPendingInputAck {
                            discard_id,
                            discarded_input: false,
                        });
                        let _ = append_control_log(
                            control_log_path.as_deref(),
                            &format!(
                                "discard_pending_input_ack discard_id={discard_id} discarded_input=false"
                            ),
                        );
                    }
                    if !notify_before_ack {
                        let _ = turn_tx.send(ControlMessage::DiscardPendingInput);
                    }
                }
                Ok(ServerToWorker::Shutdown {}) => {
                    let _ = turn_tx.send(ControlMessage::Shutdown);
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
    InputBatch { input: String },
    DiscardPendingInput,
    Shutdown,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerToWorker {
    InputBatch { input: String },
    DiscardPendingInput { discard_id: u64 },
    Shutdown {},
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
        #[serde(default, skip_serializing_if = "is_false")]
        is_continuation: bool,
    },
    OutputImage {
        mime_type: String,
        data_b64: String,
        is_update: bool,
        source: Option<String>,
    },
    InputLine {
        prompt: Option<String>,
        text: String,
    },
    InputWait {
        prompt: Option<String>,
    },
    DiscardPendingInputAck {
        discard_id: u64,
        discarded_input: bool,
    },
    SessionEnd {
        reason: String,
        message: Option<String>,
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
        self.output_text_with_continuation(stream, bytes, false)
    }

    fn output_text_with_continuation(
        &self,
        stream: &str,
        bytes: &[u8],
        is_continuation: bool,
    ) -> io::Result<()> {
        self.send(&WorkerToServer::OutputText {
            stream: stream.to_string(),
            data_b64: base64::engine::general_purpose::STANDARD.encode(bytes),
            is_continuation,
        })
    }

    fn output_image(&self, mime_type: &str, bytes: &[u8]) -> io::Result<()> {
        self.output_image_with_source(mime_type, bytes, false, None)
    }

    fn output_image_with_source(
        &self,
        mime_type: &str,
        bytes: &[u8],
        is_update: bool,
        source: Option<&str>,
    ) -> io::Result<()> {
        self.send(&WorkerToServer::OutputImage {
            mime_type: mime_type.to_string(),
            data_b64: base64::engine::general_purpose::STANDARD.encode(bytes),
            is_update,
            source: source.map(str::to_string),
        })
    }
}

fn is_false(value: &bool) -> bool {
    !*value
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
        let observed_os = take_os_interrupt();
        if observed_os && !os {
            let control_log_path = std::env::var_os(CONTROL_LOG_ENV).map(PathBuf::from);
            let _ = append_control_log(control_log_path.as_deref(), "os_interrupt_observed");
        }
        os |= observed_os;
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

fn wait_for_marker(env_name: &str) -> io::Result<()> {
    let marker = std::env::var_os(env_name)
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, env_name))?;
    wait_for_marker_path(&marker)
}

fn wait_for_marker_path(marker: &Path) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if marker.exists() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("timed out waiting for {}", marker.display()),
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(target_family = "unix")]
fn ignore_sigterm_for_late_raw_test() {
    unsafe {
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
    }
}

#[cfg(not(target_family = "unix"))]
fn ignore_sigterm_for_late_raw_test() {}

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

#[cfg(target_family = "windows")]
fn install_signal_handler() -> io::Result<()> {
    let ok = unsafe { SetConsoleCtrlHandler(Some(handle_console_ctrl), 1) };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(target_family = "windows")]
unsafe extern "system" fn handle_console_ctrl(event: u32) -> i32 {
    if event == CTRL_BREAK_EVENT || event == CTRL_C_EVENT {
        INTERRUPTED_BY_OS.store(true, Ordering::SeqCst);
        1
    } else {
        0
    }
}

#[cfg(target_family = "windows")]
fn take_os_interrupt() -> bool {
    INTERRUPTED_BY_OS.swap(false, Ordering::SeqCst)
}

#[cfg(not(any(target_family = "unix", target_family = "windows")))]
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
