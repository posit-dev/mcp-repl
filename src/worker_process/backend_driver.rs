use std::time::Duration;

use crate::backend::{Backend, WorkerLaunch};
use crate::completion_reply::CompletionInfo;
use crate::ipc::{IpcWaitError, ServerIpcConnection, ServerToWorkerIpcMessage};
use crate::output_capture::OutputTextSource;
use crate::oversized_output::OversizedOutputMode;
use crate::stdin_payload::prepare_worker_stdin_payload;
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

    fn prepare_input_payload(&self, text: &str) -> Vec<u8> {
        prepare_worker_stdin_payload(text)
    }

    fn on_input_start(
        &mut self,
        text: &str,
        payload: &[u8],
        ipc: &ServerIpcConnection,
        timeout: Duration,
    ) -> Result<(), WorkerError>;

    fn on_input_written(&mut self, _ipc: &ServerIpcConnection) -> Result<(), WorkerError> {
        Ok(())
    }

    fn should_settle_output_after_timeout(
        &self,
        oversized_output: OversizedOutputMode,
        pending_input: Option<&str>,
    ) -> bool;

    fn should_write_stdin_payload(&self) -> bool {
        true
    }

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

struct RBackendDriver {
    next_input_id: u64,
    active_input_id: Option<u64>,
}

impl RBackendDriver {
    fn new() -> Self {
        Self {
            next_input_id: 1,
            active_input_id: None,
        }
    }

    fn next_input_id(&mut self) -> u64 {
        let input_id = self.next_input_id;
        self.next_input_id = self.next_input_id.wrapping_add(1).max(1);
        input_id
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
        let _ = ipc.send(ServerToWorkerIpcMessage::Interrupt { input_id: None });
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

impl BackendDriver for RBackendDriver {
    fn prepare_input_text(&self, text: String) -> String {
        normalize_input_newlines(&text)
    }

    fn on_input_start(
        &mut self,
        text: &str,
        _payload: &[u8],
        ipc: &ServerIpcConnection,
        timeout: Duration,
    ) -> Result<(), WorkerError> {
        if let Some(message) = ipc.take_protocol_error() {
            return Err(WorkerError::Protocol(message));
        }
        let input_id = self.next_input_id();
        ipc.begin_input(input_id);
        send_worker_ipc_with_timeout(
            ipc,
            ServerToWorkerIpcMessage::InputBatch {
                input_id,
                input: text.to_string(),
            },
            timeout,
        )?;
        self.active_input_id = Some(input_id);
        if let Some(message) = ipc.take_protocol_error() {
            return Err(WorkerError::Protocol(message));
        }
        Ok(())
    }

    fn should_write_stdin_payload(&self) -> bool {
        false
    }

    fn clear_active_input(&mut self) {
        self.active_input_id = None;
    }

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
        let result = driver_wait_for_completion(timeout, ipc, OutputTextSource::Ipc);
        if result.is_ok() || matches!(result, Err(WorkerError::Protocol(_))) {
            self.active_input_id = None;
        }
        result
    }

    fn interrupt(&mut self, process: &mut WorkerProcess) -> Result<(), WorkerError> {
        if let Some(ipc) = process.ipc_connection() {
            let _ = ipc.send(ServerToWorkerIpcMessage::Interrupt {
                input_id: self.active_input_id,
            });
        }
        let result = process.send_r_interrupt();
        if result.is_ok() {
            self.active_input_id = None;
        }
        result
    }
}

struct ProtocolBackendDriver {
    next_input_id: u64,
    active_input_id: Option<u64>,
    is_builtin_python: bool,
}

impl ProtocolBackendDriver {
    fn new() -> Self {
        Self {
            next_input_id: 1,
            active_input_id: None,
            is_builtin_python: false,
        }
    }

    fn builtin_python() -> Self {
        Self {
            next_input_id: 1,
            active_input_id: None,
            is_builtin_python: true,
        }
    }

