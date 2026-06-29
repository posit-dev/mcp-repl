use std::time::{Duration, Instant};

use super::{WorkerError, WorkerManager};
use crate::completion_reply::{PagerCompletionPrompt, ReplyWithOffset, timeout_status_content};
use crate::ipc::{IpcInputReadiness, IpcWaitError, ServerToWorkerIpcMessage};
use crate::output_snapshot::{SnapshotWithImages, snapshot_page_with_images};
use crate::pager;
use crate::pending_output_tape::FormattedPendingOutput;
use crate::reply_presentation::{
    normalize_prompt, reconcile_polled_completion_prompt, reconcile_trailing_completion_prompt,
};
use crate::sandbox::SandboxStateUpdate;
use crate::worker_protocol::{WorkerErrorCode, WorkerReply};

#[derive(Clone, Copy)]
enum InterruptMode {
    Files,
    Pager,
}

#[derive(Clone, Copy)]
enum ResolvedInterruptMode {
    Files,
    Pager { page_bytes: u64 },
}

struct InterruptPromptWait {
    timed_out: bool,
    prompt: Option<String>,
}

pub(super) const INTERRUPT_TAIL_SETTLE_WINDOW: Duration = Duration::from_millis(50);
const INTERRUPT_ACK_TIMEOUT: Duration = Duration::from_millis(100);

impl WorkerManager {
    pub(super) fn interrupt_files(
        &mut self,
        timeout: Duration,
        deferred_sandbox_state_update: Option<SandboxStateUpdate>,
        suppress_session_end_reset: bool,
    ) -> Result<WorkerReply, WorkerError> {
        self.interrupt_files_with_prompt_wait(
            timeout,
            deferred_sandbox_state_update,
            suppress_session_end_reset,
            true,
        )
    }

    pub(super) fn interrupt_files_for_tail(
        &mut self,
        timeout: Duration,
        deferred_sandbox_state_update: Option<SandboxStateUpdate>,
        suppress_session_end_reset: bool,
    ) -> Result<WorkerReply, WorkerError> {
        self.interrupt_files_with_prompt_wait(
            timeout,
            deferred_sandbox_state_update,
            suppress_session_end_reset,
            false,
        )
    }

    fn interrupt_files_with_prompt_wait(
        &mut self,
        timeout: Duration,
        deferred_sandbox_state_update: Option<SandboxStateUpdate>,
        suppress_session_end_reset: bool,
        wait_for_prompt: bool,
    ) -> Result<WorkerReply, WorkerError> {
        self.interrupt_for_mode(
            InterruptMode::Files,
            timeout,
            deferred_sandbox_state_update,
            suppress_session_end_reset,
            wait_for_prompt,
        )
    }

    pub(super) fn interrupt_pager(
        &mut self,
        timeout: Duration,
        deferred_sandbox_state_update: Option<SandboxStateUpdate>,
        suppress_session_end_reset: bool,
    ) -> Result<WorkerReply, WorkerError> {
        self.interrupt_pager_with_prompt_wait(
            timeout,
            deferred_sandbox_state_update,
            suppress_session_end_reset,
            true,
        )
    }

    pub(super) fn interrupt_pager_for_tail(
        &mut self,
        timeout: Duration,
        deferred_sandbox_state_update: Option<SandboxStateUpdate>,
        suppress_session_end_reset: bool,
    ) -> Result<WorkerReply, WorkerError> {
        self.interrupt_pager_with_prompt_wait(
            timeout,
            deferred_sandbox_state_update,
            suppress_session_end_reset,
            false,
        )
    }

    fn interrupt_pager_with_prompt_wait(
        &mut self,
        timeout: Duration,
        deferred_sandbox_state_update: Option<SandboxStateUpdate>,
        suppress_session_end_reset: bool,
        wait_for_prompt: bool,
    ) -> Result<WorkerReply, WorkerError> {
        self.interrupt_for_mode(
            InterruptMode::Pager,
            timeout,
            deferred_sandbox_state_update,
            suppress_session_end_reset,
            wait_for_prompt,
        )
    }

