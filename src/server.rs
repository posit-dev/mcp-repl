use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ErrorData as McpError, JsonObject, Meta, ProtocolVersion, ServerCapabilities,
    ServerInfo,
};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub(crate) mod response;
#[cfg(test)]
mod tests;
mod timeouts;

use self::response::{
    ResponseState, TimeoutBundleReuse, strip_text_stream_meta, timeout_bundle_reuse_for_input,
};
use self::timeouts::{
    SANDBOX_UPDATE_TIMEOUT, apply_safety_margin, apply_tool_call_margin, parse_timeout,
};

use crate::backend::Backend;
use crate::oversized_output::OversizedOutputMode;
use crate::sandbox::{SANDBOX_STATE_META_CAPABILITY, SandboxStateUpdate};
use crate::sandbox_cli::{
    MISSING_INHERITED_SANDBOX_STATE_MESSAGE, SandboxCliPlan, sandbox_plan_requests_inherited_state,
};
use crate::worker_process::{WorkerError, WorkerManager};

const BUSY_FOLLOW_UP_RECHECK_WAIT: Duration = Duration::from_millis(25);

#[cfg(test)]
fn repl_tool_description_for_backend(
    backend: Backend,
    oversized_output: OversizedOutputMode,
) -> &'static str {
    match (backend, oversized_output) {
        (Backend::R, OversizedOutputMode::Files) => {
            include_str!("../docs/tool-descriptions/repl_tool_r.md")
        }
        (Backend::R, OversizedOutputMode::Pager) => {
            include_str!("../docs/tool-descriptions/repl_tool_r_pager.md")
        }
        (Backend::Python, OversizedOutputMode::Files) => {
            include_str!("../docs/tool-descriptions/repl_tool_python.md")
        }
        (Backend::Python, OversizedOutputMode::Pager) => {
            include_str!("../docs/tool-descriptions/repl_tool_python_pager.md")
        }
    }
}

#[derive(Clone)]
struct SharedServer {
    accepts_sandbox_state_meta: bool,
    state: Arc<Mutex<ServerState>>,
}

struct ServerState {
    worker: WorkerManager,
    response: ResponseState,
    oversized_output: OversizedOutputMode,
}

impl SharedServer {
    fn new(
        backend: Backend,
        sandbox_plan: SandboxCliPlan,
        oversized_output: OversizedOutputMode,
    ) -> Result<Self, WorkerError> {
        let accepts_sandbox_state_meta = sandbox_plan_requests_inherited_state(&sandbox_plan);
        Ok(Self {
            accepts_sandbox_state_meta,
            state: Arc::new(Mutex::new(ServerState {
                worker: WorkerManager::new(backend, sandbox_plan, oversized_output)?,
                response: ResponseState::new()?,
                oversized_output,
            })),
        })
    }

    fn state(&self) -> Arc<Mutex<ServerState>> {
        Arc::clone(&self.state)
    }

    fn accepts_sandbox_state_meta(&self) -> bool {
        self.accepts_sandbox_state_meta
    }

    /// Runs a closure with exclusive access to the combined worker/response state.
    /// This keeps reply finalization in the same critical section as the worker call it seals.
    async fn run_state<T, F>(&self, f: F) -> Result<T, McpError>
    where
        F: FnOnce(&mut ServerState) -> T + Send + 'static,
        T: Send + 'static,
    {
        let state = self.state.clone();
        tokio::task::spawn_blocking(move || {
            let mut state = state.lock().unwrap();
            f(&mut state)
        })
        .await
        .map_err(|err| McpError::internal_error(err.to_string(), None))
    }

    fn sandbox_state_update_for_tool_call(
        &self,
        meta: &Meta,
    ) -> Result<Option<SandboxStateUpdate>, WorkerError> {
        Self::sandbox_state_update_for_tool_call_meta(self.accepts_sandbox_state_meta(), meta)
    }