    fn next_input_id(&mut self) -> u64 {
        let input_id = self.next_input_id;
        self.next_input_id = self.next_input_id.wrapping_add(1).max(1);
        input_id
    }
}

impl BackendDriver for ProtocolBackendDriver {
    fn on_input_start(
        &mut self,
        text: &str,
        payload: &[u8],
        ipc: &ServerIpcConnection,
        timeout: Duration,
    ) -> Result<(), WorkerError> {
        let _ = payload;
        if let Some(message) = ipc.take_protocol_error() {
            return Err(WorkerError::Protocol(message));
        }
        let input_id = self.next_input_id();
        ipc.begin_input(input_id);
        send_worker_ipc_with_timeout(
            ipc,
            ServerToWorkerIpcMessage::InputBatch {
                input_id,
                input: text.to_string(),
            },
            timeout,
        )?;
        self.active_input_id = Some(input_id);
        if let Some(message) = ipc.take_protocol_error() {
            return Err(WorkerError::Protocol(message));
        }
        Ok(())
    }

    fn on_input_written(&mut self, ipc: &ServerIpcConnection) -> Result<(), WorkerError> {
        let _ = ipc;
        Ok(())
    }

    fn should_settle_output_after_timeout(
        &self,
        _oversized_output: OversizedOutputMode,
        _pending_input: Option<&str>,
    ) -> bool {
        false
    }

    fn should_write_stdin_payload(&self) -> bool {
        false
    }

    fn clear_active_input(&mut self) {
        self.active_input_id = None;
    }

    fn wait_for_completion(
        &mut self,
        timeout: Duration,
        ipc: ServerIpcConnection,
    ) -> Result<CompletionInfo, WorkerError> {
        let result = driver_wait_for_completion(timeout, ipc, OutputTextSource::Ipc);
        if result.is_ok() || matches!(result, Err(WorkerError::Protocol(_))) {
            self.active_input_id = None;
        }
        result
    }

