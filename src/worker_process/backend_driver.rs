use std::time::{Duration, Instant};

use crate::backend::{Backend, WorkerLaunch};
use crate::completion_reply::CompletionInfo;
use crate::ipc::{IpcWaitError, ServerIpcConnection, ServerToWorkerIpcMessage};
use crate::output_capture::OutputTextSource;
use crate::oversized_output::OversizedOutputMode;
use crate::worker_supervisor::WorkerProcess;

use super::WorkerError;
use super::request_lifecycle::{REQUEST_COMPLETION_STABLE_WAIT, completion_info_from_ipc};

pub(super) fn output_echo_source_for_backend(backend: Backend) -> OutputTextSource {
    match backend {
        Backend::R => OutputTextSource::Ipc,
        Backend::Python => OutputTextSource::Ipc,
    }
}

pub(super) trait BackendDriver: Send {
    fn prepare_input_text(&self, text: String) -> String {
        text
    }

    fn on_input_start(
        &mut self,
        text: &str,
        ipc: &ServerIpcConnection,
        timeout: Duration,
    ) -> Result<(), WorkerError>;

    fn should_settle_output_after_timeout(
        &self,
        oversized_output: OversizedOutputMode,
        pending_input: Option<&str>,
    ) -> bool;

    fn clear_active_input(&mut self) {}

    fn wait_for_completion(
        &mut self,
        timeout: Duration,
        ipc: ServerIpcConnection,
    ) -> Result<CompletionInfo, WorkerError>;

    fn interrupt(&mut self, process: &mut WorkerProcess) -> Result<(), WorkerError>;
}

pub(super) fn new_backend_driver(worker_launch: &WorkerLaunch) -> Box<dyn BackendDriver> {
    match worker_launch {
        WorkerLaunch::Builtin(Backend::R) => Box::new(RBackendDriver::new()),
        WorkerLaunch::Builtin(Backend::Python) => Box::new(ProtocolBackendDriver::builtin_python()),
        WorkerLaunch::Custom(_) => Box::new(ProtocolBackendDriver::new()),
    }
}

struct RBackendDriver;

impl RBackendDriver {
    fn new() -> Self {
        Self
    }
}

fn driver_wait_for_completion(
    timeout: Duration,
    ipc: ServerIpcConnection,
    echo_source: OutputTextSource,
) -> Result<CompletionInfo, WorkerError> {
    if timeout.is_zero() {
        return Err(WorkerError::Timeout(timeout));
    }
    match ipc.wait_for_request_completion(timeout, REQUEST_COMPLETION_STABLE_WAIT) {
        Ok(()) => Ok(completion_info_from_ipc(&ipc, false, echo_source)),
        Err(IpcWaitError::Timeout) => Err(WorkerError::Timeout(timeout)),
        Err(IpcWaitError::SessionEnd) => Ok(completion_info_from_ipc(&ipc, true, echo_source)),
        Err(IpcWaitError::Disconnected) => Err(WorkerError::Protocol(
            "ipc disconnected while waiting for request completion".to_string(),
        )),
        Err(IpcWaitError::Protocol(message)) => Err(WorkerError::Protocol(message)),
    }
}

#[cfg(not(any(target_family = "unix", target_family = "windows")))]
fn driver_interrupt(process: &mut WorkerProcess) -> Result<(), WorkerError> {
    if let Some(ipc) = process.ipc_connection() {
        ipc.note_interrupt_sent();
        let _ = ipc.send(ServerToWorkerIpcMessage::Interrupt {});
    }
    process.send_interrupt()
}

fn normalize_input_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn send_worker_ipc_with_timeout(
    ipc: &ServerIpcConnection,
    message: ServerToWorkerIpcMessage,
    timeout: Duration,
) -> Result<(), WorkerError> {
    match ipc.send_with_timeout(message, timeout) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::TimedOut => {
            Err(WorkerError::Timeout(timeout))
        }
        Err(err) => Err(WorkerError::Io(err)),
    }
}

