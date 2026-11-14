mod deferred;
mod operation;

pub(super) use operation::{
    EnvironmentPreparationReservation, OperationResult, WorkerOperationState,
};
use operation::{Route, RuntimeRCallbackAdmission};

use std::sync::mpsc;
use std::thread;

use crate::relay_protocol::{PARTIAL_COMMAND_EOF, RelayCommand, RelayEvent};

use super::lifecycle::OldGenerationCommitDisposition;
use super::{OutputTape, WorkerCallbacks, WorkerProcessOutcome};

pub(super) enum WorkerEvent {
    SuspendBootstrap {
        admitted: mpsc::SyncSender<Result<(), String>>,
    },
    ResumeBootstrap,
    Relay(RelayEvent),
    TransportFailure(String),
    RetireOperation {
        error: String,
        reached: mpsc::SyncSender<()>,
    },
    RelayClosed,
}

pub(super) enum ReadyCommitOutcome {
    Committed,
    Failed(String),
    Retiring,
}

pub(super) struct WorkerEventDispatcher {
    thread: thread::JoinHandle<Result<Option<WorkerProcessOutcome>, String>>,
}

struct PendingPythonCandidate {
    managed: crate::resolver::ManagedPython,
    configuration: crate::python::NativePython,
    import_resolution: Option<crate::worker_protocol::PythonImportResolution>,
}

#[derive(Default)]
struct RuntimeCandidates {
    r: Vec<crate::resolver::ManagedR>,
    python: Vec<PendingPythonCandidate>,
}

impl RuntimeCandidates {
    fn clear(&mut self) {
        self.r.clear();
        self.python.clear();
    }
}

impl WorkerEventDispatcher {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn start(
        events: mpsc::Receiver<WorkerEvent>,
        operation: WorkerOperationState,
        commands: super::platform::RelayCommandSender,
        output: OutputTape,
        callbacks: WorkerCallbacks,
        startup: mpsc::SyncSender<Result<(), String>>,
        ready_commit: mpsc::Receiver<ReadyCommitOutcome>,
        interrupts: super::platform::InterruptRequests,
        shutdown_started: super::platform::LocalShutdownObservation,
    ) -> Self {
        let thread = thread::Builder::new()
            .name("worker events".to_string())
            .spawn(move || {
                dispatch_worker_events(
                    events,
                    operation,
                    commands,
                    output,
                    callbacks,
                    startup,
                    ready_commit,
                    interrupts,
                    shutdown_started,
                )
            })
            .expect("spawn worker event dispatcher");
        Self { thread }
    }

    pub(super) fn join(self) -> Result<Option<WorkerProcessOutcome>, String> {
        self.thread
            .join()
            .map_err(|_| "worker event dispatcher task failed".to_string())?
    }
}