    fn interrupt(&mut self, process: &mut WorkerProcess) -> Result<(), WorkerError> {
        if let Some(ipc) = process.ipc_connection() {
            if let Some(input_id) = self.active_input_id {
                ipc.send(ServerToWorkerIpcMessage::Interrupt {
                    input_id: Some(input_id),
                })
                .map_err(WorkerError::Io)?;
            } else if self.is_builtin_python {
                ipc.send(ServerToWorkerIpcMessage::Interrupt { input_id: None })
                    .map_err(WorkerError::Io)?;
            }
        }
        let result = process.send_interrupt();
        if result.is_ok() {
            self.active_input_id = None;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::ipc::{ServerToWorkerIpcMessage, WorkerToServerIpcMessage};
    use crate::output_capture::OutputTextSource;

    use super::*;

    #[test]
    fn r_driver_sends_input_batch_without_stdin_payload_write() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");
        let mut driver = RBackendDriver::new();

        driver
            .on_input_start("1+1", b"1+1\n", &server, Duration::from_millis(200))
            .expect("R input_batch should send");

        assert!(
            !driver.should_write_stdin_payload(),
            "R driver should not ask the server to write managed input to stdin"
        );
        assert!(matches!(
            worker.recv(Some(Duration::from_millis(200))),
            Some(ServerToWorkerIpcMessage::InputBatch { input_id: 1, input })
                if input == "1+1"
        ));
    }

    #[test]
    fn completion_uses_input_line_and_input_wait() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");
        server.begin_input(1);
        let _ = worker.send(WorkerToServerIpcMessage::InputLine {
            input_id: 1,
            prompt: "> ".to_string(),
            text: "1+1\n".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::InputWait {
            input_id: 1,
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
        server.begin_input(1);
        let _ = worker.send(WorkerToServerIpcMessage::InputWait {
            input_id: 1,
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

        driver
            .on_input_start(
                "answer = input('p> ')",
                b"answer = input('p> ')\n",
                &server,
                Duration::from_millis(200),
            )
            .expect("input_batch should send");
        assert!(matches!(
            worker.recv(Some(Duration::from_millis(200))),
            Some(ServerToWorkerIpcMessage::InputBatch { input_id: 1, input })
                if input == "answer = input('p> ')"
        ));
        worker
            .send(WorkerToServerIpcMessage::InputWait {
                input_id: 1,
                prompt: "p> ".to_string(),
            })
            .expect("send input_wait");
        let completion = driver
            .wait_for_completion(Duration::from_millis(200), server.clone())
            .expect("input_wait should complete reply");
        assert_eq!(completion.prompt.as_deref(), Some("p> "));

        driver
            .on_input_start("ok\n", b"ok\n", &server, Duration::from_millis(200))
            .expect("next input_batch should send");
        assert!(matches!(
            worker.recv(Some(Duration::from_millis(200))),
            Some(ServerToWorkerIpcMessage::InputBatch { input_id: 2, input }) if input == "ok\n"
        ));
    }

    #[test]
    fn r_driver_sends_fresh_turn_after_input_wait() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");
        let mut driver = RBackendDriver::new();

        driver
            .on_input_start(
                "answer <- readline('p> ')",
                b"answer <- readline('p> ')\n",
                &server,
                Duration::from_millis(200),
            )
            .expect("input_batch should send");
        assert!(matches!(
            worker.recv(Some(Duration::from_millis(200))),
            Some(ServerToWorkerIpcMessage::InputBatch { input_id: 1, input })
                if input == "answer <- readline('p> ')"
        ));
        worker
            .send(WorkerToServerIpcMessage::InputWait {
                input_id: 1,
                prompt: "p> ".to_string(),
            })
            .expect("send input_wait");
        let completion = driver
            .wait_for_completion(Duration::from_millis(200), server.clone())
            .expect("input_wait should complete reply");
        assert_eq!(completion.prompt.as_deref(), Some("p> "));

        driver
            .on_input_start("ok\n", b"ok\n", &server, Duration::from_millis(200))
            .expect("next input_batch should send");
        assert!(matches!(
            worker.recv(Some(Duration::from_millis(200))),
            Some(ServerToWorkerIpcMessage::InputBatch { input_id: 2, input }) if input == "ok\n"
        ));
    }

    #[test]
    fn next_request_result_is_retained_after_explicit_input_wait() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");

        server.begin_input(1);
        let _ = worker.send(WorkerToServerIpcMessage::InputWait {
            input_id: 1,
            prompt: "> ".to_string(),
        });
        let first = driver_wait_for_completion(
            Duration::from_millis(200),
            server.clone(),
            OutputTextSource::Ipc,
        )
        .expect("expected first completion");
        assert_eq!(first.prompt.as_deref(), Some("> "));

        server.begin_input(2);
        let _ = worker.send(WorkerToServerIpcMessage::InputLine {
            input_id: 2,
            prompt: "> ".to_string(),
            text: "second()\n".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::InputWait {
            input_id: 2,
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

        server.begin_input(1);
        let _ = worker.send(WorkerToServerIpcMessage::InputLine {
            input_id: 1,
            prompt: "> ".to_string(),
            text: "first()\n".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::InputWait {
            input_id: 1,
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
        server.begin_input(1);

        let _ = worker.send(WorkerToServerIpcMessage::InputLine {
            input_id: 1,
            prompt: "> ".to_string(),
            text: "quit()\n".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::SessionEnd {
            reason: None,
            message_b64: None,
            input_id: None,
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
        server.begin_input(1);

        let _ = worker.send(WorkerToServerIpcMessage::InputLine {
            input_id: 1,
            prompt: "> ".to_string(),
            text: "quit()\n".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::SessionEnd {
            reason: None,
            message_b64: None,
            input_id: None,
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
