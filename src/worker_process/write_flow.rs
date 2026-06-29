use std::time::{Duration, Instant};

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
        let started_at = Instant::now();
        let worker_deadline = started_at + worker_timeout;
        let server_deadline = started_at + server_timeout;
        let has_tail = !plan.tail.is_empty();

        let control_reply = match (mode, plan.action, plan.stage_interrupt_after_session_end) {
            (WriteStdinMode::Files, WriteStdinControlAction::Interrupt, true) => {
                self.interrupt_files(remaining_until(worker_deadline), None, true)
            }
            (WriteStdinMode::Files, WriteStdinControlAction::Interrupt, false) => self
                .interrupt_files(
                    remaining_until(worker_deadline),
                    plan.tail_sandbox_state_update.clone(),
                    options.suppress_session_end_reset,
                ),
            (WriteStdinMode::Files, WriteStdinControlAction::Restart, _) => {
                self.restart_files(remaining_until(worker_deadline))
            }
            (WriteStdinMode::Pager, WriteStdinControlAction::Interrupt, true) => {
                self.interrupt_pager(remaining_until(worker_deadline), None, true)
            }
            (WriteStdinMode::Pager, WriteStdinControlAction::Interrupt, false) => self
                .interrupt_pager(
                    remaining_until(worker_deadline),
                    plan.tail_sandbox_state_update.clone(),
                    options.suppress_session_end_reset,
                ),
            (WriteStdinMode::Pager, WriteStdinControlAction::Restart, _) => {
                self.restart_pager(remaining_until(worker_deadline))
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
        if !has_tail {
            return Ok(Some(control_reply));
        }

        let control_prefix_item_count = prefixed_worker_reply_item_count(&control_reply);
        let tail_options = options.control_tail(plan.tail_sandbox_state_update);
        let tail_worker_timeout = remaining_until(worker_deadline);
        let tail_server_timeout = remaining_until(server_deadline);
        let remaining_reply = match mode {
            WriteStdinMode::Files => self.write_stdin_files(
                plan.tail.to_string(),
                tail_worker_timeout,
                tail_server_timeout,
                tail_options,
            ),
            WriteStdinMode::Pager => self.write_stdin_pager(
                plan.tail.to_string(),
                tail_worker_timeout,
                tail_server_timeout,
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
        let input_may_create_user_state = self.input_may_create_user_state(&text);
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
        if result.is_ok() && input_may_create_user_state {
            self.user_state_may_exist = true;
        }
        result
    }

    fn input_may_create_user_state(&self, text: &str) -> bool {
        if text.is_empty() || self.local_pager_follow_up_uses_existing_state(text) {
            return false;
        }
        match split_write_stdin_control_prefix(text) {
            Some((_control, remaining)) => !remaining.trim().is_empty(),
            None => true,
        }
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

fn remaining_until(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Backend;
    use crate::output_snapshot::{SnapshotWithImages, snapshot_page_with_images};
    use crate::sandbox::{SandboxPolicy, SandboxStateUpdate};
    use crate::sandbox_cli::SandboxCliPlan;
    use crate::worker_process::is_prechecked_follow_up_requires_meta;
    #[cfg(target_family = "unix")]
    use crate::worker_process::test_support::{
        contents_text, cwd_test_mutex, worker_process_test_temp_parent,
    };
    use crate::worker_process::test_support::{successful_test_child, test_worker_process};
    use crate::worker_protocol::ContentOrigin;
    #[cfg(target_family = "unix")]
    use std::path::PathBuf;

    #[test]
    fn exact_interrupt_remains_local_when_worker_would_respawn() {
        let plan = SandboxCliPlan {
            operations: vec![crate::sandbox_cli::SandboxCliOperation::SetMode(
                crate::sandbox_cli::SandboxModeArg::Inherit,
            )],
        };
        let mut manager = WorkerManager::new(Backend::Python, plan, OversizedOutputMode::Files)
            .expect("worker manager");

        assert!(
            manager
                .nonexecuting_follow_up_uses_existing_state("\u{3}")
                .expect("interrupt follow-up classification"),
            "a bare Ctrl-C should stay a local follow-up even when it would otherwise respawn"
        );
    }

    #[test]
    fn interrupt_pager_tail_requires_current_sandbox_when_worker_would_respawn() {
        let plan = SandboxCliPlan {
            operations: vec![crate::sandbox_cli::SandboxCliOperation::SetMode(
                crate::sandbox_cli::SandboxModeArg::Inherit,
            )],
        };
        let mut manager = WorkerManager::new(Backend::R, plan, OversizedOutputMode::Pager)
            .expect("worker manager");
        let mut process = test_worker_process(successful_test_child());
        process
            .wait_child_for_test()
            .expect("wait for the stub worker process to exit");
        manager.process = Some(process);

        manager.output.start_capture();
        manager.output_timeline.append_text(
            b"line0001\nline0002\nline0003\nline0004\n",
            false,
            ContentOrigin::Worker,
        );
        let end_offset = manager.output.end_offset().expect("output end offset");
        let SnapshotWithImages { buffer, .. } =
            snapshot_page_with_images(&manager.output, end_offset, 16);
        manager.pager.activate(buffer.expect("pager buffer"), false);

        assert!(
            !manager
                .nonexecuting_follow_up_uses_existing_state("\u{3}:q")
                .expect("interrupt follow-up classification"),
            "a pager ctrl-c tail should require current per-call sandbox metadata when it would respawn"
        );
    }

    #[test]
    fn empty_input_with_busy_guardrail_uses_existing_state() {
        let plan = SandboxCliPlan {
            operations: vec![crate::sandbox_cli::SandboxCliOperation::SetMode(
                crate::sandbox_cli::SandboxModeArg::Inherit,
            )],
        };
        let mut manager = WorkerManager::new(Backend::R, plan, OversizedOutputMode::Files)
            .expect("worker manager");
        {
            let mut slot = manager
                .guardrail
                .event
                .lock()
                .expect("guardrail event mutex poisoned");
            *slot = Some(crate::worker_supervisor::GuardrailEvent {
                message: "[repl] previous request aborted; retry your last input\n".to_string(),
                was_busy: true,
                is_error: true,
            });
        }

        assert!(
            !manager
                .empty_input_requires_spawn()
                .expect("empty-input classification"),
            "empty polls should keep pending busy-guardrail recovery local"
        );
    }

    #[test]
    fn nonempty_input_with_busy_guardrail_requires_current_state() {
        let plan = SandboxCliPlan {
            operations: vec![crate::sandbox_cli::SandboxCliOperation::SetMode(
                crate::sandbox_cli::SandboxModeArg::Inherit,
            )],
        };
        let mut manager = WorkerManager::new(Backend::R, plan, OversizedOutputMode::Files)
            .expect("worker manager");
        {
            let mut slot = manager
                .guardrail
                .event
                .lock()
                .expect("guardrail event mutex poisoned");
            *slot = Some(crate::worker_supervisor::GuardrailEvent {
                message: "[repl] previous request aborted; retry your last input\n".to_string(),
                was_busy: true,
                is_error: true,
            });
        }

        assert!(
            !manager
                .nonexecuting_follow_up_uses_existing_state("1+1")
                .expect("follow-up classification"),
            "busy-guardrail retries should require current per-call sandbox metadata"
        );
    }

    #[test]
    fn empty_input_with_idle_guardrail_requires_spawn() {
        let plan = SandboxCliPlan {
            operations: vec![crate::sandbox_cli::SandboxCliOperation::SetMode(
                crate::sandbox_cli::SandboxModeArg::Inherit,
            )],
        };
        let mut manager = WorkerManager::new(Backend::R, plan, OversizedOutputMode::Files)
            .expect("worker manager");
        {
            let mut slot = manager
                .guardrail
                .event
                .lock()
                .expect("guardrail event mutex poisoned");
            *slot = Some(crate::worker_supervisor::GuardrailEvent {
                message: "[repl] worker was idle; new session started\n".to_string(),
                was_busy: false,
                is_error: false,
            });
        }

        assert!(
            manager
                .empty_input_requires_spawn()
                .expect("empty-input classification"),
            "idle guardrail notices should still require current per-call sandbox metadata when a poll would respawn"
        );
    }

    #[test]
    fn prechecked_empty_input_requires_current_sandbox_when_worker_exited() {
        let plan = SandboxCliPlan {
            operations: vec![crate::sandbox_cli::SandboxCliOperation::SetMode(
                crate::sandbox_cli::SandboxModeArg::Inherit,
            )],
        };
        let mut manager = WorkerManager::new(Backend::R, plan, OversizedOutputMode::Files)
            .expect("worker manager");
        manager
            .stage_sandbox_state_update(SandboxStateUpdate {
                sandbox_policy: SandboxPolicy::ReadOnly {
                    network_access: false,
                },
                sandbox_cwd: None,
                use_linux_sandbox_bwrap: None,
                use_legacy_landlock: None,
            })
            .expect("initial inherited state");
        let mut process = test_worker_process(successful_test_child());
        process
            .wait_child_for_test()
            .expect("wait for the stub worker process to exit");
        manager.process = Some(process);

        let result = manager.write_stdin(
            String::new(),
            Duration::from_secs(1),
            Duration::from_secs(1),
            WriteStdinOptions {
                pending_state_prechecked: true,
                ..WriteStdinOptions::default()
            },
        );

        assert!(
            matches!(result, Err(ref err) if is_prechecked_follow_up_requires_meta(err)),
            "expected prechecked empty input to require current sandbox metadata once the worker has exited, got: {result:?}"
        );
    }

    #[test]
    fn prechecked_bare_interrupt_requires_current_sandbox_when_worker_exited() {
        let plan = SandboxCliPlan {
            operations: vec![crate::sandbox_cli::SandboxCliOperation::SetMode(
                crate::sandbox_cli::SandboxModeArg::Inherit,
            )],
        };
        let mut manager = WorkerManager::new(Backend::R, plan, OversizedOutputMode::Files)
            .expect("worker manager");
        manager
            .stage_sandbox_state_update(SandboxStateUpdate {
                sandbox_policy: SandboxPolicy::ReadOnly {
                    network_access: false,
                },
                sandbox_cwd: None,
                use_linux_sandbox_bwrap: None,
                use_legacy_landlock: None,
            })
            .expect("initial inherited state");
        let mut process = test_worker_process(successful_test_child());
        process
            .wait_child_for_test()
            .expect("wait for the stub worker process to exit");
        manager.process = Some(process);

        let result = manager.write_stdin(
            "\u{3}".to_string(),
            Duration::from_secs(1),
            Duration::from_secs(1),
            WriteStdinOptions {
                pending_state_prechecked: true,
                ..WriteStdinOptions::default()
            },
        );

        assert!(
            matches!(result, Err(ref err) if is_prechecked_follow_up_requires_meta(err)),
            "expected prechecked bare ctrl-c to require current sandbox metadata once the worker has exited, got: {result:?}"
        );
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn interrupt_tail_uses_current_sandbox_for_the_respawn() {
        let _guard = cwd_test_mutex().lock().expect("cwd mutex");
        let temp = tempfile::Builder::new()
            .prefix(".tmp-interrupt-tail-current-sandbox-")
            .tempdir_in(worker_process_test_temp_parent("worker-process"))
            .expect("tempdir");
        let sandbox_cwd = temp.path().to_path_buf();
        let plan = SandboxCliPlan {
            operations: vec![crate::sandbox_cli::SandboxCliOperation::SetMode(
                crate::sandbox_cli::SandboxModeArg::Inherit,
            )],
        };
        let mut manager = WorkerManager::new(Backend::R, plan, OversizedOutputMode::Files)
            .expect("worker manager");
        manager
            .stage_sandbox_state_update(SandboxStateUpdate {
                sandbox_policy: SandboxPolicy::ReadOnly {
                    network_access: false,
                },
                sandbox_cwd: Some(sandbox_cwd.clone()),
                use_linux_sandbox_bwrap: None,
                use_legacy_landlock: None,
            })
            .expect("initial inherited read-only state");
        let mut process = test_worker_process(successful_test_child());
        process
            .wait_child_for_test()
            .expect("wait for the stub worker process to exit");
        manager.process = Some(process);
        manager.exe_path = PathBuf::from("definitely-missing-worker-exe");

        let result = manager.write_stdin(
            "\u{3}1+1".to_string(),
            Duration::from_secs(10),
            Duration::from_secs(10),
            WriteStdinOptions {
                deferred_sandbox_state_update: Some(SandboxStateUpdate {
                    sandbox_policy: SandboxPolicy::WorkspaceWrite {
                        writable_roots: Vec::new(),
                        network_access: false,
                        exclude_tmpdir_env_var: false,
                        exclude_slash_tmp: false,
                    },
                    sandbox_cwd: Some(sandbox_cwd.clone()),
                    use_linux_sandbox_bwrap: None,
                    use_legacy_landlock: None,
                }),
                ..WriteStdinOptions::default()
            },
        );
        match result {
            Ok(WorkerReply::Output {
                contents, is_error, ..
            }) => {
                let text = contents_text(&contents);
                assert!(
                    is_error,
                    "expected the failed interrupt-tail respawn attempt to surface as an error reply"
                );
                assert!(
                    text.contains("worker error:"),
                    "expected the failed interrupt-tail respawn attempt to report a worker error, got: {text:?}"
                );
            }
            Err(WorkerError::Protocol(message)) => {
                assert!(
                    message.contains("backend info") || message.contains("ipc disconnected"),
                    "expected the failed interrupt-tail respawn attempt to fail during worker startup, got: {message:?}"
                );
            }
            Err(err) => panic!("unexpected interrupt-tail respawn error: {err}"),
        }
        assert!(
            matches!(
                manager.sandbox_state.sandbox_policy,
                SandboxPolicy::WorkspaceWrite { .. }
            ),
            "expected deferred metadata to stage before interrupt attempts the respawn"
        );
        assert_eq!(
            manager.sandbox_state.sandbox_cwd, sandbox_cwd,
            "expected deferred metadata to update the effective sandbox cwd before the respawn path"
        );
    }
}