    fn sandbox_state_update_for_tool_call_meta(
        accepts_sandbox_state_meta: bool,
        meta: &Meta,
    ) -> Result<Option<SandboxStateUpdate>, WorkerError> {
        if !accepts_sandbox_state_meta {
            return Ok(None);
        }

        let Some(raw_meta) = meta.get(SANDBOX_STATE_META_CAPABILITY) else {
            return Err(WorkerError::Sandbox(
                MISSING_INHERITED_SANDBOX_STATE_MESSAGE.to_string(),
            ));
        };
        crate::sandbox::log_sandbox_state_meta(raw_meta);
        let update = crate::sandbox::sandbox_state_update_from_codex_meta(raw_meta)
            .map_err(WorkerError::Sandbox)?;
        Ok(Some(update))
    }

    fn apply_tool_call_sandbox_state(
        state: &mut ServerState,
        update: Option<SandboxStateUpdate>,
    ) -> Result<(), WorkerError> {
        let Some(update) = update else {
            return Ok(());
        };

        if state
            .worker
            .update_sandbox_state(update, SANDBOX_UPDATE_TIMEOUT)?
        {
            state.response.clear_active_timeout_bundle()?;
        }
        Ok(())
    }

    fn stage_tool_call_sandbox_state_for_reset(
        state: &mut ServerState,
        update: Option<SandboxStateUpdate>,
    ) -> Result<(), WorkerError> {
        let Some(update) = update else {
            return Ok(());
        };

        state.worker.stage_sandbox_state_update(update)
    }

    /// Executes one `repl` call and immediately finalizes the visible reply on the server side.
    /// The response layer needs `pending_request` after the worker call to decide transcript reuse.
    async fn run_write_input(
        &self,
        input: String,
        timeout: Duration,
        meta: Meta,
    ) -> Result<CallToolResult, McpError> {
        let worker_timeout = apply_tool_call_margin(timeout);
        let server_timeout = apply_safety_margin(timeout);
        let accepts_sandbox_state_meta = self.accepts_sandbox_state_meta();
        self.run_state(move |state| {
            let timeout_bundle_reuse = timeout_bundle_reuse_for_input(&input);
            let raw_input = input;
            let use_inline_pager_materialization =
                matches!(state.oversized_output, OversizedOutputMode::Pager);
            state.worker.refresh_timeout_marker();
            let parse_tool_call_sandbox_state = || {
                SharedServer::sandbox_state_update_for_tool_call_meta(
                    accepts_sandbox_state_meta,
                    &meta,
                )
            };
            let sandbox_state_result = if raw_input.is_empty() {
                // Empty-input polls may need the current tool call's metadata
                // only when they must spawn a worker to answer the call.
                match state.worker.empty_input_requires_spawn() {
                    Ok(true) => parse_tool_call_sandbox_state().and_then(|update| {
                        SharedServer::apply_tool_call_sandbox_state(state, update)
                    }),
                    Ok(false) => Ok(()),
                    Err(err) => Err(err),
                }
            } else {
                // A timed-out request still owns busy follow-ups, but a fresh
                // non-empty call after that request has already settled must
                // run under the current tool call's sandbox metadata.
                if state.worker.pending_request() {
                    state
                        .worker
                        .refresh_timeout_marker_with_wait(BUSY_FOLLOW_UP_RECHECK_WAIT);
                }
                if state.worker.pending_request() {
                    Ok(())
                } else {
                    parse_tool_call_sandbox_state().and_then(|update| {
                        SharedServer::apply_tool_call_sandbox_state(state, update)
                    })
                }
            };
            if let Err(err) = sandbox_state_result {
                let pending_request_after = state.worker.pending_request();
                let detached_prefix_item_count = state.worker.detached_prefix_item_count();
                let mut result = finalize_visible_reply(
                    state,
                    Err(err),
                    pending_request_after,
                    TimeoutBundleReuse::None,
                    detached_prefix_item_count,
                    use_inline_pager_materialization
                        && !pending_request_after
                        && !state.response.has_timeout_bundle_state(),
                );
                strip_text_stream_meta(&mut result);
                return result;
            }
            let result = state.worker.write_stdin(
                raw_input.clone(),
                worker_timeout,
                server_timeout,
                None,
                false,
                true,
            );
            let pending_request_after = state.worker.pending_request();
            let detached_prefix_item_count = state.worker.detached_prefix_item_count();
            let mut result = finalize_visible_reply(
                state,
                result,
                pending_request_after,
                timeout_bundle_reuse,
                detached_prefix_item_count,
                use_inline_pager_materialization
                    && !pending_request_after
                    && !state.response.has_timeout_bundle_state(),
            );
            strip_text_stream_meta(&mut result);
            result
        })
        .await
    }
}

