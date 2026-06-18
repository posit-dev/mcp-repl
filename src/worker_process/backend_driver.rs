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

    fn clear_active_turn(&mut self) {}

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

#[cfg(test)]
fn driver_on_input_start(_text: &str, ipc: &ServerIpcConnection) -> Result<(), WorkerError> {
    ipc.begin_request();
    if let Some(message) = ipc.take_protocol_error() {
        return Err(WorkerError::Protocol(message));
    }
    Ok(())
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
        let _ = ipc.send(ServerToWorkerIpcMessage::Interrupt { turn_id: None });
    }
    process.send_interrupt()
}

fn normalize_input_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

impl BackendDriver for RBackendDriver {
    fn prepare_input_text(&self, text: String) -> String {
        normalize_input_newlines(&text)
    }

    fn on_input_start(
        &mut self,
        _text: &str,
        payload: &[u8],
        ipc: &ServerIpcConnection,
        _timeout: Duration,
    ) -> Result<(), WorkerError> {
        ipc.begin_request_with_stdin(payload);
        if let Some(message) = ipc.take_protocol_error() {
            return Err(WorkerError::Protocol(message));
        }
        Ok(())
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
        driver_wait_for_completion(timeout, ipc, OutputTextSource::Ipc)
    }

    fn interrupt(&mut self, process: &mut WorkerProcess) -> Result<(), WorkerError> {
        if let Some(ipc) = process.ipc_connection() {
            let _ = ipc.send(ServerToWorkerIpcMessage::Interrupt { turn_id: None });
        }
        process.send_r_interrupt()
    }
}

struct ProtocolBackendDriver {
    next_turn_id: u64,
    active_turn_id: Option<u64>,
    is_builtin_python: bool,
}

impl ProtocolBackendDriver {
    fn new() -> Self {
        Self {
            next_turn_id: 1,
            active_turn_id: None,
            is_builtin_python: false,
        }
    }

    fn builtin_python() -> Self {
        Self {
            next_turn_id: 1,
            active_turn_id: None,
            is_builtin_python: true,
        }
    }

    fn next_turn_id(&mut self) -> u64 {
        let turn_id = self.next_turn_id;
        self.next_turn_id = self.next_turn_id.wrapping_add(1).max(1);
        turn_id
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
        let turn_id = self.next_turn_id();
        ipc.begin_turn(turn_id);
        match ipc.send_with_timeout(
            ServerToWorkerIpcMessage::TurnStart {
                turn_id,
                input: text.to_string(),
            },
            timeout,
        ) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::TimedOut => {
                return Err(WorkerError::Timeout(timeout));
            }
            Err(err) => return Err(WorkerError::Io(err)),
        }
        self.active_turn_id = Some(turn_id);
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

    fn clear_active_turn(&mut self) {
        self.active_turn_id = None;
    }

    fn wait_for_completion(
        &mut self,
        timeout: Duration,
        ipc: ServerIpcConnection,
    ) -> Result<CompletionInfo, WorkerError> {
        let result = driver_wait_for_completion(timeout, ipc, OutputTextSource::Ipc);
        if matches!(result, Ok(_) | Err(WorkerError::Protocol(_))) {
            self.active_turn_id = None;
        }
        result
    }

    fn interrupt(&mut self, process: &mut WorkerProcess) -> Result<(), WorkerError> {
        if let Some(ipc) = process.ipc_connection() {
            if let Some(turn_id) = self.active_turn_id {
                ipc.send(ServerToWorkerIpcMessage::Interrupt {
                    turn_id: Some(turn_id),
                })
                .map_err(WorkerError::Io)?;
            } else if self.is_builtin_python {
                ipc.send(ServerToWorkerIpcMessage::Interrupt { turn_id: None })
                    .map_err(WorkerError::Io)?;
            }
        }
        process.send_interrupt()
    }
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use std::thread;
    use std::time::Duration;

    use crate::ipc::{IpcWaitError, WorkerToServerIpcMessage};
    use crate::output_capture::OutputTextSource;

    use super::*;

    #[test]
    fn completion_infers_nested_waiting_prompt_that_reuses_primary_prompt_text() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");
        driver_on_input_start("value <- readline(prompt = \"> \")", &server)
            .expect("begin request");
        let prompt = "> ".to_string();
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: prompt.clone(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineResult {
            prompt: prompt.clone(),
            line: "value <- readline(prompt = \"> \")\n".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: prompt.clone(),
        });

