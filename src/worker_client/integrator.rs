//! Integrator requests share execution admission, but retain separate delivery ownership.

use std::time::{Duration, Instant};

use super::environment::{PreparationIntent, PrepareResult};
use super::evaluation::Origin;
use super::execution::EvaluationStartError;
use super::lifecycle::LifecycleState;
use super::{Client, Content, Response};

#[derive(Clone)]
pub(crate) struct IntegratorObservation {
    pub(crate) request_id: String,
    pub(crate) hidden: bool,
}

pub(crate) enum IntegratorRestart {
    Replace,
    Stop,
}

#[derive(Debug)]
pub(crate) enum IntegratorError {
    Busy,
    Timeout,
    Cancelled,
    StaleEvaluation,
    Failed(String),
}

#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum PreparationStatus {
    Prepared,
    RestartRequired,
    Busy,
    Failed,
}

impl From<String> for IntegratorError {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

impl Client {
    pub(crate) async fn integrator_prepare(
        &self,
        requirements: super::Requirements,
    ) -> serde_json::Value {
        let client = self.clone();
        let outcome =
            tokio::task::spawn_blocking(move || client.integrator_prepare_blocking(requirements))
                .await
                .unwrap_or_else(|error| {
                    Err(format!("integrator preparation task failed: {error}"))
                });
        let mut snapshot = self.inspect_requirements();
        snapshot["worker_id"] = serde_json::json!(self.0.output.worker_id());
        match outcome {
            Ok(status) => snapshot["status"] = serde_json::json!(status),
            Err(mut message) => {
                snapshot["status"] = serde_json::json!(PreparationStatus::Failed);
                // Resolver diagnostics may be large. Keep this separate from
                // MCP text assembly and make the operation's limit explicit.
                if message.len() > 8 * 1024 {
                    let mut end = 8 * 1024;
                    while !message.is_char_boundary(end) {
                        end -= 1;
                    }
                    message.truncate(end);
                    snapshot["message_truncated"] = serde_json::json!(true);
                }
                snapshot["message"] = serde_json::json!(message);
            }
        }
        snapshot
    }

    fn integrator_prepare_blocking(
        &self,
        requirements: super::Requirements,
    ) -> Result<PreparationStatus, String> {
        requirements.validate()?;
        self.validate_requirements(&requirements)?;
        let _activation = self.activate_session()?;
        let Some(startup) = self.startup_result().borrow().clone() else {
            return Ok(PreparationStatus::Busy);
        };
        startup?;
        let Ok(_operation) = self.0.admission.try_read() else {
            return Ok(PreparationStatus::Busy);
        };
        let Ok(generation) = self.admit() else {
            return Ok(PreparationStatus::Busy);
        };
        let Ok(preparation) = self.0.preparation.try_write() else {
            return Ok(PreparationStatus::Busy);
        };
        // Match standalone preparation's idle requirement, including completed
        // evaluations whose owner has not yet collected their output.
        if self.evaluation()?.is_some() {
            return Ok(PreparationStatus::Busy);
        }
        {
            let lifecycle = self
                .0
                .lifecycle
                .lock()
                .map_err(|_| "worker lifecycle lock poisoned".to_string())?;
            lifecycle.ensure_generation(&generation)?;
            // Configuration can be ready while native interpreter startup is
            // still running. Preparation must not queue behind that bootstrap.
            // The preparation guard excludes a new launch after this check;
            // absent or stopped workers remain eligible for prestart changes.
            if lifecycle.worker_bootstrapping()? {
                return Ok(PreparationStatus::Busy);
            }
        }
        match self.prepare_admitted(
            requirements,
            &generation,
            &preparation,
            PreparationIntent::Standalone,
        )? {
            PrepareResult::Prepared => Ok(PreparationStatus::Prepared),
            PrepareResult::RestartRequired => Ok(PreparationStatus::RestartRequired),
            PrepareResult::Failed { response, message }
            | PrepareResult::WorkerStopped { response, message } => {
                // These existing failure paths may include older MCP output.
                // Restore it before releasing preparation admission; only the
                // operation diagnostic is copied into this caller's receipt.
                self.0.output.recover(response);
                Err(message)
            }
        }
    }