    fn interrupt_for_mode(
        &mut self,
        mode: InterruptMode,
        timeout: Duration,
        deferred_sandbox_state_update: Option<SandboxStateUpdate>,
        suppress_session_end_reset: bool,
        wait_for_prompt: bool,
    ) -> Result<WorkerReply, WorkerError> {
        Self::begin_interrupt(timeout);
        let deadline = Instant::now() + timeout;
        let interrupt_drains_existing_completion =
            self.pending_request || self.settled_pending_completion.is_some();
        let interrupt_sent_at = self.interrupt_worker_if_running(remaining_until(deadline))?;
        let mode = self.resolve_interrupt_mode(mode);

        if interrupt_drains_existing_completion {
            return self.drain_existing_completion_after_interrupt(
                mode,
                remaining_until(deadline),
                deferred_sandbox_state_update,
                suppress_session_end_reset,
            );
        }

        let prompt_wait = if wait_for_prompt {
            self.wait_for_interrupt_prompt(remaining_until(deadline), interrupt_sent_at)?
        } else {
            InterruptPromptWait {
                timed_out: false,
                prompt: None,
            }
        };
        let timed_out = prompt_wait.timed_out;
        let reply = self.build_interrupt_reply_for_mode(mode, prompt_wait, timeout);
        let session_end = self.session_end_seen;
        Self::end_interrupt(timed_out, session_end);
        let reply = self.finalize_reply(reply);
        self.maybe_reset_after_session_end_with_options(
            deferred_sandbox_state_update,
            suppress_session_end_reset,
            false,
        )?;
        Ok(reply)
    }

    fn resolve_interrupt_mode(&self, mode: InterruptMode) -> ResolvedInterruptMode {
        match mode {
            InterruptMode::Files => ResolvedInterruptMode::Files,
            InterruptMode::Pager => ResolvedInterruptMode::Pager {
                page_bytes: pager::resolve_page_bytes(None),
            },
        }
    }

    fn begin_interrupt(timeout: Duration) {
        crate::event_log::log(
            "worker_interrupt_begin",
            serde_json::json!({
                "timeout_ms": timeout.as_millis(),
            }),
        );
    }

    fn end_interrupt(timed_out: bool, session_end: bool) {
        crate::event_log::log(
            "worker_interrupt_end",
            serde_json::json!({
                "timed_out": timed_out,
                "session_end": session_end,
            }),
        );
    }

    fn interrupt_target_running(&mut self) -> Result<bool, WorkerError> {
        match self.process.as_mut() {
            Some(process) => process.is_running(),
            None => Ok(false),
        }
    }