fn server_info(advertise_sandbox_capabilities: bool) -> ServerInfo {
    let capabilities = if advertise_sandbox_capabilities {
        ServerCapabilities::builder()
            .enable_tools()
            .enable_experimental_with(sandbox_capabilities())
            .build()
    } else {
        ServerCapabilities::builder().enable_tools().build()
    };
    ServerInfo::new(capabilities).with_protocol_version(ProtocolVersion::V_2025_06_18)
}

#[derive(Clone, Copy)]
struct LoggedToolRouter<'a, S> {
    inner: &'a ToolRouter<S>,
}

impl<'a, S> LoggedToolRouter<'a, S>
where
    S: Send + Sync + 'static,
{
    fn new(inner: &'a ToolRouter<S>) -> Self {
        Self { inner }
    }

    async fn call(
        &self,
        context: rmcp::handler::server::tool::ToolCallContext<'_, S>,
    ) -> Result<CallToolResult, McpError> {
        let tool = context.name.clone();
        crate::event_log::log_lazy("tool_call_begin", || {
            let arguments = context.arguments.clone().unwrap_or_default();
            let task = context.task.clone();
            json!({
                "tool": tool.as_ref(),
                "arguments": arguments,
                "task": task,
                "meta": context.request_context.meta.clone(),
            })
        });
        let result = self.inner.call(context).await;
        match &result {
            Ok(result) => {
                crate::event_log::log_lazy("tool_call_end", || {
                    let serialized = serde_json::to_value(result)
                        .unwrap_or_else(|err| json!({"serialize_error": err.to_string()}));
                    json!({
                        "tool": tool.as_ref(),
                        "result": serialized,
                    })
                });
            }
            Err(err) => {
                crate::event_log::log_lazy("tool_call_error", || {
                    json!({
                        "tool": tool.as_ref(),
                        "error": err.to_string(),
                    })
                });
            }
        }
        result
    }

    fn list_all(&self) -> Vec<rmcp::model::Tool> {
        self.inner.list_all()
    }

    fn get(&self, name: &str) -> Option<&rmcp::model::Tool> {
        self.inner.get(name)
    }
}