    pub(crate) async fn integrator_restart(
        &self,
        mode: IntegratorRestart,
    ) -> Result<serde_json::Value, IntegratorError> {
        let client = self.clone();
        // A disconnected caller must not abandon retirement or leave ordinary
        // output owned by the control channel. The blocking task retains both.
        tokio::task::spawn_blocking(move || {
            let control = client.begin_controlled_send()?;
            let restart = client.restart_blocking(
                super::Requirements::default(),
                super::WORKER_SHUTDOWN_GRACE,
                // The eventual MCP collection supplies its own terminal state.
                match mode {
                    IntegratorRestart::Replace => super::lifecycle::RestartLaunch::Followup,
                    IntegratorRestart::Stop => super::lifecycle::RestartLaunch::OnNextSend,
                },
                Some(&control),
            )?;
            let _operation = client.admit_controlled_operation();
            // Retirement has already settled each private evaluation with its
            // original collector. Keep all remaining output available to MCP,
            // including failure details and restart notices, without delivering it.
            client.0.output.recover(restart.response);
            let generation = restart.generation.ok_or_else(|| {
                "session restart did not complete; collect Console output for details".to_string()
            })?;
            client
                .0
                .startup_failed
                .store(false, std::sync::atomic::Ordering::Release);
            client.finish_startup(Ok(()));
            client.ensure_controlled_generation(&control, &generation)?;
            Ok(serde_json::json!({
                "accepted": true,
                "worker_id": client.0.output.worker_id(),
            }))
        })
        .await
        .map_err(|error| {
            IntegratorError::Failed(format!("integrator restart task failed: {error}"))
        })?
        .map_err(IntegratorError::Failed)
    }

    /// Pin the observed evaluation while queuing stdin so a late reply cannot
    /// reach another cell or a replacement worker.
    pub(crate) fn integrator_stdin(
        &self,
        evaluation_id: u64,
        input: String,
    ) -> Result<(), IntegratorError> {
        let active = self.evaluation()?;
        let lifecycle = self
            .0
            .lifecycle
            .lock()
            .map_err(|_| "worker lifecycle lock poisoned".to_string())?;
        let evaluation = &active
            .as_ref()
            .filter(|active| active.evaluation.id() == evaluation_id)
            .ok_or(IntegratorError::StaleEvaluation)?
            .evaluation;
        lifecycle.ensure_generation(&evaluation.admission.generation)?;
        if evaluation.admission.outcome()?.is_some() {
            return Err(IntegratorError::StaleEvaluation);
        }
        evaluation.submit_integrator_stdin(input)?;
        Ok(())
    }