#[allow(clippy::too_many_arguments)]
fn dispatch_worker_events(
    events: mpsc::Receiver<WorkerEvent>,
    operation: WorkerOperationState,
    commands: super::platform::RelayCommandSender,
    output: OutputTape,
    callbacks: WorkerCallbacks,
    startup: mpsc::SyncSender<Result<(), String>>,
    ready_commit: mpsc::Receiver<ReadyCommitOutcome>,
    interrupts: super::platform::InterruptRequests,
    shutdown_started: super::platform::LocalShutdownObservation,
) -> Result<Option<WorkerProcessOutcome>, String> {
    let mut startup = Some(startup);
    let stdout = output.direct_stdout();
    let stderr = output.direct_stderr();
    let mut candidates = RuntimeCandidates::default();
    let mut worker_acquired = false;
    let mut runtime_started = false;
    let mut semantic_failure = false;
    let mut stdout_closed = false;
    let mut stderr_closed = false;
    let mut sideband_closed = false;
    let mut relay_fatal = false;
    let mut retirement_failure = None;
    let mut intentional_shutdown = false;
    let mut retiring = false;
    let mut process_outcome = None;
    let mut relay_closed = false;

    let mut deferred = deferred::DeferredEvents::default();
    loop {
        let event = if !operation.bootstrap_suspended()? && !deferred.is_empty() {
            match deferred.pop() {
                Ok(Some(event)) => WorkerEvent::Relay(event),
                Ok(None) => unreachable!("nonempty deferred bootstrap spool"),
                Err(error) => {
                    deferred.clear();
                    fail_dispatch(&operation, &mut startup, &interrupts, error);
                    semantic_failure = true;
                    continue;
                }
            }
        } else {
            let Ok(event) = events.recv() else { break };
            event
        };
        match event {
            WorkerEvent::SuspendBootstrap { admitted } => {
                let _ = admitted.send(operation.suspend_bootstrap());
            }
            WorkerEvent::ResumeBootstrap => operation.resume_bootstrap()?,
            WorkerEvent::Relay(event) => {
                if !semantic_failure
                    && !retiring
                    && operation.bootstrap_suspended()?
                    && (bootstrap_callback(&event)
                        || (!deferred.is_empty() && sideband_semantic(&event)))
                {
                    // Once a callback waits, later events from the same worker
                    // sideband must not overtake its commit. Independent streams
                    // and relay lifetime observations remain responsive.
                    if let Err(error) = deferred.push(&event) {
                        deferred.clear();
                        fail_dispatch(&operation, &mut startup, &interrupts, error);
                        semantic_failure = true;
                    }
                    continue;
                }
                if process_outcome.is_some() {
                    fail_dispatch(
                        &operation,
                        &mut startup,
                        &interrupts,
                        "worker relay sent an event after worker outcome".to_string(),
                    );
                    semantic_failure = true;
                    continue;
                }
                let result = match event {
                    RelayEvent::WorkerStarted { .. } if worker_acquired => {
                        Err("worker relay sent an unexpected worker start".to_string())
                    }
                    RelayEvent::WorkerStarted { pid } => {
                        worker_acquired = true;
                        let retained = callbacks.client.inspect_requirements();
                        output.worker_started(pid, &retained["requirements"]);
                        if let Some(active) = callbacks
                            .client
                            .0
                            .evaluation
                            .lock()
                            .map_err(|_| "evaluation lock poisoned")?
                            .as_ref()
                        {
                            active.evaluation.associate_waiting_session();
                        }
                        Ok(())
                    }
                    RelayEvent::Stdout { data } => {
                        if stdout_closed {
                            Err("worker relay sent stdout after closing the stream".to_string())
                        } else {
                            stdout.push(data.as_bytes());
                            Ok(())
                        }
                    }
                    RelayEvent::StdoutBytes { data } => {
                        if stdout_closed {
                            Err("worker relay sent stdout after closing the stream".to_string())
                        } else {
                            data.decode().map(|data| stdout.push(&data))
                        }
                    }
                    RelayEvent::Stderr { data } => {
                        if stderr_closed {
                            Err("worker relay sent stderr after closing the stream".to_string())
                        } else {
                            stderr.push(data.as_bytes());
                            Ok(())
                        }
                    }
                    RelayEvent::StderrBytes { data } => {
                        if stderr_closed {
                            Err("worker relay sent stderr after closing the stream".to_string())
                        } else {
                            data.decode().map(|data| stderr.push(&data))
                        }
                    }
                    RelayEvent::StdoutClosed => {
                        if stdout_closed {
                            Err("worker relay closed stdout twice".to_string())
                        } else {
                            stdout.close();
                            stdout_closed = true;
                            Ok(())
                        }
                    }
                    RelayEvent::StderrClosed => {
                        if stderr_closed {
                            Err("worker relay closed stderr twice".to_string())
                        } else {
                            stderr.close();
                            stderr_closed = true;
                            Ok(())
                        }
                    }
                    RelayEvent::WorkerSidebandClosed => {
                        if sideband_closed {
                            Err("worker relay closed the worker sideband twice".to_string())
                        } else {
                            sideband_closed = true;
                            if !intentional_shutdown && !semantic_failure && !retiring {
                                fail_dispatch(
                                    &operation,
                                    &mut startup,
                                    &interrupts,
                                    "worker sideband read failed: worker sideband closed"
                                        .to_string(),
                                );
                                semantic_failure = true;
                            }
                            Ok(())
                        }
                    }
                    RelayEvent::InterruptResult { request_id, error } => {
                        if semantic_failure || retiring {
                            Ok(())
                        } else {
                            interrupts.complete(request_id, error)
                        }
                    }
                    RelayEvent::ShutdownStarted => {
                        intentional_shutdown = true;
                        shutdown_started.observe()
                    }
                    RelayEvent::WorkerExited { code } => {
                        output.worker_ended(&format!("exited with code {code}"));
                        process_outcome = Some(WorkerProcessOutcome::Exited(code));
                        Ok(())
                    }
                    RelayEvent::WorkerSignaled { signal } => {
                        output.worker_ended(&format!("terminated by signal {signal}"));
                        process_outcome = Some(WorkerProcessOutcome::Signaled(signal));
                        Ok(())
                    }
                    RelayEvent::Fatal { message } => {
                        if relay_fatal && !retiring {
                            Err("worker relay reported two fatal failures".to_string())
                        } else {
                            relay_fatal = true;
                            if retiring {
                                // Aborting the sole writer can leave a partial
                                // command. Its EOF failure describes transport,
                                // not cleanup. Other Fatal messages and the
                                // launcher's retirement still gate replacement.
                                let aborted_frame = commands.is_aborted()
                                    && (message == PARTIAL_COMMAND_EOF
                                        || message
                                            == format!(
                                                "relay stdin frame is invalid: {PARTIAL_COMMAND_EOF}"
                                            ));
                                if !aborted_frame {
                                    retirement_failure.get_or_insert(message);
                                }
                            } else {
                                fail_dispatch(&operation, &mut startup, &interrupts, message);
                                semantic_failure = true;
                            }
                            Ok(())
                        }
                    }
                    _ if sideband_closed => Err(
                        "worker relay sent a semantic event after closing the worker sideband"
                            .to_string(),
                    ),
                    RelayEvent::RInitialization { complete: false } => {
                        // Retirement must retain evidence of startup user code,
                        // even when ordinary callbacks are no longer admitted.
                        callbacks.generation.r_initialization(false);
                        Ok(())
                    }
                    _ if semantic_failure => Ok(()),
                    semantic if retiring && ignored_during_retirement(&semantic) => Ok(()),
                    RelayEvent::Ready => {
                        if !worker_acquired || runtime_started || startup.is_none() {
                            Err("worker sent an unexpected ready message".to_string())
                        } else {
                            if let Some(startup) = startup.take() {
                                let _ = startup.send(Ok(()));
                            }
                            // Commit readiness to generation-owned lifecycle state before
                            // dispatching callbacks already queued behind Ready. Otherwise a
                            // callback could mutate state for a worker not yet admitted.
                            match ready_commit.recv() {
                                Ok(ReadyCommitOutcome::Committed) => {
                                    if let Some(recording) = callbacks
                                        .client
                                        .0
                                        .recording
                                        .lock()
                                        .expect("recording lock")
                                        .as_ref()
                                    {
                                        recording.worker_ready();
                                    }
                                    runtime_started = true;
                                    Ok(())
                                }
                                Ok(ReadyCommitOutcome::Failed(error)) => Err(error),
                                Ok(ReadyCommitOutcome::Retiring) => {
                                    operation.retire_operation(
                                        "worker stopped before operation completed".to_string(),
                                    );
                                    retiring = true;
                                    runtime_started = true;
                                    Ok(())
                                }
                                Err(_) => Err("worker readiness commit stopped".to_string()),
                            }
                        }
                    }
                    semantic if !runtime_started => Err(startup_semantic_error(&semantic)),
                    semantic => handle_semantic_event(
                        semantic,
                        &operation,
                        &commands,
                        &output,
                        &callbacks,
                        &mut candidates,
                    ),
                };
                if let Err(error) = result {
                    callbacks.client.fail_priority(&commands, &error);
                    candidates.clear();
                    fail_dispatch(&operation, &mut startup, &interrupts, error);
                    semantic_failure = true;
                }
            }
            WorkerEvent::TransportFailure(error) => {
                callbacks.client.fail_priority(&commands, &error);
                commands.abort(error.clone());
                if !retiring {
                    candidates.clear();
                    fail_dispatch(&operation, &mut startup, &interrupts, error);
                    semantic_failure = true;
                }
            }
            WorkerEvent::RetireOperation { error, reached } => {
                callbacks.client.fail_priority(&commands, &error);
                deferred.clear();
                if let Some(startup) = startup.take() {
                    let _ = startup.send(Err(error.clone()));
                }
                interrupts.fail(error.clone());
                operation.retire_operation(error);
                candidates.clear();
                retiring = true;
                let _ = reached.send(());
                if relay_closed {
                    break;
                }
            }
            WorkerEvent::RelayClosed => {
                relay_closed = true;
                if !(retiring
                    || stdout_closed
                        && stderr_closed
                        && sideband_closed
                        && process_outcome.is_some())
                {
                    let error = if startup.is_some() {
                        "worker relay exited before readiness"
                    } else {
                        "worker relay stdout closed before retirement completed"
                    };
                    fail_relay_exit(&operation, &mut startup, &interrupts, error.to_string());
                    semantic_failure = true;
                }
                // Record the exit cause before rejected command admission can
                // let a racing evaluator publish an ordinary transport failure.
                commands.abort("worker relay command transport closed".to_string());
                if retiring || semantic_failure || !intentional_shutdown {
                    break;
                }
            }
        }
    }

    if !stdout_closed {
        stdout.close();
    }
    if !stderr_closed {
        stderr.close();
    }
    if !relay_closed && !retiring {
        fail_dispatch(
            &operation,
            &mut startup,
            &interrupts,
            "worker event queue closed".to_string(),
        );
    }
    callbacks.client.fail_priority(
        &commands,
        "worker stopped before priority execution completed",
    );
    interrupts.fail("worker stopped before interrupt completed".to_string());
    retirement_failure.map_or(Ok(process_outcome), Err)
}