        let completion =
            driver_wait_for_completion(Duration::from_millis(200), server, OutputTextSource::Ipc)
                .expect("expected stable waiting prompt to complete request");
        assert_eq!(completion.prompt.as_deref(), Some("> "));
        assert_eq!(completion.echo_events.len(), 1);
        assert_eq!(completion.echo_events[0].prompt, "> ");
        assert_eq!(
            completion.echo_events[0].line,
            "value <- readline(prompt = \"> \")\n"
        );
    }

    #[test]
    fn completion_infers_stable_waiting_prompt_without_worker_completion_event() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");
        driver_on_input_start("1+1", &server).expect("begin request");
        let prompt = "> ".to_string();
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: prompt.clone(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineResult {
            prompt: prompt.clone(),
            line: "1+1\n".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: prompt.clone(),
        });

        let completion =
            driver_wait_for_completion(Duration::from_millis(200), server, OutputTextSource::Ipc)
                .expect("expected stable waiting prompt to complete request");

        assert_eq!(completion.prompt.as_deref(), Some("> "));
        assert_eq!(completion.echo_events.len(), 1);
        assert_eq!(completion.echo_events[0].line, "1+1\n");
    }

    #[test]
    fn completion_settle_after_prompt_does_not_count_as_execution_timeout() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");
        driver_on_input_start("1+1", &server).expect("begin request");
        let prompt = "> ".to_string();
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: prompt.clone(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineResult {
            prompt: prompt.clone(),
            line: "1+1\n".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: prompt.clone(),
        });
        thread::sleep(Duration::from_millis(1));

        let completion =
            driver_wait_for_completion(Duration::from_millis(5), server, OutputTextSource::Ipc)
                .expect("expected prompt seen before timeout to complete after stable settle");

        assert_eq!(completion.prompt.as_deref(), Some("> "));
        assert_eq!(completion.echo_events.len(), 1);
        assert_eq!(completion.echo_events[0].line, "1+1\n");
    }

    #[test]
    fn completion_infers_stable_continuation_prompt_when_input_is_consumed() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");
        driver_on_input_start("1+\n1", &server).expect("begin request");
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: "> ".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineResult {
            prompt: "> ".to_string(),
            line: "1+\n".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: "+ ".to_string(),
        });

        let completion =
            driver_wait_for_completion(Duration::from_millis(200), server, OutputTextSource::Ipc)
                .expect("expected stable continuation prompt to complete request");
        assert_eq!(completion.prompt.as_deref(), Some("+ "));
    }

    #[test]
    fn completion_settle_waits_for_late_echo_events() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");
        driver_on_input_start("1+\n1", &server).expect("begin request");
        let prompt = "> ".to_string();
        let delayed_worker = worker.clone();

        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: prompt.clone(),
        });

        let late_sender = thread::spawn(move || {
            thread::sleep(Duration::from_millis(1));
            let _ = delayed_worker.send(WorkerToServerIpcMessage::ReadlineResult {
                prompt: "> ".to_string(),
                line: "1+\n".to_string(),
            });
            thread::sleep(Duration::from_millis(21));
            let _ = delayed_worker.send(WorkerToServerIpcMessage::ReadlineResult {
                prompt: "+ ".to_string(),
                line: "1\n".to_string(),
            });
            let _ = delayed_worker.send(WorkerToServerIpcMessage::ReadlineStart {
                prompt: "> ".to_string(),
            });
        });

        let completion =
            driver_wait_for_completion(Duration::from_millis(200), server, OutputTextSource::Ipc)
                .expect("expected completion after stable waiting prompt");
        late_sender.join().expect("late sender should join");

        assert_eq!(completion.prompt.as_deref(), Some("> "));
        assert_eq!(completion.echo_events.len(), 2);
        assert!(completion.protocol_warnings.is_empty());
        assert_eq!(completion.echo_events[0].prompt, "> ");
        assert_eq!(completion.echo_events[0].line, "1+\n");
        assert_eq!(completion.echo_events[1].prompt, "+ ");
        assert_eq!(completion.echo_events[1].line, "1\n");
    }

    #[test]
    fn completion_waits_for_active_stdin_accounting_before_prompt_completion() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");
        server.begin_request_with_stdin(b"1+\n1\n");

        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: "> ".to_string(),
        });
        thread::sleep(REQUEST_COMPLETION_STABLE_WAIT + Duration::from_millis(5));
        let early = server
            .wait_for_request_completion(Duration::from_millis(1), REQUEST_COMPLETION_STABLE_WAIT);
        assert!(
            matches!(early, Err(IpcWaitError::Timeout)),
            "did not expect buffered readline start to complete request, got {early:?}"
        );

        let _ = worker.send(WorkerToServerIpcMessage::ReadlineInputBytes {
            data_b64: base64::engine::general_purpose::STANDARD.encode(b"1+\n"),
        });
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineResult {
            prompt: "> ".to_string(),
            line: "1+\n".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: "+ ".to_string(),
        });
        thread::sleep(REQUEST_COMPLETION_STABLE_WAIT + Duration::from_millis(5));
        let continuation = server
            .wait_for_request_completion(Duration::from_millis(1), REQUEST_COMPLETION_STABLE_WAIT);
        assert!(
            matches!(continuation, Err(IpcWaitError::Timeout)),
            "did not expect buffered continuation start to complete request, got {continuation:?}"
        );

        let _ = worker.send(WorkerToServerIpcMessage::ReadlineInputBytes {
            data_b64: base64::engine::general_purpose::STANDARD.encode(b"1\n"),
        });
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineResult {
            prompt: "+ ".to_string(),
            line: "1\n".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: "> ".to_string(),
        });

        let completion =
            driver_wait_for_completion(Duration::from_millis(200), server, OutputTextSource::Ipc)
                .expect("expected completion after final unsatisfied prompt");

        assert_eq!(completion.prompt.as_deref(), Some("> "));
        assert_eq!(completion.echo_events.len(), 2);
        assert_eq!(completion.echo_events[0].line, "1+\n");
        assert_eq!(completion.echo_events[1].line, "1\n");
    }

    #[test]
    fn next_request_result_is_retained_when_prompt_is_already_active() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");

        driver_on_input_start("first()", &server).expect("begin request");
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: "> ".to_string(),
        });
        let first = driver_wait_for_completion(
            Duration::from_millis(200),
            server.clone(),
            OutputTextSource::Ipc,
        )
        .expect("expected first completion");
        assert_eq!(first.prompt.as_deref(), Some("> "));

        driver_on_input_start("second()", &server).expect("begin request");
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineResult {
            prompt: "> ".to_string(),
            line: "second()\n".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
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
    fn completion_preserves_echo_events_when_next_prompt_arrives_immediately() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");

        driver_on_input_start("first()", &server).expect("begin request");
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: "> ".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineResult {
            prompt: "> ".to_string(),
            line: "first()\n".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: "> ".to_string(),
        });

        let completion =
            driver_wait_for_completion(Duration::from_millis(200), server, OutputTextSource::Ipc)
                .expect("expected completion after stable waiting prompt");

        assert_eq!(completion.prompt.as_deref(), Some("> "));
        assert!(completion.protocol_warnings.is_empty());
        assert_eq!(completion.echo_events.len(), 1);
        assert_eq!(completion.echo_events[0].prompt, "> ");
        assert_eq!(completion.echo_events[0].line, "first()\n");
    }

    #[test]
    fn completion_retains_echo_events_when_session_ends_before_prompt_completion() {
        let (server, worker) = crate::ipc::test_connection_pair().expect("ipc pair");
        driver_on_input_start("quit()", &server).expect("begin request");

        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: "> ".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineResult {
            prompt: "> ".to_string(),
            line: "quit()\n".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::SessionEnd {
            reason: None,
            message_b64: None,
            turn_id: None,
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
        driver_on_input_start("quit()", &server).expect("begin request");

        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: "> ".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineResult {
            prompt: "> ".to_string(),
            line: "quit()\n".to_string(),
        });
        let _ = worker.send(WorkerToServerIpcMessage::ReadlineStart {
            prompt: "> ".to_string(),
        });
        thread::sleep(Duration::from_millis(25));
        let _ = worker.send(WorkerToServerIpcMessage::SessionEnd {
            reason: None,
            message_b64: None,
            turn_id: None,
        });
        thread::sleep(Duration::from_millis(25));

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
