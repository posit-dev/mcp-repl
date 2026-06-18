use std::time::Duration;

use crate::oversized_output::OversizedOutputMode;
use crate::pager;
use crate::reply_presentation::strip_trailing_prompt;
use crate::sandbox::SandboxStateUpdate;
use crate::worker_protocol::{WorkerContent, WorkerReply};

use super::control_prefix::ControlPrefixInput;
use super::write_dispatch::WriteDispatchInput;
use super::write_preflight::{WritePreflightInput, WritePreflightOutcome};
use super::{
    WorkerError, WorkerManager, WriteStdinControlAction, prechecked_follow_up_requires_meta_error,
    split_write_stdin_control_prefix,
};

const DEFERRED_SANDBOX_UPDATE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Default)]
pub(crate) struct WriteStdinOptions {
    pub page_bytes_override: Option<u64>,
    pub echo_input: bool,
    pub pending_state_prechecked: bool,
    pub deferred_sandbox_state_update: Option<SandboxStateUpdate>,
    pub suppress_session_end_reset: bool,
}

impl WriteStdinOptions {
    pub(super) fn control_tail(
        &self,
        deferred_sandbox_state_update: Option<SandboxStateUpdate>,
    ) -> Self {
        Self {
            page_bytes_override: self.page_bytes_override,
            echo_input: self.echo_input,
            pending_state_prechecked: false,
            deferred_sandbox_state_update,
            suppress_session_end_reset: false,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum WriteStdinMode {
    Files,
    Pager,
}

impl WorkerManager {
    pub fn empty_input_requires_spawn(&mut self) -> Result<bool, WorkerError> {
        if self.empty_input_uses_existing_state() {
            return Ok(false);
        }
        let needs_spawn = match self.process.as_mut() {
            Some(process) => !process.is_running()?,
            None => true,
        };
        Ok(needs_spawn)
    }

    pub fn empty_input_polls_existing_output(&self) -> bool {
        match self.oversized_output {
            OversizedOutputMode::Files => {
                self.pending_request
                    || self.pending_output_tape.has_pending()
                    || self.settled_pending_completion.is_some()
            }
            OversizedOutputMode::Pager => {
                self.pending_request
                    || self.output.has_pending_output()
                    || self.settled_pending_completion.is_some()
            }
        }
    }

    pub fn empty_input_uses_local_pager_state(&self) -> bool {
        matches!(self.oversized_output, OversizedOutputMode::Pager)
            && self.pager.is_active()
            && !self.empty_input_polls_existing_output()
    }

    pub fn empty_input_may_auto_reset_after_poll(&self) -> bool {
        self.empty_input_polls_existing_output()
            && (self.pending_request
                || self.settled_pending_completion.is_some()
                || self.session_end_seen)
    }

    pub fn nonexecuting_follow_up_uses_existing_state(
        &mut self,
        text: &str,
    ) -> Result<bool, WorkerError> {
        if let Some((control, remaining)) = split_write_stdin_control_prefix(text) {
            return match control {
                WriteStdinControlAction::Interrupt => {
                    if remaining.is_empty() {
                        Ok(true)
                    } else {
                        Ok(self.local_pager_follow_up_uses_existing_state(remaining)
                            && !self.control_only_interrupt_requires_spawn()?)
                    }
                }
                WriteStdinControlAction::Restart => Ok(false),
            };
        }

        Ok(self.local_pager_follow_up_uses_existing_state(text))
    }

    pub(super) fn control_only_interrupt_requires_spawn(&mut self) -> Result<bool, WorkerError> {
        match self.process.as_mut() {
            Some(process) => Ok(!process.is_running()?),
            None => Ok(true),
        }
    }

    fn write_stdin_control_prefix(
        &mut self,
        mode: WriteStdinMode,
        text: &str,
        worker_timeout: Duration,
        server_timeout: Duration,
        options: &WriteStdinOptions,
    ) -> Result<Option<WorkerReply>, WorkerError> {
        let Some(prefix) = ControlPrefixInput::split(text) else {
            return Ok(None);
        };

        self.clear_guardrail_busy_event();
        let control_requires_spawn = match prefix.action() {
            WriteStdinControlAction::Interrupt => self.control_only_interrupt_requires_spawn()?,
            WriteStdinControlAction::Restart => false,
        };
        let mut plan = prefix.plan(control_requires_spawn, options)?;
        if plan.stage_before_control {
            self.stage_deferred_sandbox_state_update(plan.staged_sandbox_state_update.take())?;
        }

        let control_reply = match (mode, plan.action, plan.stage_interrupt_after_session_end) {
            (WriteStdinMode::Files, WriteStdinControlAction::Interrupt, true) => {
                self.interrupt_files(worker_timeout, None, true)
            }
            (WriteStdinMode::Files, WriteStdinControlAction::Interrupt, false) => self
                .interrupt_files(
                    worker_timeout,
                    plan.tail_sandbox_state_update.clone(),
                    options.suppress_session_end_reset,
                ),
            (WriteStdinMode::Files, WriteStdinControlAction::Restart, _) => {
                self.restart_files(worker_timeout)
            }
            (WriteStdinMode::Pager, WriteStdinControlAction::Interrupt, true) => {
                self.interrupt_pager(worker_timeout, None, true)
            }
            (WriteStdinMode::Pager, WriteStdinControlAction::Interrupt, false) => self
                .interrupt_pager(
                    worker_timeout,
                    plan.tail_sandbox_state_update.clone(),
                    options.suppress_session_end_reset,
                ),
            (WriteStdinMode::Pager, WriteStdinControlAction::Restart, _) => {
                self.restart_pager(worker_timeout)
            }
        }?;

        if plan.stage_interrupt_after_session_end
            && self.session_end_seen
            && !options.suppress_session_end_reset
        {
            self.stage_session_end_sandbox_state_update(
                plan.tail_sandbox_state_update.take(),
                options.pending_state_prechecked,
            )?;
            self.maybe_reset_after_session_end();
        }
        if plan.tail.is_empty() {
            return Ok(Some(control_reply));
        }

        let control_prefix_item_count = prefixed_worker_reply_item_count(&control_reply);
        let tail_options = options.control_tail(plan.tail_sandbox_state_update);
        let remaining_reply = match mode {
            WriteStdinMode::Files => self.write_stdin_files(
                plan.tail.to_string(),
                worker_timeout,
                server_timeout,
                tail_options,
            ),
            WriteStdinMode::Pager => self.write_stdin_pager(
                plan.tail.to_string(),
                worker_timeout,
                server_timeout,
                tail_options,
            ),
        }?;
        self.last_detached_prefix_item_count += control_prefix_item_count;
        Ok(Some(prefix_worker_reply(control_reply, remaining_reply)))
    }

    pub(super) fn stage_deferred_sandbox_state_update(
        &mut self,
        update: Option<SandboxStateUpdate>,
    ) -> Result<(), WorkerError> {
        let Some(update) = update else {
            return Ok(());
        };
        self.stage_sandbox_state_update(update)
    }

    pub(super) fn stage_session_end_sandbox_state_update(
        &mut self,
        update: Option<SandboxStateUpdate>,
        pending_state_prechecked: bool,
    ) -> Result<(), WorkerError> {
        if pending_state_prechecked && update.is_none() && self.requests_inherited_sandbox_state() {
            return Err(prechecked_follow_up_requires_meta_error());
        }

        self.stage_deferred_sandbox_state_update(update)
    }

    pub(super) fn apply_deferred_sandbox_state_update(
        &mut self,
        update: Option<SandboxStateUpdate>,
    ) -> Result<(), WorkerError> {
        let Some(update) = update else {
            return Ok(());
        };
        self.update_sandbox_state(update, DEFERRED_SANDBOX_UPDATE_TIMEOUT)?;
        Ok(())
    }

    fn empty_input_uses_existing_state(&self) -> bool {
        match self.oversized_output {
            OversizedOutputMode::Files => {
                self.pending_request
                    || self.pending_output_tape.has_pending()
                    || self.settled_pending_completion.is_some()
                    || self.guardrail_busy_event_pending()
            }
            OversizedOutputMode::Pager => {
                self.pending_request
                    || self.output.has_pending_output()
                    || self.settled_pending_completion.is_some()
                    || self.pager.is_active()
                    || self.guardrail_busy_event_pending()
            }
        }
    }

    pub(crate) fn local_pager_follow_up_uses_existing_state(&self, text: &str) -> bool {
        matches!(self.oversized_output, OversizedOutputMode::Pager) && self.pager.is_active() && {
            let trimmed = text.trim();
            trimmed.is_empty() || trimmed.starts_with(':')
        }
    }

    pub fn write_stdin(
        &mut self,
        text: String,
        worker_timeout: Duration,
        server_timeout: Duration,
        options: WriteStdinOptions,
    ) -> Result<WorkerReply, WorkerError> {
        self.write_in_progress = true;
        self.last_write_respawned = false;
        let result = match self.oversized_output {
            OversizedOutputMode::Files => {
                self.write_stdin_files(text, worker_timeout, server_timeout, options)
            }
            OversizedOutputMode::Pager => {
                self.write_stdin_pager(text, worker_timeout, server_timeout, options)
            }
        };
        self.write_in_progress = false;
        result
    }

    /// Entry point for the public `repl` tool in default files mode.
    pub(super) fn write_stdin_files(
        &mut self,
        text: String,
        worker_timeout: Duration,
        server_timeout: Duration,
        options: WriteStdinOptions,
    ) -> Result<WorkerReply, WorkerError> {
        self.last_detached_prefix_item_count = 0;
        if let Some(reply) = self.write_stdin_control_prefix(
            WriteStdinMode::Files,
            &text,
            worker_timeout,
            server_timeout,
            &options,
        )? {
            return Ok(reply);
        }

        match self.write_preflight(WritePreflightInput {
            mode: WriteStdinMode::Files,
            text: &text,
            worker_timeout,
            page_bytes: 0,
            echo_input: false,
            options: &options,
        })? {
            WritePreflightOutcome::Continue => {}
            WritePreflightOutcome::Reply(reply) => return Ok(reply),
        }

        self.dispatch_write_request(WriteDispatchInput {
            mode: WriteStdinMode::Files,
            text,
            worker_timeout,
            server_timeout,
            deferred_sandbox_state_update: options.deferred_sandbox_state_update,
            page_bytes: 0,
            echo_input: false,
            process_prechecked: false,
        })
    }

    pub(super) fn write_stdin_pager(
        &mut self,
        text: String,
        worker_timeout: Duration,
        server_timeout: Duration,
        options: WriteStdinOptions,
    ) -> Result<WorkerReply, WorkerError> {
        let page_bytes_override = options.page_bytes_override;
        let echo_input = options.echo_input;
        self.last_detached_prefix_item_count = 0;
        if let Some(reply) = self.write_stdin_control_prefix(
            WriteStdinMode::Pager,
            &text,
            worker_timeout,
            server_timeout,
            &options,
        )? {
            return Ok(reply);
        }

        let page_bytes = pager::resolve_page_bytes(page_bytes_override);
        match self.write_preflight(WritePreflightInput {
            mode: WriteStdinMode::Pager,
            text: &text,
            worker_timeout,
            page_bytes,
            echo_input,
            options: &options,
        })? {
            WritePreflightOutcome::Continue => {}
            WritePreflightOutcome::Reply(reply) => return Ok(reply),
        }

        self.dispatch_write_request(WriteDispatchInput {
            mode: WriteStdinMode::Pager,
            text,
            worker_timeout,
            server_timeout,
            deferred_sandbox_state_update: options.deferred_sandbox_state_update,
            page_bytes,
            echo_input,
            process_prechecked: true,
        })
    }
}

fn prefix_worker_reply(prefix: WorkerReply, suffix: WorkerReply) -> WorkerReply {
    let WorkerReply::Output {
        mut contents,
        is_error,
        error_code,
        prompt,
        prompt_variants,
    } = prefix;
    let WorkerReply::Output {
        contents: suffix_contents,
        is_error: suffix_is_error,
        error_code: suffix_error_code,
        prompt: suffix_prompt,
        prompt_variants: suffix_prompt_variants,
    } = suffix;
    if let Some(prompt_text) = prompt.as_deref() {
        strip_trailing_prompt(&mut contents, prompt_text);
    }
    if let Some(WorkerContent::ContentText {
        text: prefix_text, ..
    }) = contents.last_mut()
        && let Some(WorkerContent::ContentText {
            text: suffix_text, ..
        }) = suffix_contents.first()
        && !prefix_text.is_empty()
        && !suffix_text.is_empty()
        && !prefix_text.ends_with('\n')
        && !suffix_text.starts_with('\n')
    {
        prefix_text.push('\n');
    }
    contents.extend(suffix_contents);
    WorkerReply::Output {
        contents,
        is_error: is_error || suffix_is_error,
        error_code: suffix_error_code.or(error_code),
        prompt: suffix_prompt.or(prompt),
        prompt_variants: suffix_prompt_variants.or(prompt_variants),
    }
}

fn prefixed_worker_reply_item_count(prefix: &WorkerReply) -> usize {
    let WorkerReply::Output {
        contents, prompt, ..
    } = prefix;
    let Some(prompt_text) = prompt.as_deref() else {
        return contents.len();
    };
    if prompt_text.is_empty() {
        return contents.len();
    }
    let Some(idx) = contents
        .iter()
        .rposition(|content| matches!(content, WorkerContent::ContentText { .. }))
    else {
        return contents.len();
    };
    let WorkerContent::ContentText { text, .. } = &contents[idx] else {
        return contents.len();
    };
    if matches!(text.strip_suffix(prompt_text), Some("")) {
        contents.len().saturating_sub(1)
    } else {
        contents.len()
    }
}