fn begin_input_when_ready(ipc: &ServerIpcConnection, timeout: Duration) -> Result<(), WorkerError> {
    match ipc.begin_input_when_ready(timeout) {
        Ok(()) => Ok(()),
        Err(IpcWaitError::Timeout) => Err(WorkerError::Timeout(timeout)),
        Err(IpcWaitError::SessionEnd) => Err(WorkerError::Protocol(
            "worker session ended before input_wait".to_string(),
        )),
        Err(IpcWaitError::Disconnected) => Err(WorkerError::Protocol(
            "ipc disconnected while waiting for worker input_wait".to_string(),
        )),
        Err(IpcWaitError::Protocol(message)) => Err(WorkerError::Protocol(message)),
    }
}

impl BackendDriver for RBackendDriver {
    fn prepare_input_text(&self, text: String) -> String {
        normalize_input_newlines(&text)
    }

    fn on_input_start(
        &mut self,
        text: &str,
        ipc: &ServerIpcConnection,
        timeout: Duration,
    ) -> Result<(), WorkerError> {
        if let Some(message) = ipc.take_protocol_error() {
            return Err(WorkerError::Protocol(message));
        }
        let start = Instant::now();
        begin_input_when_ready(ipc, timeout)?;
        let remaining = timeout.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            return Err(WorkerError::Timeout(timeout));
        }
        send_worker_ipc_with_timeout(
            ipc,
            ServerToWorkerIpcMessage::InputBatch {
                input: text.to_string(),
            },
            remaining,
        )?;
        if let Some(message) = ipc.take_protocol_error() {
            return Err(WorkerError::Protocol(message));
        }
        Ok(())
    }

    fn clear_active_input(&mut self) {}

    fn should_settle_output_after_timeout(
        &self,
        oversized_output: OversizedOutputMode,
        pending_input: Option<&str>,
    ) -> bool {
        if !matches!(oversized_output, OversizedOutputMode::Files) {
            return false;
        }
        pending_input
            .map(|input| input.trim_end_matches(['\r', '\n']).contains('\n'))
            .unwrap_or(false)
    }

    fn wait_for_completion(
        &mut self,
        timeout: Duration,
        ipc: ServerIpcConnection,
    ) -> Result<CompletionInfo, WorkerError> {
        driver_wait_for_completion(timeout, ipc, OutputTextSource::Ipc)
    }

    fn interrupt(&mut self, process: &mut WorkerProcess) -> Result<(), WorkerError> {
        if let Some(ipc) = process.ipc_connection() {
            ipc.note_interrupt_sent();
            let _ = ipc.send(ServerToWorkerIpcMessage::Interrupt {});
        }
        process.send_r_interrupt()
    }
}

struct ProtocolBackendDriver;

impl ProtocolBackendDriver {
    fn new() -> Self {
        Self
    }

    fn builtin_python() -> Self {
        Self
    }
}