macro_rules! define_backend_tool_server {
    ($server_ty:ident, $repl_doc_path:literal) => {
        #[derive(Clone)]
        struct $server_ty {
            shared: SharedServer,
            tool_router: ToolRouter<Self>,
        }

        #[tool_router]
        impl $server_ty {
            fn new(
                backend: Backend,
                sandbox_plan: SandboxCliPlan,
                oversized_output: OversizedOutputMode,
            ) -> Result<Self, WorkerError> {
                Ok(Self {
                    shared: SharedServer::new(backend, sandbox_plan, oversized_output)?,
                    tool_router: Self::tool_router(),
                })
            }

            fn get_info(&self) -> ServerInfo {
                server_info(self.shared.accepts_sandbox_state_meta())
            }

            fn logged_tool_router(&self) -> LoggedToolRouter<'_, Self> {
                LoggedToolRouter::new(&self.tool_router)
            }

            #[doc = include_str!($repl_doc_path)]
            #[tool(
                name = "repl",
                annotations(
                    read_only_hint = false,
                    destructive_hint = false,
                    open_world_hint = false
                )
            )]
            async fn repl(
                &self,
                meta: Meta,
                params: Parameters<ReplArgs>,
            ) -> Result<CallToolResult, McpError> {
                let ReplArgs { input, timeout_ms } = params.0;
                let timeout = resolve_timeout_ms(timeout_ms, "repl", true)?;
                self.shared.run_write_input(input, timeout, meta).await
            }

            #[doc = include_str!("../docs/tool-descriptions/repl_reset_tool.md")]
            #[tool(
                name = "repl_reset",
                annotations(
                    read_only_hint = false,
                    destructive_hint = false,
                    open_world_hint = false
                )
            )]
            async fn repl_reset(
                &self,
                meta: Meta,
                _params: Parameters<ReplResetArgs>,
            ) -> Result<CallToolResult, McpError> {
                let timeout = parse_timeout(None, "repl_reset", false)?;
                let worker_timeout = apply_tool_call_margin(timeout);
                let sandbox_state_update = self.shared.sandbox_state_update_for_tool_call(&meta);
                let result = self
                    .shared
                    .run_state(move |state| {
                        let sandbox_state_result = match &sandbox_state_update {
                            Ok(update) => SharedServer::stage_tool_call_sandbox_state_for_reset(
                                state,
                                update.clone(),
                            ),
                            Err(WorkerError::Sandbox(message)) => {
                                Err(WorkerError::Sandbox(message.clone()))
                            }
                            Err(err) => Err(WorkerError::Sandbox(err.to_string())),
                        };
                        if let Err(err) = sandbox_state_result {
                            let pending_request_after = state.worker.pending_request();
                            let mut result = finalize_visible_reply(
                                state,
                                Err(err),
                                pending_request_after,
                                TimeoutBundleReuse::None,
                                0,
                                true,
                            );
                            strip_text_stream_meta(&mut result);
                            return result;
                        }
                        let result = state.worker.restart(worker_timeout);
                        let pending_request_after = state.worker.pending_request();
                        let mut result = finalize_visible_reply(
                            state,
                            result,
                            pending_request_after,
                            TimeoutBundleReuse::None,
                            0,
                            true,
                        );
                        strip_text_stream_meta(&mut result);
                        result
                    })
                    .await?;
                Ok(result)
            }
        }

        #[tool_handler(router = self.logged_tool_router())]
        impl ServerHandler for $server_ty {
            fn get_info(&self) -> ServerInfo {
                $server_ty::get_info(self)
            }
        }
    };
}

fn finalize_visible_reply(
    state: &mut ServerState,
    result: Result<crate::worker_protocol::WorkerReply, WorkerError>,
    pending_request_after: bool,
    timeout_bundle_reuse: TimeoutBundleReuse,
    detached_prefix_item_count: usize,
    use_inline_pager_materialization: bool,
) -> CallToolResult {
    match state.oversized_output {
        OversizedOutputMode::Files => state.response.finalize_worker_result(
            result,
            pending_request_after,
            timeout_bundle_reuse,
            detached_prefix_item_count,
        ),
        OversizedOutputMode::Pager if use_inline_pager_materialization => state
            .response
            .materialize_worker_result_inline(result, detached_prefix_item_count),
        OversizedOutputMode::Pager => state.response.finalize_worker_result(
            result,
            pending_request_after,
            timeout_bundle_reuse,
            detached_prefix_item_count,
        ),
    }
}

define_backend_tool_server!(RFilesToolServer, "../docs/tool-descriptions/repl_tool_r.md");
define_backend_tool_server!(
    RPagerToolServer,
    "../docs/tool-descriptions/repl_tool_r_pager.md"
);
define_backend_tool_server!(
    PythonFilesToolServer,
    "../docs/tool-descriptions/repl_tool_python.md"
);
define_backend_tool_server!(
    PythonPagerToolServer,
    "../docs/tool-descriptions/repl_tool_python_pager.md"
);

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReplArgs {
    input: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Deserialize, JsonSchema, Default)]