fn bootstrap_callback(event: &RelayEvent) -> bool {
    matches!(
        event,
        RelayEvent::ResolveR { .. }
            | RelayEvent::RActivated { .. }
            | RelayEvent::RActivationFailed { .. }
            | RelayEvent::ResolvePython { .. }
            | RelayEvent::ResolvePythonVersion { .. }
            | RelayEvent::PythonActivated { .. }
            | RelayEvent::PythonActivationFailed { .. }
            | RelayEvent::RuntimeInitialized { .. }
    )
}

fn sideband_semantic(event: &RelayEvent) -> bool {
    !matches!(
        event,
        RelayEvent::WorkerStarted { .. }
            | RelayEvent::Stdout { .. }
            | RelayEvent::RInitialization { .. }
            | RelayEvent::StdoutBytes { .. }
            | RelayEvent::Stderr { .. }
            | RelayEvent::StderrBytes { .. }
            | RelayEvent::StdoutClosed
            | RelayEvent::StderrClosed
            | RelayEvent::WorkerSidebandClosed
            | RelayEvent::InterruptResult { .. }
            | RelayEvent::ShutdownStarted
            | RelayEvent::WorkerExited { .. }
            | RelayEvent::WorkerSignaled { .. }
            | RelayEvent::Fatal { .. }
    )
}