    pub(crate) async fn integrator_evaluate(
        &self,
        cell: crate::cell::Cell,
        wait: Option<Duration>,
        transcript: crate::transcript::Transcript,
        call_id: Option<u64>,
        mut cancelled: tokio::sync::watch::Receiver<bool>,
        observation: IntegratorObservation,
    ) -> Result<(Response, u64, serde_json::Value), IntegratorError> {
        if *cancelled.borrow() {
            return Err(IntegratorError::Cancelled);
        }
        let _activation = self.activate_session()?;
        let (generation, mut connection) = {
            let lifecycle = self
                .0
                .lifecycle
                .lock()
                .map_err(|_| "worker lifecycle lock poisoned".to_string())?;
            (lifecycle.generation.clone(), lifecycle.worker_connection())
        };
        let deadline = wait.map(|wait| Instant::now() + wait);
        if let Some(deadline) = deadline {
            pending(deadline, &mut cancelled, self.ready()).await??;
        } else {
            self.startup_result()
                .borrow()
                .clone()
                .ok_or(IntegratorError::Busy)??;
        }
        // Readiness retains successful configuration even when prelaunch failed.
        // MCP owns that failure flag and its output; an integrator must neither
        // consume them nor admit a cell that the evaluator would skip as failed.
        if self
            .0
            .startup_failed
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(IntegratorError::Failed(
                "worker startup failed; code was not run".into(),
            ));
        }
        self.validate_cell(&cell)?;
        let mut first_attempt = true;
        let mut waiting_recorded = false;
        let (evaluation, claim) = loop {
            // Register before inspecting the slot; transport acknowledgment can
            // release it even when no subsequent MCP call reaps the old owner.
            let changed = self.0.admission_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let operation = match deadline {
                Some(deadline) => {
                    pending(deadline, &mut cancelled, self.0.admission.read()).await?
                }
                None => self
                    .0
                    .admission
                    .try_read()
                    .map_err(|_| IntegratorError::Busy)?,
            };
            let preparation = match deadline {
                Some(deadline) => {
                    pending(deadline, &mut cancelled, self.0.preparation.read()).await?
                }
                None => self
                    .0
                    .preparation
                    .try_read()
                    .map_err(|_| IntegratorError::Busy)?,
            };
            self.ensure_generation(&generation)?;
            let bootstrap = {
                let lifecycle = self
                    .0
                    .lifecycle
                    .lock()
                    .map_err(|_| "worker lifecycle lock poisoned".to_string())?;
                let current = lifecycle.worker_connection();
                // Automatic recovery can replace a process without replacing
                // its logical generation. Queued source still targets the
                // original interpreter state; initial startup may attach once.
                if let Some(expected) = &connection {
                    if current
                        .as_ref()
                        .is_none_or(|current| !current.is_same_connection(expected))
                    {
                        return Err(IntegratorError::Failed("worker connection changed before integrator admission; code was not run".into()));
                    }
                } else {
                    connection = current;
                }
                lifecycle.worker_bootstrapping()?
            };
            let busy = self.current_evaluation()?.is_some() || self.admit().is_err() || bootstrap;
            if !busy {
                if *cancelled.borrow() {
                    return Err(IntegratorError::Cancelled);
                }
                // Tokio timeouts can return an already-ready future after their
                // deadline. Only a zero-duration first attempt is an idle probe.
                if deadline.is_some_and(|deadline| Instant::now() >= deadline)
                    && (wait != Some(Duration::ZERO) || !first_attempt)
                {
                    return Err(IntegratorError::Timeout);
                }
                let result = self.start_evaluation_owned(
                    crate::cell::Cell {
                        language: cell.language,
                        source: cell.source.clone(),
                    },
                    None,
                    generation.clone(),
                    transcript.clone(),
                    call_id,
                    None,
                    &mut None,
                    None,
                    Origin::Integrator,
                    Some(observation.clone()),
                );
                match result {
                    Ok(started) => break started,
                    Err(EvaluationStartError::Busy(_)) => {}
                    Err(EvaluationStartError::Failed(error)) => return Err(error.into()),
                }
            }
            drop(preparation);
            drop(operation);
            let Some(deadline) = deadline else {
                return Err(IntegratorError::Busy);
            };
            first_attempt = false;
            if !waiting_recorded {
                transcript.protocol(
                    "integrator_waiting",
                    serde_json::json!({"call_id": call_id}),
                );
                waiting_recorded = true;
            }
            pending(deadline, &mut cancelled, changed).await?;
        };

        // Admission is synchronous above. Install the retained owner before the
        // next await, so cancellation cannot leave a sideband cell pollable by MCP.
        let evaluation_id = evaluation.id();
        let client = self.clone();
        let collector = tokio::spawn(async move {
            let result = evaluation.wait_terminal(claim).await;
            let mut result = result.map(|wait| {
                super::output::render_response(super::send::send_response_from_wait(wait))
            });
            let mut active = client.0.evaluation.lock().expect("worker evaluation lock");
            evaluation.restore_idle_prelude();
            if let Ok(response) = &mut result {
                response.settle_delivery();
            }
            if active
                .as_ref()
                .is_some_and(|current| std::sync::Arc::ptr_eq(&current.evaluation, &evaluation))
            {
                *active = None;
            }
            client.0.admission_changed.notify_waiters();
            result
        });
        collector
            .await
            .map_err(|error| {
                IntegratorError::Failed(format!("integrator evaluation task failed: {error}"))
            })?
            .map(|response| {
                (
                    response,
                    evaluation_id,
                    self.0.output.typed_output(Some(evaluation_id)),
                )
            })
            .map_err(IntegratorError::Failed)
    }

