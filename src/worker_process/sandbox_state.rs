use std::time::Duration;

use crate::managed_network::ManagedProxyConfig;
use crate::sandbox::{SandboxState, SandboxStateUpdate};
use crate::sandbox_cli::{
    MISSING_INHERITED_SANDBOX_STATE_MESSAGE, SandboxCliPlan,
    resolve_effective_sandbox_state_with_defaults, sandbox_plan_requests_inherited_state,
    validate_sandbox_plan_with_defaults,
};
use crate::worker_supervisor::GuardrailEvent;

use super::{WorkerError, WorkerManager};

pub(super) struct InitialSandboxState {
    pub(super) state: SandboxState,
    pub(super) awaiting_inherited_state: bool,
}

struct PreparedSandboxStateUpdate {
    update_for_log: serde_json::Value,
    requires_restart: bool,
    missing_before: bool,
}

pub(super) fn prepare_initial_sandbox_state(
    plan: &SandboxCliPlan,
    defaults: &SandboxState,
) -> Result<InitialSandboxState, WorkerError> {
    let awaiting_inherited_state = sandbox_plan_requests_inherited_state(plan);
    let state = if awaiting_inherited_state {
        validate_sandbox_plan_with_defaults(plan, defaults).map_err(WorkerError::Sandbox)?;
        defaults.clone()
    } else {
        resolve_effective_sandbox_state_with_defaults(plan, None, defaults)
            .map_err(WorkerError::Sandbox)?
    };

    Ok(InitialSandboxState {
        state,
        awaiting_inherited_state,
    })
}

pub(super) fn managed_network_proxy_config_for_state(
    state: &SandboxState,
) -> Result<Option<ManagedProxyConfig>, WorkerError> {
    if !state.managed_network_policy.has_domain_restrictions() {
        return Ok(None);
    }
    if !state.sandbox_policy.has_full_network_access() {
        return Ok(None);
    }
    if !state.sandbox_policy.requires_sandbox() {
        return Err(WorkerError::Sandbox(
            "managed network domain restrictions require built-in sandbox enforcement".to_string(),
        ));
    }
    if !cfg!(target_os = "macos") {
        return Err(WorkerError::Sandbox(
            "managed network domain restrictions are currently supported only on macOS".to_string(),
        ));
    }
    ManagedProxyConfig::from_policy(&state.managed_network_policy)
        .map(Some)
        .map_err(|err| WorkerError::Sandbox(err.to_string()))
}

impl WorkerManager {
    pub fn bootstrap_local_inherited_sandbox_state(&mut self) -> Result<bool, WorkerError> {
        if !self.missing_inherited_sandbox_state() {
            return Ok(false);
        }

        let update = SandboxStateUpdate {
            sandbox_policy: self.sandbox_defaults.sandbox_policy.clone(),
            sandbox_cwd: Some(self.sandbox_defaults.sandbox_cwd.clone()),
            use_linux_sandbox_bwrap: Some(self.sandbox_defaults.use_linux_sandbox_bwrap),
            use_legacy_landlock: None,
        };
        crate::event_log::log(
            "worker_local_inherit_bootstrap",
            serde_json::json!({
                "sandbox_policy": update.sandbox_policy.clone(),
                "sandbox_cwd": update.sandbox_cwd.clone(),
                "use_linux_sandbox_bwrap": update.use_linux_sandbox_bwrap,
            }),
        );
        self.stage_sandbox_state_update(update)?;
        Ok(true)
    }

    pub(super) fn requests_inherited_sandbox_state(&self) -> bool {
        sandbox_plan_requests_inherited_state(&self.sandbox_plan)
    }

    pub(super) fn missing_inherited_sandbox_state(&self) -> bool {
        self.requests_inherited_sandbox_state() && self.inherited_sandbox_state.is_none()
    }

    pub(super) fn require_inherited_sandbox_state(&self) -> Result<(), WorkerError> {
        if self.missing_inherited_sandbox_state() {
            return Err(WorkerError::Sandbox(
                MISSING_INHERITED_SANDBOX_STATE_MESSAGE.to_string(),
            ));
        }
        Ok(())
    }