fn ignored_during_retirement(event: &RelayEvent) -> bool {
    matches!(
        event,
        RelayEvent::Ready
            | RelayEvent::RuntimeInitialized { .. }
            | RelayEvent::RInitialization { .. }
            | RelayEvent::InputRequested { .. }
            | RelayEvent::InputReceived
            | RelayEvent::InputCancelled
            | RelayEvent::ResolveR { .. }
            | RelayEvent::RActivated { .. }
            | RelayEvent::RActivationFailed { .. }
            | RelayEvent::ResolvePython { .. }
            | RelayEvent::ResolvePythonVersion { .. }
            | RelayEvent::PythonActivated { .. }
            | RelayEvent::PythonActivationFailed { .. }
    )
}

fn fail_dispatch(
    operation: &WorkerOperationState,
    startup: &mut Option<mpsc::SyncSender<Result<(), String>>>,
    interrupts: &super::platform::InterruptRequests,
    error: String,
) {
    if let Some(startup) = startup.take() {
        let _ = startup.send(Err(error.clone()));
    }
    interrupts.fail(error.clone());
    operation.fail(error);
}

fn fail_relay_exit(
    operation: &WorkerOperationState,
    startup: &mut Option<mpsc::SyncSender<Result<(), String>>>,
    interrupts: &super::platform::InterruptRequests,
    error: String,
) {
    if let Some(startup) = startup.take() {
        let _ = startup.send(Err(error.clone()));
    }
    interrupts.fail(error.clone());
    operation.fail_from_relay_exit(error);
}

