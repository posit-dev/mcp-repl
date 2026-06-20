use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use crate::completion_reply::{CompletionInfo, InputFallback};
use crate::ipc::{IpcWaitError, ServerIpcConnection};
use crate::output_capture::{OutputTextSource, update_last_reply_marker_offset_max};
use crate::oversized_output::OversizedOutputMode;
use crate::pending_output_tape::PendingSidebandKind;
use crate::reply_presentation::build_input_transcript;

use super::backend_driver::output_echo_source_for_backend;
use super::{WorkerError, WorkerManager};

pub(super) const REQUEST_COMPLETION_STABLE_WAIT: Duration = Duration::from_millis(20);
const COMPLETION_METADATA_SETTLE_MAX: Duration = Duration::from_millis(30);
const COMPLETION_METADATA_SETTLE_POLL: Duration = Duration::from_millis(5);
const COMPLETION_METADATA_STABLE: Duration = Duration::from_millis(10);
const OUTPUT_READER_QUIESCE_GRACE: Duration = Duration::from_millis(120);
const OUTPUT_READER_COMPLETION_STABLE: Duration = if cfg!(target_os = "macos") {
    Duration::from_millis(80)
} else {
    Duration::from_millis(15)
};
const OUTPUT_READER_TIMEOUT_SETTLE_MAX: Duration = Duration::from_millis(900);

pub(super) struct RequestState {
    pub(super) timeout: Duration,
    pub(super) started_at: Instant,
}

fn collect_completion_metadata(ipc: &ServerIpcConnection) -> (Option<String>, Vec<String>) {
    let mut prompt = ipc.try_take_prompt();
    let mut prompt_variants = ipc.take_prompt_history();
    let mut echo_event_count = ipc.pending_echo_event_count();
    let mut saw_late_echo_event = false;

    let start = Instant::now();
    let mut stable_for = Duration::from_millis(0);
    while start.elapsed() < COMPLETION_METADATA_SETTLE_MAX {
        thread::sleep(COMPLETION_METADATA_SETTLE_POLL);
        let next_prompt = ipc.try_take_prompt();
        let mut next_prompt_variants = ipc.take_prompt_history();
        let next_echo_event_count = ipc.pending_echo_event_count();
        if next_echo_event_count > echo_event_count {
            saw_late_echo_event = true;
        }
        let changed = next_prompt.is_some()
            || !next_prompt_variants.is_empty()
            || next_echo_event_count != echo_event_count;

        if let Some(value) = next_prompt {
            prompt = Some(value);
        }
        prompt_variants.append(&mut next_prompt_variants);
        echo_event_count = next_echo_event_count;

        if changed {
            stable_for = Duration::from_millis(0);
        } else {
            stable_for = stable_for.saturating_add(COMPLETION_METADATA_SETTLE_POLL);
            if !saw_late_echo_event && stable_for >= COMPLETION_METADATA_STABLE {
                break;
            }
        }
    }

    if prompt.is_none() {
        prompt = prompt_variants
            .iter()
            .rev()
            .find(|value| !value.is_empty())
            .cloned();
    }

    (prompt, prompt_variants)
}

pub(super) fn completion_info_from_ipc(
    ipc: &ServerIpcConnection,
    session_end_seen: bool,
    echo_source: OutputTextSource,
) -> CompletionInfo {
    let (prompt, prompt_variants) = if session_end_seen {
        (None, None)
    } else {
        let (prompt, prompt_variants) = collect_completion_metadata(ipc);
        (prompt, Some(prompt_variants))
    };

    let mut echo_events = ipc.take_echo_events();
    for event in &mut echo_events {
        event.source = echo_source;
    }

    CompletionInfo {
        prompt,
        prompt_variants,
        echo_events,
        protocol_warnings: ipc.take_protocol_warnings(),
        session_end_seen,
    }
}