#[serde(deny_unknown_fields)]
struct ReplResetArgs {}

fn resolve_timeout_ms(
    timeout_ms: Option<u64>,
    tool_name: &str,
    allow_zero: bool,
) -> Result<Duration, McpError> {
    let timeout_secs = timeout_ms.map(|value| Duration::from_millis(value).as_secs_f64());
    parse_timeout(timeout_secs, tool_name, allow_zero)
}

fn sandbox_capabilities() -> BTreeMap<String, JsonObject> {
    let mut capability = JsonObject::new();
    capability.insert("version".to_string(), json!("1.0.0"));
    let mut experimental = BTreeMap::new();
    experimental.insert(SANDBOX_STATE_META_CAPABILITY.to_string(), capability);
    experimental
}

async fn run_backend_server<S>(
    service: S,
    shutdown_state: Arc<Mutex<ServerState>>,
) -> Result<(), Box<dyn std::error::Error>>
where
    S: ServerHandler + Send + Sync + Clone + 'static,
{
    let warm_state = shutdown_state.clone();
    thread::spawn(move || {
        crate::event_log::log("worker_warm_start_begin", json!({}));
        let mut state = warm_state.lock().unwrap();
        if let Err(err) = state.worker.warm_start() {
            eprintln!("worker warm start error: {err}");
            crate::event_log::log(
                "worker_warm_start_error",
                json!({
                    "error": err.to_string(),
                }),
            );
            return;
        }
        crate::event_log::log("worker_warm_start_end", json!({"status": "ok"}));
    });

    crate::event_log::log("server_listen_begin", json!({}));
    let result: Result<(), Box<dyn std::error::Error>> = async {
        let running = rmcp::serve_server(service, rmcp::transport::stdio()).await?;
        running
            .waiting()
            .await
            .map(|_| ())
            .map_err(|err| err.into())
    }
    .await;

    {
        let mut state = shutdown_state.lock().unwrap();
        state.worker.shutdown();
        if let Err(err) = state.response.shutdown() {
            eprintln!("output bundle cleanup error: {err}");
            crate::event_log::log(
                "output_bundle_cleanup_error",
                json!({
                    "error": err.to_string(),
                }),
            );
        }
    }
    match &result {
        Ok(()) => crate::event_log::log("server_listen_end", json!({"status": "ok"})),
        Err(err) => crate::event_log::log(
            "server_listen_end",
            json!({
                "status": "error",
                "error": err.to_string(),
            }),
        ),
    }
    result
}

pub async fn run(
    backend: Backend,
    sandbox_plan: SandboxCliPlan,
    oversized_output: OversizedOutputMode,
) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("starting mcp-repl server");
    crate::event_log::log(
        "server_run_begin",
        json!({
            "backend": format!("{backend:?}"),
        }),
    );
    match backend {
        Backend::R => match oversized_output {
            OversizedOutputMode::Files => {
                let service = RFilesToolServer::new(backend, sandbox_plan, oversized_output)?;
                run_backend_server(service.clone(), service.shared.state()).await
            }
            OversizedOutputMode::Pager => {
                let service = RPagerToolServer::new(backend, sandbox_plan, oversized_output)?;
                run_backend_server(service.clone(), service.shared.state()).await
            }
        },
        Backend::Python => match oversized_output {
            OversizedOutputMode::Files => {
                let service = PythonFilesToolServer::new(backend, sandbox_plan, oversized_output)?;
                run_backend_server(service.clone(), service.shared.state()).await
            }
            OversizedOutputMode::Pager => {
                let service = PythonPagerToolServer::new(backend, sandbox_plan, oversized_output)?;
                run_backend_server(service.clone(), service.shared.state()).await
            }
        },
    }
}