impl BackendDriver for ProtocolBackendDriver {
    fn on_input_start(
        &mut self,
        text: &str,
        ipc: &ServerIpcConnection,
        timeout: Duration,
    ) -> Result<(), WorkerError> {
        if let Some(message) = ipc.take_protocol_error() {
            return Err(WorkerError::Protocol(message));
        }
        let start = Instant::now();
        begin_input_when_ready(ipc, timeout)?;
        let remaining = timeout.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            return Err(WorkerError::Timeout(timeout));
        }
        send_worker_ipc_with_timeout(
            ipc,
            ServerToWorkerIpcMessage::InputBatch {
                input: text.to_string(),
            },
            remaining,
        )?;
        if let Some(message) = ipc.take_protocol_error() {
            return Err(WorkerError::Protocol(message));
        }
        Ok(())
    }

    fn should_settle_output_after_timeout(
        &self,
        _oversized_output: OversizedOutputMode,
        _pending_input: Option<&str>,
    ) -> bool {
        false
    }

    fn clear_active_input(&mut self) {}

    fn wait_for_completion(
        &mut self,
        timeout: Duration,
        ipc: ServerIpcConnection,
    ) -> Result<CompletionInfo, WorkerError> {
        driver_wait_for_completion(timeout, ipc, OutputTextSource::Ipc)
    }

    fn interrupt(&mut self, process: &mut WorkerProcess) -> Result<(), WorkerError> {
        if let Some(ipc) = process.ipc_connection() {
            ipc.note_interrupt_sent();
            ipc.send(ServerToWorkerIpcMessage::Interrupt {})
                .map_err(WorkerError::Io)?;
        }
        process.send_interrupt()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::ipc::{ServerToWorkerIpcMessage, WorkerToServerIpcMessage};
    use crate::output_capture::OutputTextSource;

    use super::*;

    fn make_ready_for_input(
        server: &ServerIpcConnection,
        worker: &crate::ipc::WorkerIpcConnection,
        prompt: &str,
    ) {
        worker
            .send(WorkerToServerIpcMessage::InputWait {
                prompt: prompt.to_string(),
            })
            .expect("send input_wait");
        server
            .wait_for_input_wait(Duration::from_millis(200))
            .expect("server observes input_wait");
    }

    #[test]
    fn r_driver_sends_input_batch() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");
        let mut driver = RBackendDriver::new();
        make_ready_for_input(&server, &worker, "> ");

        driver
            .on_input_start("1+1", &server, Duration::from_millis(200))
            .expect("R input_batch should send");

        assert!(matches!(
            worker.recv(Some(Duration::from_millis(200))),
            Some(ServerToWorkerIpcMessage::InputBatch { input }) if input == "1+1"
        ));
    }

    #[test]
    fn completion_uses_input_line_and_input_wait() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");
        make_ready_for_input(&server, &worker, "> ");
        server.begin_input().expect("begin input");
        let _ = worker.send(WorkerToServerIpcMessage::InputLine {
            prompt: "> ".to_string(),
            text: "1+1\n".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::InputWait {
            prompt: "> ".to_string(),
        });

        let completion =
            driver_wait_for_completion(Duration::from_millis(200), server, OutputTextSource::Ipc)
                .expect("expected explicit input_wait to complete request");

        assert_eq!(completion.prompt.as_deref(), Some("> "));
        assert_eq!(completion.echo_events.len(), 1);
        assert_eq!(completion.echo_events[0].prompt, "> ");
        assert_eq!(completion.echo_events[0].line, "1+1\n");
    }

    #[test]
    fn completion_uses_input_wait_prompt() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");
        make_ready_for_input(&server, &worker, "> ");
        server.begin_input().expect("begin input");
        let _ = worker.send(WorkerToServerIpcMessage::InputWait {
            prompt: "debug> ".to_string(),
        });

        let completion =
            driver_wait_for_completion(Duration::from_millis(200), server, OutputTextSource::Ipc)
                .expect("expected input_wait to complete request");
        assert_eq!(completion.prompt.as_deref(), Some("debug> "));
    }

    #[test]
    fn protocol_driver_sends_fresh_turn_after_input_wait() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");
        let mut driver = ProtocolBackendDriver::new();
        make_ready_for_input(&server, &worker, ">>> ");

        driver
            .on_input_start("answer = input('p> ')", &server, Duration::from_millis(200))
            .expect("input_batch should send");
        assert!(matches!(
            worker.recv(Some(Duration::from_millis(200))),
            Some(ServerToWorkerIpcMessage::InputBatch { input }) if input == "answer = input('p> ')"
        ));
        worker
            .send(WorkerToServerIpcMessage::InputWait {
                prompt: "p> ".to_string(),
            })
            .expect("send input_wait");
        let completion = driver
            .wait_for_completion(Duration::from_millis(200), server.clone())
            .expect("input_wait should complete reply");
        assert_eq!(completion.prompt.as_deref(), Some("p> "));

        driver
            .on_input_start("ok\n", &server, Duration::from_millis(200))
            .expect("next input_batch should send");
        assert!(matches!(
            worker.recv(Some(Duration::from_millis(200))),
            Some(ServerToWorkerIpcMessage::InputBatch { input }) if input == "ok\n"
        ));
    }

    #[test]
    fn r_driver_sends_fresh_turn_after_input_wait() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");
        let mut driver = RBackendDriver::new();
        make_ready_for_input(&server, &worker, "> ");

        driver
            .on_input_start(
                "answer <- readline('p> ')",
                &server,
                Duration::from_millis(200),
            )
            .expect("input_batch should send");
        assert!(matches!(
            worker.recv(Some(Duration::from_millis(200))),
            Some(ServerToWorkerIpcMessage::InputBatch { input }) if input == "answer <- readline('p> ')"
        ));
        worker
            .send(WorkerToServerIpcMessage::InputWait {
                prompt: "p> ".to_string(),
            })
            .expect("send input_wait");
        let completion = driver
            .wait_for_completion(Duration::from_millis(200), server.clone())
            .expect("input_wait should complete reply");
        assert_eq!(completion.prompt.as_deref(), Some("p> "));

        driver
            .on_input_start("ok\n", &server, Duration::from_millis(200))
            .expect("next input_batch should send");
        assert!(matches!(
            worker.recv(Some(Duration::from_millis(200))),
            Some(ServerToWorkerIpcMessage::InputBatch { input }) if input == "ok\n"
        ));
    }

    #[test]
    fn next_request_result_is_retained_after_explicit_input_wait() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");

        make_ready_for_input(&server, &worker, "> ");
        server.begin_input().expect("begin first input");
        let _ = worker.send(WorkerToServerIpcMessage::InputWait {
            prompt: "> ".to_string(),
        });
        let first = driver_wait_for_completion(
            Duration::from_millis(200),
            server.clone(),
            OutputTextSource::Ipc,
        )
        .expect("expected first completion");
        assert_eq!(first.prompt.as_deref(), Some("> "));

        server.begin_input().expect("begin second input");
        let _ = worker.send(WorkerToServerIpcMessage::InputLine {
            prompt: "> ".to_string(),
            text: "second()\n".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::InputWait {
            prompt: "> ".to_string(),
        });

        let second =
            driver_wait_for_completion(Duration::from_millis(200), server, OutputTextSource::Ipc)
                .expect("expected second completion");

        assert!(second.protocol_warnings.is_empty());
        assert_eq!(second.echo_events.len(), 1);
        assert_eq!(second.echo_events[0].prompt, "> ");
        assert_eq!(second.echo_events[0].line, "second()\n");
    }

    #[test]
    fn completion_preserves_echo_events_before_input_wait() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");

        make_ready_for_input(&server, &worker, "> ");
        server.begin_input().expect("begin input");
        let _ = worker.send(WorkerToServerIpcMessage::InputLine {
            prompt: "> ".to_string(),
            text: "first()\n".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::InputWait {
            prompt: "> ".to_string(),
        });

        let completion =
            driver_wait_for_completion(Duration::from_millis(200), server, OutputTextSource::Ipc)
                .expect("expected completion after input_wait");

        assert_eq!(completion.prompt.as_deref(), Some("> "));
        assert!(completion.protocol_warnings.is_empty());
        assert_eq!(completion.echo_events.len(), 1);
        assert_eq!(completion.echo_events[0].prompt, "> ");
        assert_eq!(completion.echo_events[0].line, "first()\n");
    }

    #[test]
    fn completion_retains_echo_events_when_session_ends_before_prompt_completion() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");
        make_ready_for_input(&server, &worker, "> ");
        server.begin_input().expect("begin input");

        let _ = worker.send(WorkerToServerIpcMessage::InputLine {
            prompt: "> ".to_string(),
            text: "quit()\n".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::SessionEnd {
            reason: None,
            message: None,
        });

        let completion =
            driver_wait_for_completion(Duration::from_millis(200), server, OutputTextSource::Ipc)
                .expect("expected completion after session end");

        assert!(completion.session_end_seen);
        assert_eq!(completion.echo_events.len(), 1);
        assert_eq!(completion.echo_events[0].prompt, "> ");
        assert_eq!(completion.echo_events[0].line, "quit()\n");
    }

    #[test]
    fn completion_reports_session_end_when_prompt_is_also_stable() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");
        make_ready_for_input(&server, &worker, "> ");
        server.begin_input().expect("begin input");

        let _ = worker.send(WorkerToServerIpcMessage::InputLine {
            prompt: "> ".to_string(),
            text: "quit()\n".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::SessionEnd {
            reason: None,
            message: None,
        });

        let completion =
            driver_wait_for_completion(Duration::from_millis(200), server, OutputTextSource::Ipc)
                .expect("expected completion after session end");

        assert!(completion.session_end_seen);
        assert_eq!(completion.echo_events.len(), 1);
        assert_eq!(completion.echo_events[0].prompt, "> ");
        assert_eq!(completion.echo_events[0].line, "quit()\n");
    }

    #[test]
    fn normalize_input_newlines_canonicalizes_crlf_and_cr() {
        assert_eq!(normalize_input_newlines("a\r\nb\rc\n"), "a\nb\nc\n");
    }
}