    // Replaces the inherited sandbox snapshot for this tool call. The new
    // policy becomes effective in the worker only after the current process is
    // recycled and a replacement worker is spawned. Session temp handling is
    // separate: the server-owned temp dir is reset before each spawn, and
    // today that reset reuses the same configured path in place.
    fn prepare_sandbox_state_update(
        &mut self,
        update: SandboxStateUpdate,
    ) -> Result<PreparedSandboxStateUpdate, WorkerError> {
        let update_for_log = serde_json::to_value(&update)
            .unwrap_or_else(|err| serde_json::json!({"serialize_error": err.to_string()}));
        crate::sandbox::log_sandbox_policy_update(&update.sandbox_policy);
        let mut inherited_state = self.sandbox_defaults.clone();
        inherited_state.apply_update(update);
        #[cfg(target_os = "linux")]
        self.apply_linux_bwrap_fallback_override(&mut inherited_state);
        let resolved_state = resolve_effective_sandbox_state_with_defaults(
            &self.sandbox_plan,
            Some(&inherited_state),
            &self.sandbox_defaults,
        )
        .map_err(WorkerError::Sandbox)?;
        #[cfg(target_os = "linux")]
        let resolved_state = {
            let mut resolved_state = resolved_state;
            self.apply_linux_bwrap_fallback_override(&mut resolved_state);
            resolved_state
        };
        let missing_before = self.missing_inherited_sandbox_state();
        self.inherited_sandbox_state = Some(inherited_state);
        let requires_restart = self.sandbox_state != resolved_state;
        self.sandbox_state = resolved_state;
        #[cfg(target_os = "windows")]
        if requires_restart {
            // Prepared Windows launch state is keyed to the effective worker
            // sandbox configuration. Drop it before respawn so the next worker
            // picks up the updated sandbox state.
            self.windows_sandbox_launch = None;
        }
        Ok(PreparedSandboxStateUpdate {
            update_for_log,
            requires_restart,
            missing_before,
        })
    }

    #[cfg(target_os = "linux")]
    fn apply_linux_bwrap_fallback_override(&self, state: &mut SandboxState) {
        if self.linux_bwrap_fallback_disabled {
            state.use_linux_sandbox_bwrap = false;
        }
    }

    fn log_sandbox_state_update(
        prepared: &PreparedSandboxStateUpdate,
        timeout: Option<Duration>,
        respawned: bool,
    ) {
        crate::event_log::log(
            "worker_sandbox_state_update",
            serde_json::json!({
                "changed": prepared.requires_restart,
                "timeout_ms": timeout.map(|timeout| timeout.as_millis()),
                "respawned": respawned,
                "update": prepared.update_for_log,
            }),
        );
    }

    pub fn stage_sandbox_state_update(
        &mut self,
        update: SandboxStateUpdate,
    ) -> Result<(), WorkerError> {
        let prepared = self.prepare_sandbox_state_update(update)?;
        Self::log_sandbox_state_update(&prepared, None, false);
        Ok(())
    }

    pub fn update_sandbox_state(
        &mut self,
        update: SandboxStateUpdate,
        timeout: Duration,
    ) -> Result<bool, WorkerError> {
        let prepared = self.prepare_sandbox_state_update(update)?;
        let mut respawned = false;
        if !prepared.requires_restart {
            if prepared.missing_before && self.process.is_none() {
                self.spawn_worker_after_initial_sandbox_state()?;
                respawned = true;
            }
            Self::log_sandbox_state_update(&prepared, Some(timeout), respawned);
            return Ok(respawned);
        }

        let aborted_request = self.pending_request;
        let had_prior_session = self.last_spawn.is_some();
        self.restart_worker_after_sandbox_state_change(timeout)?;
        if had_prior_session {
            self.stage_sandbox_change_restart_notice(aborted_request);
            self.next_live_prefix_belongs_to_reply = true;
        }
        respawned = true;
        Self::log_sandbox_state_update(&prepared, Some(timeout), respawned);
        Ok(respawned)
    }

    pub(super) fn stage_sandbox_change_restart_notice(&mut self, aborted_request: bool) {
        let policy = serde_json::to_string(&self.sandbox_state.sandbox_policy)
            .unwrap_or_else(|err| format!("{{\"serialize_error\":\"{}\"}}", err));
        let mut message = String::from("[repl] sandbox policy changed; new session started\n");
        if aborted_request {
            message.push_str("[repl] previous request aborted because sandbox policy changed\n");
        }
        message.push_str(&format!("[repl] new sandbox policy: {policy}\n"));
        let event = GuardrailEvent {
            message,
            was_busy: false,
            is_error: false,
        };
        match &mut self.pending_server_notice {
            Some(pending) => pending.message.push_str(&event.message),
            None => self.pending_server_notice = Some(event),
        }
    }
}