impl WorkerManager {
    pub(super) fn send_worker_request(
        &mut self,
        text: String,
        worker_timeout: Duration,
        server_timeout: Duration,
    ) -> Result<RequestState, WorkerError> {
        let text = self.driver.prepare_input_text(text);
        let started_at = Instant::now();
        let prompt = self.current_prompt_hint();
        self.remember_prompt(prompt);
        self.pending_request_input = Some(text.clone());
        let ipc = self
            .process
            .as_ref()
            .and_then(|process| process.ipc_connection())
            .ok_or_else(|| WorkerError::Protocol("worker ipc unavailable".to_string()))?;
        if server_timeout.is_zero() {
            return Err(WorkerError::Timeout(server_timeout));
        }
        let server_deadline = started_at + server_timeout;
        let remaining = server_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(WorkerError::Timeout(server_timeout));
        }
        if let Some(process) = self.process.as_ref() {
            process.note_accepted_input_starting();
        }
        self.driver.on_input_start(&text, &ipc, remaining)?;
        self.settled_pending_completion = None;
        self.guardrail.busy.store(true, Ordering::Relaxed);
        let remaining = server_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(WorkerError::Timeout(server_timeout));
        }
        Ok(RequestState {
            timeout: worker_timeout,
            started_at,
        })
    }

    pub(super) fn wait_for_request_completion(
        &mut self,
        timeout: Duration,
    ) -> Result<CompletionInfo, WorkerError> {
        let Some(process) = self.process.as_ref() else {
            return Err(WorkerError::Protocol(
                "worker process unavailable".to_string(),
            ));
        };
        let ipc = process
            .ipc_connection()
            .ok_or_else(|| WorkerError::Protocol("worker ipc unavailable".to_string()))?;
        let start = Instant::now();
        let mut result = self.driver.wait_for_completion(timeout, ipc.clone());
        if matches!(
            &result,
            Err(WorkerError::Protocol(message))
                if message.contains("ipc disconnected while waiting for request completion")
        ) {
            crate::diagnostics::startup_log(
                "worker-request: ipc disconnected while waiting for completion; checking process",
            );
            let deadline = Instant::now() + Duration::from_millis(500);
            let mut worker_exited = self.process.is_none();
            while !worker_exited {
                worker_exited = match self.process.as_mut() {
                    Some(process) => !process.is_running()?,
                    None => true,
                };
                if worker_exited || Instant::now() >= deadline {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            if worker_exited {
                crate::diagnostics::startup_log(
                    "worker-request: treating disconnected exited worker as session end",
                );
                result = Ok(CompletionInfo {
                    prompt: None,
                    prompt_variants: None,
                    echo_events: Vec::new(),
                    protocol_warnings: ipc.take_protocol_warnings(),
                    session_end_seen: true,
                });
            } else {
                crate::diagnostics::startup_log(
                    "worker-request: disconnected worker did not exit during grace period",
                );
            }
        }
        // Best-effort: after IPC completion, give the output reader threads a brief window to
        // drain any bytes already written by the worker before we snapshot the ring.
        let elapsed = start.elapsed();
        let remaining = timeout.saturating_sub(elapsed);
        if result.is_ok() {
            self.pending_output_tape
                .append_sideband(PendingSidebandKind::RequestBoundary);
        }
        self.settle_output_after_completion(remaining);
        if result.is_ok()
            && let Some(message) = ipc.take_protocol_error()
        {
            return Err(WorkerError::Protocol(message));
        }
        if self.guardrail_event_pending() {
            let event = self
                .guardrail
                .event
                .lock()
                .expect("guardrail event mutex poisoned")
                .take()
                .expect("guardrail event should be present");
            return Err(WorkerError::Guardrail(event.message));
        }
        result
    }

    pub(super) fn settle_output_after_completion(&self, budget: Duration) {
        let total = budget.min(OUTPUT_READER_QUIESCE_GRACE);
        if total.is_zero() {
            return;
        }
        let stable_needed = OUTPUT_READER_COMPLETION_STABLE.min(total);
        self.settle_output_until_stable(total, stable_needed);
    }

    pub(super) fn wait_for_late_files_output_after_settled_completion(&self, budget: Duration) {
        if self.pending_output_tape.has_pending() {
            return;
        }
        let total = budget.min(OUTPUT_READER_TIMEOUT_SETTLE_MAX);
        if total.is_zero() {
            return;
        }

        let poll = Duration::from_millis(5);
        let start = Instant::now();
        while start.elapsed() < total {
            let remaining = total.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                break;
            }
            thread::sleep(poll.min(remaining));
            if self.pending_output_tape.has_pending() {
                self.settle_output_after_completion(total.saturating_sub(start.elapsed()));
                return;
            }
        }
    }

    pub(super) fn settle_output_after_timeout(&self) {
        let total = OUTPUT_READER_TIMEOUT_SETTLE_MAX;
        let stable_needed = Duration::from_millis(40);
        let poll = Duration::from_millis(5);
        let start = Instant::now();
        let baseline = self.pending_output_tape.current_settle_state();
        let mut last_seq = baseline.progress_seq;
        let mut ready = baseline.has_image;
        let mut stable_for = Duration::from_millis(0);
        while start.elapsed() < total {
            thread::sleep(poll);
            let now = self.pending_output_tape.current_settle_state();
            if !ready
                && (now.has_image || now.readline_results_seen > baseline.readline_results_seen)
            {
                ready = true;
                stable_for = Duration::from_millis(0);
                last_seq = now.progress_seq;
                continue;
            }
            if now.progress_seq == last_seq {
                stable_for = stable_for.saturating_add(poll);
                if ready && stable_for >= stable_needed {
                    return;
                }
            } else {
                last_seq = now.progress_seq;
                stable_for = Duration::from_millis(0);
            }
        }
    }

    pub(super) fn should_settle_output_after_timeout(&self) -> bool {
        self.driver.should_settle_output_after_timeout(
            self.oversized_output,
            self.pending_request_input.as_deref(),
        )
    }

    fn settle_output_until_stable(&self, total: Duration, stable_needed: Duration) {
        if total.is_zero() {
            return;
        }
        let poll = Duration::from_millis(5);
        let start = Instant::now();

        let mut last = match self.oversized_output {
            OversizedOutputMode::Files => self.pending_output_tape.current_seq(),
            OversizedOutputMode::Pager => self.output.end_offset().unwrap_or(0),
        };
        let mut stable_for = Duration::from_millis(0);
        while start.elapsed() < total {
            thread::sleep(poll);
            let now = match self.oversized_output {
                OversizedOutputMode::Files => self.pending_output_tape.current_seq(),
                OversizedOutputMode::Pager => self.output.end_offset().unwrap_or(0),
            };
            if now == last {
                stable_for = stable_for.saturating_add(poll);
                if stable_for >= stable_needed {
                    return;
                }
            } else {
                last = now;
                stable_for = Duration::from_millis(0);
            }
        }
    }

    pub(super) fn resolve_timeout_marker(&mut self) {
        self.resolve_timeout_marker_with_wait(Duration::from_millis(0));
    }

    pub fn refresh_timeout_marker(&mut self) {
        self.resolve_timeout_marker();
    }

    pub(super) fn resolve_timeout_marker_with_wait(&mut self, wait: Duration) {
        if !self.pending_request {
            return;
        }
        if self.settled_pending_error.is_some() {
            return;
        }
        let Some(ipc) = self
            .process
            .as_ref()
            .and_then(|process| process.ipc_connection())
        else {
            return;
        };
        let status = if wait.is_zero() {
            ipc.wait_for_request_completion(Duration::ZERO, REQUEST_COMPLETION_STABLE_WAIT)
        } else {
            ipc.wait_for_request_completion(wait, REQUEST_COMPLETION_STABLE_WAIT)
        };
        match status {
            Ok(()) => {
                let mut settled_completion = completion_info_from_ipc(
                    &ipc,
                    false,
                    output_echo_source_for_backend(self.backend),
                );
                self.pending_output_tape
                    .append_sideband(PendingSidebandKind::RequestBoundary);
                self.settle_output_after_completion(Duration::from_millis(120));
                if matches!(self.oversized_output, OversizedOutputMode::Pager) {
                    update_last_reply_marker_offset_max(self.output.end_offset().unwrap_or(0));
                }
                let worker_exited = match self.process.as_mut() {
                    Some(process) => match process.is_running() {
                        Ok(running) => !running,
                        Err(_) => false,
                    },
                    None => true,
                };
                self.clear_pending_request_state();
                if worker_exited {
                    settled_completion.session_end_seen = true;
                    self.note_session_end(true);
                } else {
                    self.remember_prompt(settled_completion.prompt.clone());
                }
                self.settled_pending_completion = Some(settled_completion);
            }
            Err(IpcWaitError::SessionEnd) => {
                self.settle_pending_session_end(&ipc);
            }
            Err(IpcWaitError::Protocol(message)) => {
                self.driver.clear_active_input();
                self.settled_pending_error = Some(WorkerError::Protocol(message));
            }
            Err(IpcWaitError::Timeout | IpcWaitError::Disconnected) => {
                let worker_exited = self
                    .process
                    .as_mut()
                    .and_then(|process| process.is_running().ok())
                    .is_some_and(|running| !running);
                if worker_exited {
                    self.settle_pending_session_end(&ipc);
                }
            }
        }
    }

    pub(super) fn settle_pending_session_end(&mut self, ipc: &ServerIpcConnection) {
        let settled_completion =
            completion_info_from_ipc(ipc, true, output_echo_source_for_backend(self.backend));
        self.pending_output_tape
            .append_sideband(PendingSidebandKind::RequestBoundary);
        self.settle_output_after_completion(Duration::from_millis(120));
        self.note_session_end(true);
        self.clear_pending_request_state();
        self.settled_pending_completion = Some(settled_completion);
    }

    pub(super) fn clear_pending_request_state(&mut self) {
        self.clear_pending_request_state_with_active_turn(false);
    }

    pub(super) fn clear_pending_request_state_with_active_turn(&mut self, preserve: bool) {
        self.pending_request = false;
        self.pending_request_started_at = None;
        if !preserve {
            self.driver.clear_active_input();
        }
        self.settled_pending_completion = None;
        self.settled_pending_error = None;
        self.guardrail.busy.store(false, Ordering::Relaxed);
    }

    pub(super) fn take_input_fallback(&mut self, completion: &CompletionInfo) -> InputFallback {
        let raw_input = completion
            .echo_events
            .is_empty()
            .then(|| self.pending_request_input.take())
            .flatten();
        let transcript = raw_input
            .as_deref()
            .and_then(|input| build_input_transcript(completion.prompt.as_deref(), input));
        InputFallback {
            transcript,
            raw_input,
        }
    }
}
