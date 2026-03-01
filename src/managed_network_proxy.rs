use std::collections::HashMap;
use std::sync::Arc;

use crate::sandbox::SandboxState;
use async_trait::async_trait;
use codex_network_proxy::ConfigReloader;
use codex_network_proxy::ConfigState;
use codex_network_proxy::NetworkMode;
use codex_network_proxy::NetworkProxy;
use codex_network_proxy::NetworkProxyConfig;
use codex_network_proxy::NetworkProxyConstraints;
use codex_network_proxy::NetworkProxyHandle;
use codex_network_proxy::NetworkProxyState;
use codex_network_proxy::build_config_state;

pub struct ManagedNetworkProxy {
    runtime: Option<tokio::runtime::Runtime>,
    handle: Option<NetworkProxyHandle>,
}

impl ManagedNetworkProxy {
    pub fn start_for_state(
        state: &SandboxState,
        env: &mut HashMap<String, String>,
    ) -> Result<Option<Self>, String> {
        if !state.managed_network_policy.is_enabled()
            || !state.sandbox_policy.has_full_network_access()
        {
            return Ok(None);
        }

        let mut config = NetworkProxyConfig::default();
        config.network.enabled = true;
        config.network.mode = NetworkMode::Full;
        config.network.allowed_domains = state.managed_network_policy.allowed_domains.clone();
        config.network.denied_domains = state.managed_network_policy.denied_domains.clone();
        config.network.allow_local_binding = state.managed_network_policy.allow_local_binding;

        let config_state = build_config_state(config, NetworkProxyConstraints::default())
            .map_err(|err| format!("failed to build managed network proxy config: {err}"))?;
        let state = NetworkProxyState::with_reloader(
            config_state.clone(),
            Arc::new(StaticConfigReloader {
                state: config_state,
            }),
        );

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|err| format!("failed to build managed network proxy runtime: {err}"))?;

        let proxy = runtime
            .block_on(async {
                NetworkProxy::builder()
                    .state(Arc::new(state))
                    .managed_by_codex(false)
                    .build()
                    .await
            })
            .map_err(|err| format!("failed to build managed network proxy: {err:#}"))?;

        let handle = runtime
            .block_on(proxy.run())
            .map_err(|err| format!("failed to run managed network proxy: {err:#}"))?;

        proxy.apply_to_env(env);

        Ok(Some(Self {
            runtime: Some(runtime),
            handle: Some(handle),
        }))
    }
}

impl Drop for ManagedNetworkProxy {
    fn drop(&mut self) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        let Some(runtime) = self.runtime.take() else {
            return;
        };

        let shutdown = move || {
            let _ = runtime.block_on(async { handle.shutdown().await });
        };

        if tokio::runtime::Handle::try_current().is_ok() {
            let _ = std::thread::Builder::new()
                .name("mcp-managed-network-proxy-shutdown".to_string())
                .spawn(shutdown);
        } else {
            shutdown();
        }
    }
}

#[derive(Clone)]
struct StaticConfigReloader {
    state: ConfigState,
}

#[async_trait]
impl ConfigReloader for StaticConfigReloader {
    fn source_label(&self) -> String {
        "mcp-repl managed network proxy static config".to_string()
    }

    async fn maybe_reload(&self) -> anyhow::Result<Option<ConfigState>> {
        Ok(None)
    }

    async fn reload_now(&self) -> anyhow::Result<ConfigState> {
        Ok(self.state.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::ManagedNetworkProxy;
    use crate::sandbox::{ManagedNetworkPolicy, SandboxPolicy, SandboxState};
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn managed_state(network_access: bool) -> SandboxState {
        SandboxState {
            sandbox_policy: SandboxPolicy::WorkspaceWrite {
                writable_roots: Vec::new(),
                network_access,
                exclude_tmpdir_env_var: false,
                exclude_slash_tmp: false,
            },
            sandbox_cwd: PathBuf::from("/"),
            codex_linux_sandbox_exe: None,
            use_linux_sandbox_bwrap: false,
            managed_network_policy: ManagedNetworkPolicy {
                enabled: true,
                allowed_domains: vec!["example.com".to_string()],
                denied_domains: vec!["blocked.example.com".to_string()],
                allow_local_binding: false,
            },
            session_temp_dir: std::env::temp_dir().join("mcp-repl-managed-proxy-test"),
        }
    }

    #[test]
    fn start_for_state_skips_when_network_disabled() {
        let mut env = HashMap::new();
        let state = managed_state(false);
        let proxy = ManagedNetworkProxy::start_for_state(&state, &mut env)
            .expect("start_for_state should succeed");
        assert!(proxy.is_none());
        assert!(!env.contains_key("HTTP_PROXY"));
    }

    #[test]
    fn start_for_state_sets_proxy_env_when_managed_network_is_enabled() {
        let mut env = HashMap::new();
        let state = managed_state(true);
        let proxy = match ManagedNetworkProxy::start_for_state(&state, &mut env) {
            Ok(proxy) => proxy,
            Err(err) if err.contains("Operation not permitted") => return,
            Err(err) => panic!("start_for_state should succeed: {err}"),
        };
        assert!(proxy.is_some(), "expected managed proxy to start");
        assert!(
            env.contains_key("HTTP_PROXY"),
            "managed proxy should set HTTP_PROXY"
        );
        assert!(
            env.contains_key("HTTPS_PROXY"),
            "managed proxy should set HTTPS_PROXY"
        );
        assert!(
            env.contains_key("NO_PROXY"),
            "managed proxy should set NO_PROXY"
        );
    }

    #[test]
    fn drop_inside_tokio_runtime_does_not_panic() {
        let mut env = HashMap::new();
        let state = managed_state(true);
        let proxy = match ManagedNetworkProxy::start_for_state(&state, &mut env) {
            Ok(Some(proxy)) => proxy,
            Ok(None) => panic!("expected managed proxy to start"),
            Err(err) if err.contains("Operation not permitted") => return,
            Err(err) => panic!("start_for_state should succeed: {err}"),
        };

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime should build");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(async move {
                drop(proxy);
            });
        }));

        assert!(
            result.is_ok(),
            "dropping managed proxy inside a tokio runtime should not panic"
        );
    }
}