    pub(crate) fn integrator_status(&self) -> serde_json::Value {
        let Ok(mut active) = self.0.evaluation.try_lock() else {
            return serde_json::json!({"state": "busy", "idle": false, "evaluation": null});
        };
        if active.as_ref().is_some_and(|active| {
            active
                .evaluation
                .reap_delivered_completion()
                .unwrap_or(false)
        }) {
            *active = None;
            self.0.admission_changed.notify_waiters();
        }
        let Ok(lifecycle) = self.0.lifecycle.try_lock() else {
            return serde_json::json!({"state": "busy", "idle": false, "evaluation": null});
        };
        let (state, evaluation) = match lifecycle.state {
            LifecycleState::Restarting { .. } => ("restarting", serde_json::Value::Null),
            LifecycleState::ShuttingDown { .. } => ("shutting_down", serde_json::Value::Null),
            LifecycleState::Ready => match active.as_ref() {
                Some(active) => {
                    let (state, started) = active.evaluation.observation();
                    (
                        state,
                        serde_json::json!({
                            "id": active.evaluation.id(),
                            "origin": if active.evaluation.is_integrator() { "integrator" } else { "mcp" },
                            "language": active.language,
                            "started": started,
                        }),
                    )
                }
                None if !self.startup_finished() || lifecycle.starting() => {
                    ("starting", serde_json::Value::Null)
                }
                None if self
                    .startup_result()
                    .borrow()
                    .as_ref()
                    .is_some_and(Result::is_err)
                    || self
                        .0
                        .startup_failed
                        .load(std::sync::atomic::Ordering::Acquire) =>
                {
                    ("failed", serde_json::Value::Null)
                }
                None if self.0.preparation.try_read().is_err() => {
                    ("preparing", serde_json::Value::Null)
                }
                None => (
                    lifecycle.integrator_phase().unwrap_or("idle"),
                    serde_json::Value::Null,
                ),
            },
        };
        serde_json::json!({"state": state, "idle": state == "idle", "evaluation": evaluation})
    }

    pub(crate) fn integrator_preview(
        &self,
        writer: crate::transcript::WriterState,
    ) -> Result<Vec<Content>, IntegratorError> {
        let active = self
            .0
            .evaluation
            .lock()
            .map_err(|_| IntegratorError::Busy)?;
        match active.as_ref() {
            Some(active) => active
                .evaluation
                .preview(writer)
                .ok_or(IntegratorError::Busy),
            None => Ok(self.0.output.preview(writer)),
        }
    }

    pub(crate) fn inherit_integrator_output(&self, source: &Self) {
        if source.0.output.history_enabled() {
            self.enable_integrator_output();
        }
    }

    pub(crate) fn integrator_worker_id(&self) -> u64 {
        self.0.output.worker_id()
    }

    pub(crate) fn enable_integrator_output(&self) {
        self.0.output.enable_history();
    }

    pub(crate) fn integrator_output(&self) -> serde_json::Value {
        self.0.output.typed_output(None)
    }

    pub(crate) fn integrator_subscribe(&self) -> tokio::sync::watch::Receiver<()> {
        self.0.output.subscribe()
    }
}

async fn pending<T>(
    deadline: Instant,
    cancelled: &mut tokio::sync::watch::Receiver<bool>,
    future: impl std::future::Future<Output = T>,
) -> Result<T, IntegratorError> {
    if *cancelled.borrow() {
        return Err(IntegratorError::Cancelled);
    }
    tokio::select! {
        biased;
        _ = cancelled.changed() => Err(IntegratorError::Cancelled),
        result = tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), future) => result.map_err(|_| IntegratorError::Timeout),
    }
}
