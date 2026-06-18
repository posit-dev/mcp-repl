use crate::sandbox::SandboxStateUpdate;
use crate::stdin_payload::{WriteStdinControlAction, split_write_stdin_control_prefix};

use super::write_flow::WriteStdinOptions;
use super::{WorkerError, prechecked_follow_up_requires_meta_error};

pub(super) struct ControlPrefixInput<'a> {
    action: WriteStdinControlAction,
    tail: &'a str,
}

impl<'a> ControlPrefixInput<'a> {
    pub(super) fn split(input: &'a str) -> Option<Self> {
        let (action, tail) = split_write_stdin_control_prefix(input)?;
        Some(Self { action, tail })
    }

    pub(super) fn action(&self) -> WriteStdinControlAction {
        self.action
    }

    pub(super) fn plan(
        self,
        control_requires_spawn: bool,
        options: &WriteStdinOptions,
    ) -> Result<ControlPrefixPlan<'a>, WorkerError> {
        if options.pending_state_prechecked
            && control_requires_spawn
            && options.deferred_sandbox_state_update.is_none()
            && !options.suppress_session_end_reset
        {
            return Err(prechecked_follow_up_requires_meta_error());
        }

        let stage_before_control =
            control_requires_spawn || matches!(self.action, WriteStdinControlAction::Restart);
        let stage_interrupt_after_session_end =
            matches!(self.action, WriteStdinControlAction::Interrupt) && !stage_before_control;
        let staged_sandbox_state_update = if stage_before_control {
            options.deferred_sandbox_state_update.clone()
        } else {
            None
        };
        let tail_sandbox_state_update = if stage_before_control {
            None
        } else {
            options.deferred_sandbox_state_update.clone()
        };

        Ok(ControlPrefixPlan {
            action: self.action,
            tail: self.tail,
            stage_before_control,
            stage_interrupt_after_session_end,
            staged_sandbox_state_update,
            tail_sandbox_state_update,
        })
    }
}

pub(super) struct ControlPrefixPlan<'a> {
    pub(super) action: WriteStdinControlAction,
    pub(super) tail: &'a str,
    pub(super) stage_before_control: bool,
    pub(super) stage_interrupt_after_session_end: bool,
    pub(super) staged_sandbox_state_update: Option<SandboxStateUpdate>,
    pub(super) tail_sandbox_state_update: Option<SandboxStateUpdate>,
}
