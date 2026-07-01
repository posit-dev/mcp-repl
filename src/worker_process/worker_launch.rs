#[cfg(any(test, target_os = "windows"))]
use crate::backend::Backend;
#[cfg(target_os = "linux")]
use crate::oversized_output::OversizedOutputMode;
use crate::reply_presentation::normalize_prompt;
#[cfg(target_os = "linux")]
use crate::worker_protocol::ContentOrigin;
#[cfg(target_os = "linux")]
use crate::worker_supervisor::linux_sandbox_startup_retryable;
use crate::worker_supervisor::{
    InitialWorkerPrompt, SupervisorSpawn, WorkerProcess, WorkerSpawnContext, WorkerSupervisor,
};

#[cfg(target_os = "windows")]
use super::worker_context_event_payload;
use super::{WorkerError, WorkerManager, configured_python_executable_hint};

#[cfg(target_os = "linux")]
const LINUX_BWRAP_FALLBACK_NOTICE: &str =
    "[repl] Linux bubblewrap sandbox unavailable; continuing without bwrap\n";

#[cfg(any(test, target_os = "windows"))]
pub(super) fn backend_prepares_windows_sandbox_launch(backend: Backend) -> bool {
    matches!(backend, Backend::R | Backend::Python)
}

impl WorkerManager {
    pub(super) fn spawn_process_files(&mut self) -> Result<WorkerProcess, WorkerError> {
        #[cfg(target_os = "linux")]
        {
            loop {
                match self.spawn_process(false, false) {
                    Ok(process) => return Ok(process),
                    Err(err) => {
                        if self.maybe_retry_spawn_without_linux_bwrap(&err, false) {
                            continue;
                        }
                        return Err(err);
                    }
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        self.spawn_process(false, false)
    }

    pub(super) fn spawn_process_with_pager(
        &mut self,
        preserve_pager: bool,
    ) -> Result<WorkerProcess, WorkerError> {
        #[cfg(target_os = "linux")]
        {
            loop {
                match self.spawn_process(true, preserve_pager) {
                    Ok(process) => return Ok(process),
                    Err(err) => {
                        if self.maybe_retry_spawn_without_linux_bwrap(&err, preserve_pager) {
                            continue;
                        }
                        return Err(err);
                    }
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        self.spawn_process(true, preserve_pager)
    }

    fn spawn_process(
        &mut self,
        pager_spawn: bool,
        preserve_pager: bool,
    ) -> Result<WorkerProcess, WorkerError> {
        self.ensure_managed_network_proxy()?;
        #[cfg(target_os = "windows")]
        let prepared_windows_launch = self.ensure_windows_sandbox_launch()?;
        let SupervisorSpawn {
            process,
            initial_prompt,
            python_executable_hint,
        } = WorkerSupervisor::spawn(
            self.worker_launch.clone(),
            &self.exe_path,
            self.backend,
            &self.sandbox_state,
            WorkerSpawnContext {
                oversized_output: self.oversized_output,
                output_timeline: self.output_timeline.clone(),
                guardrail: self.guardrail.clone(),
                managed_network_proxy: self.managed_network_proxy.as_ref(),
                #[cfg(target_os = "windows")]
                prepared_windows_launch,
            },
        )?;
        self.active_python_executable_hint = python_executable_hint
            .or_else(|| configured_python_executable_hint(&self.worker_launch));
        self.remember_spawned_initial_prompt(initial_prompt);
        self.record_spawn();
        let payload = if pager_spawn {
            serde_json::json!({
                "backend": format!("{:?}", self.backend),
                "spawn_count": self.spawn_count,
                "preserve_pager": preserve_pager,
            })
        } else {
            serde_json::json!({
                "backend": format!("{:?}", self.backend),
                "spawn_count": self.spawn_count,
            })
        };
        crate::event_log::log("worker_spawn_end", payload);
        Ok(process)
    }

    #[cfg(target_os = "linux")]
    pub(super) fn maybe_retry_spawn_without_linux_bwrap(
        &mut self,
        err: &WorkerError,
        preserve_pager: bool,
    ) -> bool {
        match super::sandbox_state::managed_network_proxy_config_for_state(&self.sandbox_state) {
            Ok(None) => {}
            Ok(Some(_)) | Err(_) => return false,
        }
        if !self.sandbox_state.use_linux_sandbox_bwrap || !linux_sandbox_startup_retryable(err) {
            return false;
        }

        crate::event_log::log(
            "worker_spawn_retry_without_bwrap",
            serde_json::json!({
                "backend": format!("{:?}", self.backend),
                "error": err.to_string(),
            }),
        );

        self.linux_bwrap_fallback_disabled = true;
        self.sandbox_state.use_linux_sandbox_bwrap = false;
        self.sandbox_defaults.use_linux_sandbox_bwrap = false;
        if let Some(inherited_state) = self.inherited_sandbox_state.as_mut() {
            inherited_state.use_linux_sandbox_bwrap = false;
        }

        match self.oversized_output {
            OversizedOutputMode::Files => {
                self.reset_output_state_files(true);
                self.pending_output_tape
                    .append_stdout_status_line(LINUX_BWRAP_FALLBACK_NOTICE.as_bytes());
            }
            OversizedOutputMode::Pager => {
                self.reset_output_state_pager(true, preserve_pager);
                self.output_timeline.append_text(
                    LINUX_BWRAP_FALLBACK_NOTICE.as_bytes(),
                    false,
                    ContentOrigin::Server,
                );
            }
        }

        true
    }

    fn remember_spawned_initial_prompt(&mut self, initial_prompt: Option<InitialWorkerPrompt>) {
        match initial_prompt {
            Some(InitialWorkerPrompt::Immediate(raw_prompt)) => {
                self.remember_prompt(Some(raw_prompt));
            }
            Some(InitialWorkerPrompt::Waited(raw_prompt)) => {
                if let Some(prompt) = normalize_prompt(Some(raw_prompt)) {
                    self.last_prompt = Some(prompt);
                }
            }
            None => {}
        }
    }

    fn record_spawn(&mut self) {
        let now = std::time::Instant::now();
        self.last_spawn = Some(now);
        self.spawn_count = self.spawn_count.saturating_add(1);
    }

    #[cfg(target_os = "windows")]
    pub(super) fn ensure_windows_sandbox_launch(
        &mut self,
    ) -> Result<Option<crate::windows_sandbox::PreparedSandboxLaunch>, WorkerError> {
        if !backend_prepares_windows_sandbox_launch(self.backend)
            || !self.sandbox_state.sandbox_policy.requires_sandbox()
        {
            self.windows_sandbox_launch = None;
            return Ok(None);
        }

        let launch_matches = self.windows_sandbox_launch.as_ref().is_some_and(|launch| {
            let network_identity = windows_sandbox_network_identity_for_state(&self.sandbox_state);
            let Ok(network_identity) = network_identity else {
                return false;
            };
            launch.matches_with_network_identity(
                &self.sandbox_state.sandbox_policy,
                &self.sandbox_state.sandbox_cwd,
                &self.sandbox_state.session_temp_dir,
                &network_identity,
            )
        });
        if launch_matches {
            // Reuse the prepared Windows launch only while the effective worker
            // sandbox configuration still matches. Session temp ACLs are
            // refreshed separately on each spawn after the temp dir reset.
            crate::windows_sandbox::refresh_prepared_sandbox_launch_acl_state(
                self.windows_sandbox_launch
                    .as_ref()
                    .expect("matching launch must exist"),
            )
            .map_err(WorkerError::Sandbox)?;
            return Ok(self.windows_sandbox_launch.clone());
        }

        crate::event_log::log_lazy("worker_windows_sandbox_prepare_begin", || {
            worker_context_event_payload(&self.worker_launch, self.backend, &self.sandbox_state)
        });
        let network_identity = windows_sandbox_network_identity_for_state(&self.sandbox_state)?;
        let prepared = crate::windows_sandbox::prepare_sandbox_launch_with_network_identity(
            &self.sandbox_state.sandbox_policy,
            &self.sandbox_state.sandbox_cwd,
            &self.sandbox_state.session_temp_dir,
            network_identity,
        );
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(err) => return Err(WorkerError::Sandbox(err)),
        };
        crate::event_log::log(
            "worker_windows_sandbox_prepare_end",
            serde_json::json!({
                "status": "ok",
                "capability_sid": prepared.capability_sid(),
                "network_identity": match prepared.network_identity() {
                    crate::windows_sandbox::WindowsSandboxNetworkIdentity::CurrentUser => "current-user",
                    crate::windows_sandbox::WindowsSandboxNetworkIdentity::OfflineProxy(_) => "offline-proxy",
                },
            }),
        );
        self.windows_sandbox_launch = Some(prepared);

        Ok(self.windows_sandbox_launch.clone())
    }
}

#[cfg(target_os = "windows")]
fn windows_sandbox_network_identity_for_state(
    state: &crate::sandbox::SandboxState,
) -> Result<crate::windows_sandbox::WindowsSandboxNetworkIdentity, WorkerError> {
    if !windows_sandbox_requires_offline_proxy_identity(state) {
        return Ok(crate::windows_sandbox::WindowsSandboxNetworkIdentity::CurrentUser);
    }
    let setup = crate::windows_sandbox_setup::load_offline_setup().map_err(WorkerError::Sandbox)?;
    Ok(crate::windows_sandbox::WindowsSandboxNetworkIdentity::OfflineProxy(setup))
}

#[cfg(target_os = "windows")]
fn windows_sandbox_requires_offline_proxy_identity(state: &crate::sandbox::SandboxState) -> bool {
    if state.managed_network_policy.has_domain_restrictions() {
        return true;
    }
    matches!(
        state.sandbox_policy,
        crate::sandbox::SandboxPolicy::WorkspaceWrite {
            network_access: false,
            ..
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(any(target_os = "linux", target_family = "windows"))]
    use crate::oversized_output::OversizedOutputMode;
    #[cfg(target_family = "windows")]
    use crate::sandbox::ManagedNetworkPolicy;
    #[cfg(any(target_os = "linux", target_family = "windows"))]
    use crate::sandbox::SandboxPolicy;
    #[cfg(target_os = "linux")]
    use crate::sandbox::{SandboxState, SandboxStateUpdate};
    #[cfg(any(target_os = "linux", target_family = "windows"))]
    use crate::sandbox_cli::SandboxCliPlan;
    #[cfg(target_os = "linux")]
    use crate::sandbox_cli::resolve_effective_sandbox_state_with_defaults;
    #[cfg(target_os = "linux")]
    use crate::worker_process::test_support::contents_text;
    #[cfg(target_os = "linux")]
    use std::time::Duration;

    #[cfg(target_family = "windows")]
    fn force_windows_full_network_workspace_write(manager: &mut WorkerManager) {
        if let SandboxPolicy::WorkspaceWrite { network_access, .. } =
            &mut manager.sandbox_state.sandbox_policy
        {
            *network_access = true;
        }
    }

    #[cfg(target_family = "windows")]
    fn windows_workspace_write_state(network_access: bool) -> crate::sandbox::SandboxState {
        crate::sandbox::SandboxState {
            sandbox_policy: SandboxPolicy::WorkspaceWrite {
                writable_roots: Vec::new(),
                network_access,
                exclude_tmpdir_env_var: false,
                exclude_slash_tmp: false,
            },
            ..Default::default()
        }
    }

    #[cfg(target_family = "windows")]
    #[test]
    fn windows_network_identity_selection_scopes_offline_proxy_cases() {
        let read_only = crate::sandbox::SandboxState {
            sandbox_policy: SandboxPolicy::ReadOnly {
                network_access: false,
            },
            ..Default::default()
        };
        assert!(
            !windows_sandbox_requires_offline_proxy_identity(&read_only),
            "read-only should keep the current-user sandbox launch path"
        );

        let workspace_no_network = windows_workspace_write_state(false);
        assert!(
            windows_sandbox_requires_offline_proxy_identity(&workspace_no_network),
            "workspace-write without full network should use the offline proxy identity"
        );

        let workspace_full_network = windows_workspace_write_state(true);
        assert!(
            !windows_sandbox_requires_offline_proxy_identity(&workspace_full_network),
            "full-network workspace-write without domain rules should stay current-user"
        );

        let mut managed_domains = windows_workspace_write_state(true);
        managed_domains.managed_network_policy = ManagedNetworkPolicy {
            allowed_domains: vec!["example.com".to_string()],
            denied_domains: Vec::new(),
            allow_local_binding: false,
        };
        assert!(
            windows_sandbox_requires_offline_proxy_identity(&managed_domains),
            "managed domain rules should use the offline proxy identity"
        );
    }

    #[test]
    fn python_backend_prepares_windows_sandbox_launch() {
        assert!(
            backend_prepares_windows_sandbox_launch(Backend::Python),
            "Python uses the embedded worker wrapper and needs the prepared Windows capability SID"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_bwrap_startup_retry_disables_bwrap_and_announces_fallback() {
        let mut manager = WorkerManager::new(
            Backend::R,
            SandboxCliPlan::default(),
            OversizedOutputMode::Files,
        )
        .expect("worker manager");
        manager.sandbox_state.use_linux_sandbox_bwrap = true;
        manager.sandbox_defaults.use_linux_sandbox_bwrap = true;
        manager.inherited_sandbox_state = Some(SandboxState {
            use_linux_sandbox_bwrap: true,
            ..manager.sandbox_state.clone()
        });

        let retry = manager.maybe_retry_spawn_without_linux_bwrap(
            &WorkerError::Protocol("ipc disconnected while waiting for backend info".to_string()),
            false,
        );

        assert!(
            retry,
            "expected backend-info disconnect to trigger bwrap fallback"
        );
        assert!(
            !manager.sandbox_state.use_linux_sandbox_bwrap,
            "expected effective sandbox state to disable bwrap after fallback"
        );
        assert!(
            !manager.sandbox_defaults.use_linux_sandbox_bwrap,
            "expected sandbox defaults to disable bwrap after fallback"
        );
        assert!(
            manager
                .inherited_sandbox_state
                .as_ref()
                .is_some_and(|state| !state.use_linux_sandbox_bwrap),
            "expected inherited sandbox state to disable bwrap after fallback"
        );

        let output = manager.pending_output_tape.drain_final_output();
        let text = contents_text(&output.contents);
        assert!(
            text.contains("continuing without bwrap"),
            "expected fallback notice in visible output, got: {text:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_bwrap_startup_retry_is_disabled_for_managed_domains() {
        let mut manager = WorkerManager::new(
            Backend::Python,
            SandboxCliPlan::default(),
            OversizedOutputMode::Files,
        )
        .expect("worker manager");
        manager.sandbox_state.sandbox_policy = SandboxPolicy::WorkspaceWrite {
            writable_roots: Vec::new(),
            network_access: true,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
        };
        manager.sandbox_state.use_linux_sandbox_bwrap = true;
        manager.sandbox_state.managed_network_policy.allowed_domains =
            vec!["example.com".to_string()];
        manager.sandbox_defaults.use_linux_sandbox_bwrap = true;

        let retry = manager.maybe_retry_spawn_without_linux_bwrap(
            &WorkerError::Protocol("ipc disconnected while waiting for backend info".to_string()),
            false,
        );

        assert!(
            !retry,
            "managed domains must fail closed instead of falling back"
        );
        assert!(
            manager.sandbox_state.use_linux_sandbox_bwrap,
            "fallback must not disable bwrap for managed-domain enforcement"
        );
        assert!(
            manager.sandbox_defaults.use_linux_sandbox_bwrap,
            "fallback must not mutate defaults for managed-domain enforcement"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_bwrap_startup_retry_allows_inactive_managed_domains_without_network() {
        let mut manager = WorkerManager::new(
            Backend::Python,
            SandboxCliPlan::default(),
            OversizedOutputMode::Files,
        )
        .expect("worker manager");
        manager.sandbox_state.sandbox_policy = SandboxPolicy::WorkspaceWrite {
            writable_roots: Vec::new(),
            network_access: false,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
        };
        manager.sandbox_state.use_linux_sandbox_bwrap = true;
        manager.sandbox_state.managed_network_policy.allowed_domains =
            vec!["example.com".to_string()];
        manager.sandbox_defaults.use_linux_sandbox_bwrap = true;

        let retry = manager.maybe_retry_spawn_without_linux_bwrap(
            &WorkerError::Protocol("ipc disconnected while waiting for backend info".to_string()),
            false,
        );

        assert!(
            retry,
            "inactive managed domain rules should not block the no-network bwrap fallback"
        );
        assert!(
            !manager.sandbox_state.use_linux_sandbox_bwrap,
            "fallback should disable bwrap when no managed-domain enforcement is active"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_bwrap_startup_retry_stays_disabled_after_followup_codex_meta_update() {
        use crate::sandbox::sandbox_state_update_from_codex_meta;
        use serde_json::json;

        let plan = SandboxCliPlan {
            operations: vec![crate::sandbox_cli::SandboxCliOperation::SetMode(
                crate::sandbox_cli::SandboxModeArg::Inherit,
            )],
        };
        let mut manager = WorkerManager::new(Backend::Python, plan, OversizedOutputMode::Files)
            .expect("worker manager");
        let mut inherited_state = manager.sandbox_defaults.clone();
        inherited_state.apply_update(SandboxStateUpdate {
            sandbox_policy: SandboxPolicy::WorkspaceWrite {
                writable_roots: Vec::new(),
                network_access: false,
                exclude_tmpdir_env_var: false,
                exclude_slash_tmp: false,
            },
            sandbox_cwd: Some(std::env::temp_dir()),
            use_linux_sandbox_bwrap: Some(true),
            use_legacy_landlock: None,
        });
        manager.inherited_sandbox_state = Some(inherited_state.clone());
        manager.sandbox_state = resolve_effective_sandbox_state_with_defaults(
            &manager.sandbox_plan,
            Some(&inherited_state),
            &manager.sandbox_defaults,
        )
        .expect("resolved initial sandbox state");
        assert!(
            manager.sandbox_state.use_linux_sandbox_bwrap,
            "test setup should start with bwrap enabled"
        );

        let retry = manager.maybe_retry_spawn_without_linux_bwrap(
            &WorkerError::Protocol("ipc disconnected while waiting for backend info".to_string()),
            false,
        );
        assert!(retry, "expected startup failure to disable bwrap");

        let sandbox_cwd = std::env::temp_dir();
        let sandbox_cwd_uri = url::Url::from_file_path(&sandbox_cwd)
            .expect("absolute sandbox cwd should convert to file URI")
            .to_string();
        let update = sandbox_state_update_from_codex_meta(&json!({
            "permissionProfile": {
                "type": "managed",
                "file_system": {
                    "type": "restricted",
                    "entries": [
                        {
                            "path": {
                                "type": "special",
                                "value": { "kind": "root" }
                            },
                            "access": "read"
                        },
                        {
                            "path": {
                                "type": "special",
                                "value": { "kind": "project_roots" }
                            },
                            "access": "write"
                        },
                        {
                            "path": {
                                "type": "special",
                                "value": { "kind": "tmpdir" }
                            },
                            "access": "write"
                        },
                        {
                            "path": {
                                "type": "special",
                                "value": { "kind": "slash_tmp" }
                            },
                            "access": "write"
                        }
                    ]
                },
                "network": "restricted"
            },
            "sandboxCwd": sandbox_cwd_uri,
            "useLegacyLandlock": false,
            "codexLinuxSandboxExe": "/tmp/codex-linux-sandbox"
        }))
        .expect("Codex sandbox metadata");
        manager
            .update_sandbox_state(update, Duration::from_millis(1))
            .expect("follow-up sandbox state");

        assert!(
            !manager.sandbox_state.use_linux_sandbox_bwrap,
            "follow-up Codex metadata should preserve the local no-bwrap fallback"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_bwrap_startup_retry_stays_disabled_after_followup_plan_bwrap_override() {
        use crate::sandbox::sandbox_state_update_from_codex_meta;
        use serde_json::json;

        let plan = SandboxCliPlan {
            operations: vec![
                crate::sandbox_cli::SandboxCliOperation::SetMode(
                    crate::sandbox_cli::SandboxModeArg::Inherit,
                ),
                crate::sandbox_cli::SandboxCliOperation::Config(
                    crate::sandbox_cli::SandboxConfigOperation::SetUseLinuxSandboxBwrap(true),
                ),
            ],
        };
        let mut manager = WorkerManager::new(Backend::Python, plan, OversizedOutputMode::Files)
            .expect("worker manager");
        let mut inherited_state = manager.sandbox_defaults.clone();
        inherited_state.apply_update(SandboxStateUpdate {
            sandbox_policy: SandboxPolicy::WorkspaceWrite {
                writable_roots: Vec::new(),
                network_access: false,
                exclude_tmpdir_env_var: false,
                exclude_slash_tmp: false,
            },
            sandbox_cwd: Some(std::env::temp_dir()),
            use_linux_sandbox_bwrap: None,
            use_legacy_landlock: None,
        });
        manager.inherited_sandbox_state = Some(inherited_state.clone());
        manager.sandbox_state = resolve_effective_sandbox_state_with_defaults(
            &manager.sandbox_plan,
            Some(&inherited_state),
            &manager.sandbox_defaults,
        )
        .expect("resolved initial sandbox state");
        assert!(
            manager.sandbox_state.use_linux_sandbox_bwrap,
            "test setup should start with the plan-level bwrap override enabled"
        );

        let retry = manager.maybe_retry_spawn_without_linux_bwrap(
            &WorkerError::Protocol("ipc disconnected while waiting for backend info".to_string()),
            false,
        );
        assert!(retry, "expected startup failure to disable bwrap");

        let sandbox_cwd = std::env::temp_dir();
        let sandbox_cwd_uri = url::Url::from_file_path(&sandbox_cwd)
            .expect("absolute sandbox cwd should convert to file URI")
            .to_string();
        let update = sandbox_state_update_from_codex_meta(&json!({
            "permissionProfile": {
                "type": "managed",
                "file_system": {
                    "type": "restricted",
                    "entries": [
                        {
                            "path": {
                                "type": "special",
                                "value": { "kind": "root" }
                            },
                            "access": "read"
                        },
                        {
                            "path": {
                                "type": "special",
                                "value": { "kind": "project_roots" }
                            },
                            "access": "write"
                        },
                        {
                            "path": {
                                "type": "special",
                                "value": { "kind": "tmpdir" }
                            },
                            "access": "write"
                        },
                        {
                            "path": {
                                "type": "special",
                                "value": { "kind": "slash_tmp" }
                            },
                            "access": "write"
                        }
                    ]
                },
                "network": "restricted"
            },
            "sandboxCwd": sandbox_cwd_uri,
            "useLegacyLandlock": false,
            "codexLinuxSandboxExe": "/tmp/codex-linux-sandbox"
        }))
        .expect("Codex sandbox metadata");
        manager
            .update_sandbox_state(update, Duration::from_millis(1))
            .expect("follow-up sandbox state");

        assert!(
            !manager.sandbox_state.use_linux_sandbox_bwrap,
            "plan-level bwrap overrides should not re-enable bwrap after the local fallback"
        );
    }

    #[cfg(target_family = "windows")]
    #[test]
    fn windows_sandbox_prepare_access_denied_fails_fast() {
        let _guard = crate::windows_sandbox::prepare_sandbox_launch_test_mutex()
            .lock()
            .expect("windows sandbox test mutex");
        crate::windows_sandbox::set_prepare_sandbox_launch_test_error(Some(
            "failed to prepare writable ACL on 'C:\\workspace': SetNamedSecurityInfoW failed: 5"
                .to_string(),
        ));

        let mut manager = WorkerManager::new(
            Backend::R,
            SandboxCliPlan::default(),
            OversizedOutputMode::Files,
        )
        .expect("worker manager");
        force_windows_full_network_workspace_write(&mut manager);
        let result = manager.ensure_windows_sandbox_launch();

        crate::windows_sandbox::set_prepare_sandbox_launch_test_error(None);

        assert!(
            matches!(
                result,
                Err(WorkerError::Sandbox(ref message))
                    if message.contains("SetNamedSecurityInfoW failed: 5")
            ),
            "access-denied prepare failures should abort launch preparation, got: {result:?}"
        );
        assert!(
            manager.windows_sandbox_launch.is_none(),
            "failed launch preparation should not cache a prepared launch"
        );
    }

    #[cfg(target_family = "windows")]
    #[test]
    fn windows_python_sandbox_prepare_access_denied_fails_fast() {
        let _guard = crate::windows_sandbox::prepare_sandbox_launch_test_mutex()
            .lock()
            .expect("windows sandbox test mutex");
        crate::windows_sandbox::set_prepare_sandbox_launch_test_error(Some(
            "failed to prepare writable ACL on 'C:\\workspace': SetNamedSecurityInfoW failed: 5"
                .to_string(),
        ));

        let mut manager = WorkerManager::new(
            Backend::Python,
            SandboxCliPlan::default(),
            OversizedOutputMode::Files,
        )
        .expect("worker manager");
        force_windows_full_network_workspace_write(&mut manager);
        let result = manager.ensure_windows_sandbox_launch();

        crate::windows_sandbox::set_prepare_sandbox_launch_test_error(None);

        assert!(
            matches!(
                result,
                Err(WorkerError::Sandbox(ref message))
                    if message.contains("SetNamedSecurityInfoW failed: 5")
            ),
            "Python access-denied prepare failures should abort launch preparation, got: {result:?}"
        );
        assert!(
            manager.windows_sandbox_launch.is_none(),
            "failed Python launch preparation should not cache a prepared launch"
        );
    }

    #[cfg(target_family = "windows")]
    #[test]
    fn windows_sandbox_cache_hit_refreshes_prepared_launch_acl_state_before_reuse() {
        let _guard = crate::windows_sandbox::prepare_sandbox_launch_test_mutex()
            .lock()
            .expect("windows sandbox test mutex");

        let mut manager = WorkerManager::new(
            Backend::R,
            SandboxCliPlan::default(),
            OversizedOutputMode::Files,
        )
        .expect("worker manager");
        force_windows_full_network_workspace_write(&mut manager);

        let first = manager
            .ensure_windows_sandbox_launch()
            .expect("initial launch preparation should succeed");
        assert!(
            first.is_some(),
            "initial launch preparation should populate the prepared-launch cache"
        );

        crate::windows_sandbox::set_apply_prepared_launch_acl_state_test_error(Some(
            "cache hit should refresh ACL state".to_string(),
        ));

        let second = manager.ensure_windows_sandbox_launch();

        crate::windows_sandbox::set_apply_prepared_launch_acl_state_test_error(None);

        assert!(
            matches!(
                second,
                Err(WorkerError::Sandbox(ref err))
                    if err.contains("cache hit should refresh ACL state")
            ),
            "cache hits should refresh ACL state before reusing the prepared launch, got: {second:?}"
        );
        assert_eq!(
            manager.windows_sandbox_launch, first,
            "cache-hit refresh failures should preserve the cached launch for later retries"
        );
    }
}