    fn interrupt_worker_if_running(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<Instant>, WorkerError> {
        if !self.interrupt_target_running()? {
            return Ok(None);
        }

        let process = self
            .process
            .as_mut()
            .expect("worker process should be available");
        match send_ordered_interrupt(process, timeout) {
            Ok(interrupt_sent_at) => Ok(Some(interrupt_sent_at)),
            Err(err) => {
                self.reset()?;
                crate::event_log::log(
                    "worker_interrupt_error",
                    serde_json::json!({
                        "error": err.to_string(),
                    }),
                );
                Err(err)
            }
        }
    }

    fn drain_existing_completion_after_interrupt(
        &mut self,
        mode: ResolvedInterruptMode,
        timeout: Duration,
        deferred_sandbox_state_update: Option<SandboxStateUpdate>,
        suppress_session_end_reset: bool,
    ) -> Result<WorkerReply, WorkerError> {
        let mut reply = match mode {
            ResolvedInterruptMode::Files => self.poll_pending_output_files(timeout)?,
            ResolvedInterruptMode::Pager { page_bytes } => {
                self.poll_pending_output_pager(timeout, page_bytes)?
            }
        };
        let pager_active = self.pager.is_active();
        let prompt = match &reply.reply {
            WorkerReply::Output { prompt, .. } => prompt.clone(),
        };
        let WorkerReply::Output { contents, .. } = &mut reply.reply;
        match mode {
            ResolvedInterruptMode::Files => {
                reconcile_polled_completion_prompt(contents, prompt, self.backend);
            }
            ResolvedInterruptMode::Pager { .. } if !pager_active => {
                reconcile_polled_completion_prompt(contents, prompt, self.backend);
            }
            ResolvedInterruptMode::Pager { .. } => {}
        }
        let reply = self.finalize_reply(reply);
        self.maybe_reset_after_session_end_with_options(
            deferred_sandbox_state_update,
            suppress_session_end_reset,
            false,
        )?;
        Ok(reply)
    }

    fn wait_for_interrupt_prompt(
        &mut self,
        timeout: Duration,
        interrupt_sent_at: Option<Instant>,
    ) -> Result<InterruptPromptWait, WorkerError> {
        let mut timed_out = false;
        let mut prompt: Option<String> = None;
        if let Some(process) = self.process.as_ref()
            && let Some(ipc) = process.ipc_connection()
        {
            if timeout.is_zero() {
                timed_out = true;
            } else {
                let readiness = match interrupt_sent_at {
                    Some(sent_at) => ipc.wait_for_input_wait_or_fresh_ready(timeout, sent_at),
                    None => ipc.wait_for_input_readiness(timeout),
                };
                match readiness {
                    Ok(IpcInputReadiness::InputWait(value)) => {
                        prompt = Some(value);
                    }
                    Ok(IpcInputReadiness::Ready) => {
                        prompt = None;
                    }
                    Err(IpcWaitError::Timeout) => {
                        timed_out = true;
                    }
                    Err(IpcWaitError::SessionEnd) => {
                        self.note_session_end(true);
                    }
                    Err(IpcWaitError::Disconnected) => {}
                    Err(IpcWaitError::Protocol(message)) => {
                        return Err(WorkerError::Protocol(message));
                    }
                }
            }
        }

        Ok(InterruptPromptWait { timed_out, prompt })
    }

    fn build_interrupt_reply_for_mode(
        &mut self,
        mode: ResolvedInterruptMode,
        prompt_wait: InterruptPromptWait,
        timeout: Duration,
    ) -> ReplyWithOffset {
        match mode {
            ResolvedInterruptMode::Files => self.build_interrupt_reply_files(prompt_wait, timeout),
            ResolvedInterruptMode::Pager { page_bytes } => {
                self.build_interrupt_reply_pager(prompt_wait, timeout, page_bytes)
            }
        }
    }

    fn build_interrupt_reply_files(
        &mut self,
        prompt_wait: InterruptPromptWait,
        timeout: Duration,
    ) -> ReplyWithOffset {
        let FormattedPendingOutput {
            mut contents,
            saw_stderr,
        } = self.drain_formatted_output();
        let is_error = saw_stderr;

        if prompt_wait.timed_out {
            contents.push(timeout_status_content(timeout));
        }

        let session_end = self.session_end_seen;
        let raw_prompt = if session_end || prompt_wait.timed_out {
            None
        } else {
            prompt_wait.prompt
        };
        let resolved_prompt = normalize_prompt(raw_prompt.clone());
        let prompt_to_remember = if !session_end && !prompt_wait.timed_out {
            normalize_prompt(raw_prompt)
        } else {
            raw_prompt
        };
        self.remember_prompt(prompt_to_remember);
        if !session_end && !prompt_wait.timed_out {
            reconcile_trailing_completion_prompt(
                &mut contents,
                resolved_prompt.clone(),
                self.backend,
            );
        }

        ReplyWithOffset {
            reply: WorkerReply::Output {
                contents,
                is_error,
                error_code: prompt_wait.timed_out.then_some(WorkerErrorCode::Timeout),
                prompt: (!session_end).then_some(()).and(resolved_prompt),
                prompt_variants: None,
            },
        }
    }

    fn build_interrupt_reply_pager(
        &mut self,
        prompt_wait: InterruptPromptWait,
        timeout: Duration,
        page_bytes: u64,
    ) -> ReplyWithOffset {
        if !prompt_wait.timed_out {
            self.output_timeline.seal_utf8_tails();
        }
        let start_offset = self.output.current_offset().unwrap_or(0);
        let mut end_offset = self.output.end_offset().unwrap_or(start_offset);
        if end_offset < start_offset {
            end_offset = start_offset;
        }

        let is_error = self
            .output
            .saw_stderr_in_range(start_offset.min(end_offset), end_offset);
        let SnapshotWithImages {
            mut contents,
            pages_left,
            buffer,
            last_range,
        } = snapshot_page_with_images(&self.output, end_offset, page_bytes);

        if prompt_wait.timed_out {
            contents.push(timeout_status_content(timeout));
        }

        pager::maybe_activate_and_append_footer(
            &mut self.pager,
            &mut contents,
            pages_left,
            is_error,
            buffer,
            last_range,
        );

        let session_end = self.session_end_seen;
        let raw_prompt = if session_end || prompt_wait.timed_out {
            None
        } else {
            prompt_wait.prompt
        };
        let resolved_prompt = normalize_prompt(raw_prompt.clone());
        let prompt_to_remember = if !session_end && !prompt_wait.timed_out {
            normalize_prompt(raw_prompt)
        } else {
            raw_prompt
        };
        self.remember_prompt(prompt_to_remember);
        if self.pager.is_active() && !session_end {
            self.pager_prompt = Some(PagerCompletionPrompt::from_prompt(resolved_prompt.clone()));
        }
        if !session_end && !prompt_wait.timed_out && !self.pager.is_active() {
            reconcile_trailing_completion_prompt(
                &mut contents,
                resolved_prompt.clone(),
                self.backend,
            );
        }

        ReplyWithOffset {
            reply: WorkerReply::Output {
                contents,
                is_error,
                error_code: prompt_wait.timed_out.then_some(WorkerErrorCode::Timeout),
                prompt: (!self.pager.is_active() && !session_end)
                    .then_some(())
                    .and(resolved_prompt),
                prompt_variants: None,
            },
        }
    }
}

fn send_ordered_interrupt(
    process: &mut crate::worker_supervisor::WorkerProcess,
    timeout: Duration,
) -> Result<Instant, WorkerError> {
    if let Some(ipc) = process.ipc_connection() {
        ipc.note_interrupt_sent();
        let ack_wait_since = Instant::now();
        match ipc.send(ServerToWorkerIpcMessage::Interrupt {}) {
            Ok(()) => {
                let ack_timeout = timeout.min(INTERRUPT_ACK_TIMEOUT);
                match ipc.wait_for_fresh_interrupt_ack(ack_timeout, ack_wait_since) {
                    Ok(Some(ack)) => {
                        crate::event_log::log(
                            "worker_interrupt_ack_observed",
                            serde_json::json!({
                                "discarded_input": ack.discarded_input,
                                "elapsed_ms": ack_wait_since.elapsed().as_millis(),
                            }),
                        );
                    }
                    Ok(None) => {
                        crate::event_log::log(
                            "worker_interrupt_ack_timeout",
                            serde_json::json!({
                                "timeout_ms": ack_timeout.as_millis(),
                            }),
                        );
                    }
                    Err(IpcWaitError::Protocol(message)) => {
                        crate::event_log::log(
                            "worker_interrupt_ack_wait_error",
                            serde_json::json!({
                                "error": format!("protocol: {message}"),
                            }),
                        );
                        return Err(WorkerError::Protocol(message));
                    }
                    Err(err) => {
                        crate::event_log::log(
                            "worker_interrupt_ack_wait_error",
                            serde_json::json!({
                                "error": ipc_wait_error_message(&err),
                            }),
                        );
                    }
                }
            }
            Err(err) => {
                crate::event_log::log(
                    "worker_interrupt_sideband_send_error",
                    serde_json::json!({
                        "error": err.to_string(),
                    }),
                );
            }
        }
    }

    let os_interrupt_sent_at = Instant::now();
    process.send_interrupt()?;
    crate::event_log::log(
        "worker_interrupt_os_sent",
        serde_json::json!({
            "after_sideband_cleanup": true,
        }),
    );
    Ok(os_interrupt_sent_at)
}

fn remaining_until(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

fn ipc_wait_error_message(err: &IpcWaitError) -> String {
    match err {
        IpcWaitError::Timeout => "timeout".to_string(),
        IpcWaitError::SessionEnd => "session_end".to_string(),
        IpcWaitError::Disconnected => "disconnected".to_string(),
        IpcWaitError::Protocol(message) => format!("protocol: {message}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Backend;
    use crate::completion_reply::CompletionInfo;
    use crate::oversized_output::OversizedOutputMode;
    use crate::sandbox_cli::SandboxCliPlan;
    use crate::worker_process::test_support::contents_text;

    #[test]
    fn interrupt_files_drains_settled_completion_preserving_matching_output() {
        let mut manager = WorkerManager::new(
            Backend::Python,
            SandboxCliPlan::default(),
            OversizedOutputMode::Files,
        )
        .expect("worker manager");
        manager
            .pending_output_tape
            .append_stdout_bytes(b">>> import time; time.sleep(0.07)\nDETACHED_OK\n");
        manager.pending_request_input = Some("import time; time.sleep(0.07)\n".to_string());
        manager.settled_pending_completion = Some(CompletionInfo {
            prompt: Some(">>> ".to_string()),
            prompt_variants: Some(vec![">>> ".to_string()]),
            protocol_warnings: Vec::new(),
            session_end_seen: false,
        });

        let WorkerReply::Output { contents, .. } = manager
            .interrupt(Duration::from_millis(10), None, false)
            .expect("interrupt reply");
        let text = contents_text(&contents);

        assert!(
            text.contains("DETACHED_OK\n"),
            "expected the settled completion output to be preserved, got: {text:?}"
        );
        assert!(
            text.contains(">>> import time; time.sleep(0.07)"),
            "expected settled completion output matching submitted input to remain visible through interrupt handling, got: {text:?}"
        );
        assert!(
            text.contains(">>> "),
            "expected the settled completion to keep the prompt on the interrupt reply, got: {text:?}"
        );
        assert!(
            manager.settled_pending_completion.is_none(),
            "expected the settled completion to be consumed by the interrupt follow-up"
        );
    }
}
