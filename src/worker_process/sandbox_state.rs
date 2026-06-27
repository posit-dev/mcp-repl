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
    #[cfg(target_os = "windows")]
    if matches!(
        state.sandbox_policy,
        crate::sandbox::SandboxPolicy::WorkspaceWrite {
            network_access: false,
            ..
        }
    ) {
        return Ok(Some(ManagedProxyConfig {
            allowed_domains: Vec::new(),
            denied_domains: Vec::new(),
            allow_local_binding: state.managed_network_policy.allow_local_binding,
        }));
    }
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
    if !(cfg!(target_os = "macos") || cfg!(target_os = "windows")) {
        return Err(WorkerError::Sandbox(
            "managed network domain restrictions are currently supported only on macOS and Windows"
                .to_string(),
        ));
    }
    ManagedProxyConfig::from_policy(&state.managed_network_policy)
        .map(Some)
        .map_err(|err| WorkerError::Sandbox(err.to_string()))
}

impl WorkerManager {
    #[cfg(debug_assertions)]
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
        #[cfg(target_os = "linux")]
        let mut update = update;
        #[cfg(target_os = "linux")]
        self.apply_linux_bwrap_default_override(&mut update);
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

    #[cfg(target_os = "linux")]
    fn apply_linux_bwrap_default_override(&self, update: &mut SandboxStateUpdate) {
        if !self.sandbox_defaults.use_linux_sandbox_bwrap {
            update.use_linux_sandbox_bwrap = update.use_linux_sandbox_bwrap.map(|_| false);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Backend;
    use crate::oversized_output::OversizedOutputMode;
    use crate::sandbox::SandboxPolicy;
    use crate::sandbox_cli::{
        SandboxCliOperation, SandboxConfigOperation, SandboxModeArg,
        resolve_effective_sandbox_state_with_defaults,
    };
    use crate::worker_process::test_support::cwd_test_mutex;
    use std::time::Duration;

    #[test]
    fn inherit_ending_invalid_plan_fails_during_startup_validation() {
        let plan = SandboxCliPlan {
            operations: vec![
                SandboxCliOperation::SetMode(SandboxModeArg::ReadOnly),
                SandboxCliOperation::AddWritableRoot(std::env::temp_dir()),
                SandboxCliOperation::SetMode(SandboxModeArg::Inherit),
            ],
        };

        let err = match WorkerManager::new(Backend::Python, plan, OversizedOutputMode::Files) {
            Ok(_) => panic!("invalid inherit-ending plan should fail during startup"),
            Err(err) => err,
        };

        assert!(
            matches!(err, WorkerError::Sandbox(ref message) if message.contains("--add-writable-root can only be used while sandbox mode is workspace-write")),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn inherit_workspace_write_refinements_wait_for_client_state() {
        let writable_root = std::env::temp_dir();
        let plan = SandboxCliPlan {
            operations: vec![
                SandboxCliOperation::SetMode(SandboxModeArg::Inherit),
                SandboxCliOperation::AddWritableRoot(writable_root.clone()),
                SandboxCliOperation::Config(SandboxConfigOperation::SetWorkspaceNetworkAccess(
                    true,
                )),
            ],
        };
        let mut manager = WorkerManager::new(Backend::Python, plan, OversizedOutputMode::Files)
            .expect("worker manager");

        manager
            .stage_sandbox_state_update(SandboxStateUpdate {
                sandbox_policy: SandboxPolicy::WorkspaceWrite {
                    writable_roots: Vec::new(),
                    network_access: false,
                    exclude_tmpdir_env_var: false,
                    exclude_slash_tmp: false,
                },
                sandbox_cwd: Some(writable_root.clone()),
                use_linux_sandbox_bwrap: None,
                use_legacy_landlock: None,
            })
            .expect("workspace-write Codex metadata should satisfy deferred refinements");

        let SandboxPolicy::WorkspaceWrite {
            writable_roots,
            network_access,
            ..
        } = &manager.sandbox_state.sandbox_policy
        else {
            panic!(
                "expected staged inherit refinements to resolve to workspace-write, got {:?}",
                manager.sandbox_state.sandbox_policy
            );
        };
        assert!(
            *network_access,
            "expected deferred workspace network setting to apply after client metadata"
        );
        assert!(
            writable_roots.iter().any(|path| path == &writable_root),
            "expected deferred writable root to apply after client metadata"
        );
    }

    #[test]
    fn failed_sandbox_update_does_not_commit_inherited_state() {
        let _guard = cwd_test_mutex().lock().expect("cwd mutex");
        let plan = SandboxCliPlan {
            operations: vec![
                SandboxCliOperation::SetMode(SandboxModeArg::Inherit),
                SandboxCliOperation::Config(SandboxConfigOperation::SetWorkspaceNetworkAccess(
                    true,
                )),
            ],
        };
        let mut manager = WorkerManager::new(Backend::Python, plan, OversizedOutputMode::Files)
            .expect("worker manager");
        let mut inherited_before = manager.sandbox_defaults.clone();
        inherited_before.apply_update(SandboxStateUpdate {
            sandbox_policy: SandboxPolicy::WorkspaceWrite {
                writable_roots: Vec::new(),
                network_access: false,
                exclude_tmpdir_env_var: false,
                exclude_slash_tmp: false,
            },
            sandbox_cwd: None,
            use_linux_sandbox_bwrap: None,
            use_legacy_landlock: None,
        });
        manager.inherited_sandbox_state = Some(inherited_before.clone());
        manager.sandbox_state = resolve_effective_sandbox_state_with_defaults(
            &manager.sandbox_plan,
            Some(&inherited_before),
            &manager.sandbox_defaults,
        )
        .expect("resolved initial inherited sandbox state");

        let err = manager
            .update_sandbox_state(
                SandboxStateUpdate {
                    sandbox_policy: SandboxPolicy::DangerFullAccess,
                    sandbox_cwd: None,
                    use_linux_sandbox_bwrap: None,
                    use_legacy_landlock: None,
                },
                Duration::from_millis(1),
            )
            .expect_err("danger-full-access should fail workspace-write-only config");
        assert!(
            matches!(err, WorkerError::Sandbox(ref msg) if msg.contains("requires workspace-write mode")),
            "unexpected error: {err}"
        );
        assert_eq!(
            manager.inherited_sandbox_state,
            Some(inherited_before),
            "failed updates must not mutate inherited sandbox baseline"
        );
    }
}