fn startup_semantic_error(event: &RelayEvent) -> String {
    match event {
        RelayEvent::ConsoleOutput { data } | RelayEvent::ConsoleDiagnostic { data } => {
            format!("worker emitted output before readiness: {data}")
        }
        RelayEvent::Image { .. } => "worker emitted an image before readiness".to_string(),
        _ => "worker did not report readiness".to_string(),
    }
}

fn handle_semantic_event(
    event: RelayEvent,
    operation: &WorkerOperationState,
    commands: &super::platform::RelayCommandSender,
    output: &OutputTape,
    callbacks: &WorkerCallbacks,
    candidates: &mut RuntimeCandidates,
) -> Result<(), String> {
    use crate::worker_protocol::ConsoleChannel::{Diagnostic, Output};

    match event {
        RelayEvent::Priority { request_id, event } => {
            let completed = matches!(
                event,
                crate::worker_protocol::PriorityEvent::Completed { .. }
            );
            callbacks
                .client
                .priority_event(commands, request_id, event)?;
            if completed {
                operation.finish_r_priority(request_id)?;
            }
            Ok(())
        }
        RelayEvent::CellStarted => operation.with_route(|route| match route {
            Route::Cell(evaluation) => evaluation.start_cell_capture(),
            _ => Err("worker started a cell without an admitted source".into()),
        }),
        RelayEvent::ExecutionError { error } => operation.with_route(|route| match route {
            Route::Cell(evaluation) => evaluation.execution_error(error),
            _ => Err("worker reported an execution error without an admitted source".into()),
        }),
        RelayEvent::ConsoleOutput { data } => operation.with_route(|route| match route {
            Route::Cell(evaluation) => evaluation.output(Output, data),
            Route::Bootstrap | Route::Preparation | Route::Idle => {
                output.push_console_text(Output, data);
                Ok(())
            }
        }),
        RelayEvent::ConsoleDiagnostic { data } => operation.with_route(|route| match route {
            Route::Cell(evaluation) => evaluation.output(Diagnostic, data),
            Route::Bootstrap | Route::Preparation | Route::Idle => {
                output.push_console_text(Diagnostic, data);
                Ok(())
            }
        }),
        RelayEvent::Image { data, mime_type } => operation.with_route(|route| match route {
            Route::Cell(evaluation) => evaluation.image(data, mime_type),
            Route::Bootstrap | Route::Preparation | Route::Idle => {
                crate::transcript::validate_image_data(&data)?;
                let recording = callbacks
                    .client
                    .0
                    .recording
                    .lock()
                    .expect("recording lock")
                    .clone();
                output.push_image_with_artifact(data, mime_type, |data, mime_type| {
                    recording.as_ref().map_or(Ok(None), |recording| {
                        recording.persist_session_image(data, mime_type)
                    })
                })
            }
        }),
        RelayEvent::InputRequested { prompt } => {
            let rendered = serde_json::to_string(&prompt)
                .map_err(|error| format!("failed to render worker input prompt: {error}"))?;
            operation.input_requested(prompt, rendered, output)
        }
        RelayEvent::InputReceived | RelayEvent::InputCancelled => operation.input_received(),
        RelayEvent::ResolveR { packages } => {
            use crate::worker_client::environment::RuntimeRResolutionFailure;
            use crate::worker_protocol::RResolutionFailureKind;

            if matches!(
                operation.begin_runtime_r_callback(|| {
                    commands.send(RelayCommand::RResolutionFailed {
                        failure: RResolutionFailureKind::Host,
                        message:
                            "R package resolution is unavailable during requirement preparation"
                                .to_string(),
                    })
                })?,
                RuntimeRCallbackAdmission::Busy
            ) {
                return Ok(());
            }
            let (response, awaiting_activation) = match callbacks.resolve_r(packages) {
                Ok(managed) => {
                    let library = managed
                        .library()
                        .to_str()
                        .ok_or_else(|| "resolved R library path is not UTF-8".to_string())?
                        .to_string();
                    candidates.r.push(managed);
                    (RelayCommand::RResolved { library }, true)
                }
                Err(failure) => {
                    let (failure, message) = match failure {
                        RuntimeRResolutionFailure::Ordinary(message) => {
                            (RResolutionFailureKind::Host, message)
                        }
                        RuntimeRResolutionFailure::Interrupted => (
                            RResolutionFailureKind::Interrupted,
                            "R package resolution interrupted".to_string(),
                        ),
                        RuntimeRResolutionFailure::Cancelled(message) => {
                            (RResolutionFailureKind::Operation, message)
                        }
                        RuntimeRResolutionFailure::Infrastructure(message) => {
                            return Err(message);
                        }
                    };
                    (RelayCommand::RResolutionFailed { failure, message }, false)
                }
            };
            commands.send(response)?;
            operation.runtime_r_reply_sent(awaiting_activation)
        }
        RelayEvent::RActivated { library } => {
            operation.ensure_runtime_r_activation_phase()?;
            callbacks.activate_r(library, &mut candidates.r)?;
            operation.finish_runtime_r_callback()
        }
        RelayEvent::RActivationFailed {
            library,
            message: _,
        } => {
            operation.ensure_runtime_r_activation_phase()?;
            callbacks.fail_r_activation(library, &mut candidates.r)?;
            operation.finish_runtime_r_callback()
        }
        RelayEvent::ResolvePython { request } => {
            if request.import_resolution.is_some() {
                operation.with_route(|route| match route {
                    Route::Cell(_) | Route::Bootstrap => Ok(()),
                    Route::Preparation | Route::Idle => Err(
                        "worker requested automatic Python import resolution outside an evaluation"
                            .to_string(),
                    ),
                })?;
            }
            let import_resolution = request.import_resolution.clone();
            let response = match callbacks.resolve_python(
                request,
                operation.python_preparation_extensions()?,
                operation.python_preparation_requirements()?,
            ) {
                Ok((managed, configuration)) => {
                    let python = managed.python().to_string_lossy().into_owned();
                    let native = Some(Box::new(crate::worker_protocol::NativePythonActivation {
                        selected: configuration.clone(),
                        requirements: managed.requirements().clone(),
                    }));
                    candidates.python.push(PendingPythonCandidate {
                        managed,
                        configuration,
                        import_resolution,
                    });
                    RelayCommand::PythonResolved { python, native }
                }
                Err(message) => RelayCommand::PythonResolutionFailed { message },
            };
            commands.send(response)
        }
        RelayEvent::ResolvePythonVersion { request } => {
            let response = match callbacks.resolve_python_version(request) {
                Ok(version) => RelayCommand::PythonVersionResolved { version },
                Err(message) => RelayCommand::PythonVersionResolutionFailed { message },
            };
            commands.send(response)
        }
        RelayEvent::PythonActivated { requirements } => {
            let activated = requirements.clone().normalized();
            let candidate = candidates
                .python
                .iter()
                .rposition(|candidate| candidate.managed.requirements() == &activated)
                .map(|index| candidates.python.remove(index));
            let (managed, configuration, resolution) = match candidate {
                Some(candidate) => (
                    Some(candidate.managed),
                    Some(candidate.configuration),
                    candidate.import_resolution,
                ),
                None => (None, None, None),
            };
            candidates.python.clear();
            let disposition = callbacks.activate_python(
                requirements,
                managed,
                configuration,
                operation.python_preparation_extensions()?,
            )?;
            if disposition == OldGenerationCommitDisposition::Commit
                && let Some(resolution) = resolution
                && resolution.module != resolution.distribution
            {
                operation.with_route(|route| match route {
                    Route::Cell(evaluation) => evaluation.bounded_notice(format!(
                        "resolved PyPI distribution '{}' for Python import '{}'",
                        resolution.distribution, resolution.module
                    )),
                    Route::Bootstrap => {
                        output.push_notice_line(format!(
                            "resolved PyPI distribution '{}' for Python import '{}'",
                            resolution.distribution, resolution.module
                        ));
                        Ok(())
                    }
                    Route::Preparation | Route::Idle => Err(
                        "worker activated an automatic Python import resolution outside an evaluation"
                            .to_string(),
                    ),
                })?;
            }
            Ok(())
        }
        RelayEvent::PythonActivationFailed { requirements } => {
            // R declarations can activate during a cell, explicit preparation,
            // or an idle callback. In every context the failure must identify
            // a provisional candidate belonging to this generation.
            let expected = requirements.normalized();
            if !candidates
                .python
                .iter()
                .any(|candidate| candidate.managed.requirements() == &expected)
            {
                return Err("worker failed an unexpected Python candidate".into());
            }
            candidates.python.clear();
            callbacks.fail_python_activation()?;
            Ok(())
        }
        RelayEvent::RuntimeInitialized { interrupted } => {
            callbacks.finish_bootstrap(operation, interrupted)
        }
        RelayEvent::RInitialization { complete } => {
            callbacks.generation.r_initialization(complete);
            Ok(())
        }
        event @ (RelayEvent::Completed
        | RelayEvent::RPrepared { .. }
        | RelayEvent::RPreparationFailed { .. }
        | RelayEvent::PythonPrepared
        | RelayEvent::PythonPreparationFailed { .. }
        | RelayEvent::PythonPreparationRejected { .. }) => {
            operation.complete(event, &mut candidates.r, &mut candidates.python, output)
        }
        RelayEvent::Ready
        | RelayEvent::WorkerStarted { .. }
        | RelayEvent::Stdout { .. }
        | RelayEvent::StdoutBytes { .. }
        | RelayEvent::Stderr { .. }
        | RelayEvent::StderrBytes { .. }
        | RelayEvent::StdoutClosed
        | RelayEvent::StderrClosed
        | RelayEvent::WorkerSidebandClosed
        | RelayEvent::InterruptResult { .. }
        | RelayEvent::ShutdownStarted
        | RelayEvent::WorkerExited { .. }
        | RelayEvent::WorkerSignaled { .. }
        | RelayEvent::Fatal { .. } => unreachable!("non-semantic relay event reached dispatcher"),
    }
}
