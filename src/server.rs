use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, CustomNotification, CustomRequest, CustomResult, ErrorCode,
    ErrorData as McpError, JsonObject, ProtocolVersion, ServerCapabilities, ServerInfo,
};
use rmcp::{RoleServer, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

mod response;
#[cfg(test)]
mod tests;
mod timeouts;

use self::response::ResponseState;
use self::timeouts::{
    SANDBOX_UPDATE_TIMEOUT, apply_safety_margin, apply_tool_call_margin, parse_timeout,
};

use crate::backend::Backend;
use crate::sandbox::{SANDBOX_STATE_CAPABILITY, SANDBOX_STATE_METHOD, SandboxStateUpdate};
use crate::sandbox_cli::SandboxCliPlan;
use crate::worker_process::{WorkerError, WorkerManager};

#[cfg(test)]
fn repl_tool_description_for_backend(backend: Backend) -> &'static str {
    match backend {
        Backend::R => include_str!("../docs/tool-descriptions/repl_tool_r.md"),
        Backend::Python => include_str!("../docs/tool-descriptions/repl_tool_python.md"),
    }
}

#[derive(Clone)]
struct SharedServer {
    state: Arc<Mutex<ServerState>>,
}

struct ServerState {
    worker: WorkerManager,
    response: ResponseState,
}

impl SharedServer {
    fn new(backend: Backend, sandbox_plan: SandboxCliPlan) -> Result<Self, WorkerError> {
        Ok(Self {
            state: Arc::new(Mutex::new(ServerState {
                worker: WorkerManager::new(backend, sandbox_plan)?,
                response: ResponseState::new()?,
            })),
        })
    }

    fn state(&self) -> Arc<Mutex<ServerState>> {
        Arc::clone(&self.state)
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

    /// Executes one `repl` call and immediately finalizes the visible reply on the server side.
    /// The response layer needs `pending_request` after the worker call to decide transcript reuse.
    async fn run_write_input(
        &self,
        input: String,
        timeout: Duration,
    ) -> Result<CallToolResult, McpError> {
        let worker_timeout = apply_tool_call_margin(timeout);
        let server_timeout = apply_safety_margin(timeout);
        self.run_state(move |state| {
            let reuse_active_timeout_bundle = should_reuse_timeout_bundle(&input);
            let result =
                state
                    .worker
                    .write_stdin(input, worker_timeout, server_timeout, None, false);
            let detached_prefix_item_count = state.worker.detached_prefix_item_count();
            state.response.finalize_worker_result(
                result,
                state.worker.pending_request(),
                reuse_active_timeout_bundle,
                detached_prefix_item_count,
            )
        })
        .await
    }

    async fn on_custom_request(&self, request: CustomRequest) -> Result<CustomResult, McpError> {
        crate::event_log::log(
            "custom_request_received",
            json!({
                "method": request.method.clone(),
                "params": request.params.clone(),
            }),
        );
        crate::sandbox::log_sandbox_state_event(&request.method, request.params.as_ref());
        if request.method != SANDBOX_STATE_METHOD {
            crate::event_log::log(
                "custom_request_rejected",
                json!({
                    "method": request.method.clone(),
                    "reason": "method_not_found",
                }),
            );
            return Err(McpError::new(
                ErrorCode::METHOD_NOT_FOUND,
                request.method,
                None,
            ));
        }

        let update = request
            .params_as::<SandboxStateUpdate>()
            .map_err(|err| McpError::invalid_params(err.to_string(), None))?
            .ok_or_else(|| McpError::invalid_params("missing sandbox state params", None))?;
        let update_for_log = serde_json::to_value(&update)
            .unwrap_or_else(|err| json!({"serialize_error": err.to_string()}));

        let outcome = self
            .run_state(move |state| {
                let outcome = state
                    .worker
                    .update_sandbox_state(update, SANDBOX_UPDATE_TIMEOUT);
                if matches!(outcome, Ok(true))
                    && let Err(err) = state.response.clear_active_timeout_bundle()
                {
                    return Err(err);
                }
                outcome
            })
            .await?;
        match outcome {
            Ok(changed) => {
                crate::event_log::log(
                    "sandbox_state_request_applied",
                    json!({
                        "changed": changed,
                        "update": update_for_log,
                    }),
                );
            }
            Err(err) => {
                crate::event_log::log(
                    "sandbox_state_request_failed",
                    json!({
                        "error": err.to_string(),
                        "update": update_for_log,
                    }),
                );
                return Err(McpError::internal_error(err.to_string(), None));
            }
        }

        Ok(CustomResult::new(json!({})))
    }

    async fn on_custom_notification(&self, notification: CustomNotification) {
        crate::event_log::log(
            "custom_notification_received",
            json!({
                "method": notification.method.clone(),
                "params": notification.params.clone(),
            }),
        );
        crate::sandbox::log_sandbox_state_event(&notification.method, notification.params.as_ref());
        if notification.method != SANDBOX_STATE_METHOD {
            return;
        }

        let update = match notification.params_as::<SandboxStateUpdate>() {
            Ok(Some(update)) => update,
            Ok(None) => {
                eprintln!("sandbox update missing params");
                crate::event_log::log(
                    "sandbox_state_notification_failed",
                    json!({
                        "error": "missing sandbox state params",
                    }),
                );
                return;
            }
            Err(err) => {
                eprintln!("sandbox update parse error: {err}");
                crate::event_log::log(
                    "sandbox_state_notification_failed",
                    json!({
                        "error": err.to_string(),
                    }),
                );
                return;
            }
        };
        let update_for_log = serde_json::to_value(&update)
            .unwrap_or_else(|err| json!({"serialize_error": err.to_string()}));

        match self
            .run_state(move |state| {
                let outcome = state
                    .worker
                    .update_sandbox_state(update, SANDBOX_UPDATE_TIMEOUT);
                if matches!(outcome, Ok(true))
                    && let Err(err) = state.response.clear_active_timeout_bundle()
                {
                    return Err(err);
                }
                outcome
            })
            .await
        {
            Ok(Ok(changed)) => {
                crate::event_log::log(
                    "sandbox_state_notification_applied",
                    json!({
                        "changed": changed,
                        "update": update_for_log.clone(),
                    }),
                );
            }
            Ok(Err(err)) => {
                eprintln!("sandbox update failed: {err}");
                crate::event_log::log(
                    "sandbox_state_notification_failed",
                    json!({
                        "error": err.to_string(),
                        "update": update_for_log.clone(),
                    }),
                );
            }
            Err(err) => {
                eprintln!("sandbox update failed: {err}");
                crate::event_log::log(
                    "sandbox_state_notification_failed",
                    json!({
                        "error": err.to_string(),
                        "update": update_for_log.clone(),
                    }),
                );
            }
        }
    }
}

fn should_reuse_timeout_bundle(input: &str) -> bool {
    if matches!(input.chars().next(), Some('\u{3}' | '\u{4}')) {
        return false;
    }
    input.is_empty() || input.chars().all(|ch| matches!(ch, '\r' | '\n'))
}

fn server_info() -> ServerInfo {
    ServerInfo::new(
        ServerCapabilities::builder()
            .enable_tools()
            .enable_experimental_with(sandbox_capabilities())
            .build(),
    )
    .with_protocol_version(ProtocolVersion::V_2025_06_18)
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
            fn new(backend: Backend, sandbox_plan: SandboxCliPlan) -> Result<Self, WorkerError> {
                Ok(Self {
                    shared: SharedServer::new(backend, sandbox_plan)?,
                    tool_router: Self::tool_router(),
                })
            }

            fn get_info(&self) -> ServerInfo {
                server_info()
            }

            fn logged_tool_router(&self) -> LoggedToolRouter<'_, Self> {
                LoggedToolRouter::new(&self.tool_router)
            }

            #[doc = include_str!($repl_doc_path)]
            #[tool(name = "repl")]
            async fn repl(&self, params: Parameters<ReplArgs>) -> Result<CallToolResult, McpError> {
                let ReplArgs { input, timeout_ms } = params.0;
                let timeout = resolve_timeout_ms(timeout_ms, "repl", true)?;
                self.shared.run_write_input(input, timeout).await
            }

            #[doc = include_str!("../docs/tool-descriptions/repl_reset_tool.md")]
            #[tool(name = "repl_reset")]
            async fn repl_reset(
                &self,
                _params: Parameters<ReplResetArgs>,
            ) -> Result<CallToolResult, McpError> {
                let timeout = parse_timeout(None, "repl_reset", false)?;
                let worker_timeout = apply_tool_call_margin(timeout);
                let result = self
                    .shared
                    .run_state(move |state| {
                        let result = state.worker.restart(worker_timeout);
                        state.response.finalize_worker_result(
                            result,
                            state.worker.pending_request(),
                            false,
                            0,
                        )
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

            async fn on_custom_request(
                &self,
                request: CustomRequest,
                _context: rmcp::service::RequestContext<RoleServer>,
            ) -> Result<CustomResult, McpError> {
                self.shared.on_custom_request(request).await
            }

            async fn on_custom_notification(
                &self,
                notification: CustomNotification,
                _context: rmcp::service::NotificationContext<RoleServer>,
            ) {
                self.shared.on_custom_notification(notification).await
            }
        }
    };
}

define_backend_tool_server!(RToolServer, "../docs/tool-descriptions/repl_tool_r.md");
define_backend_tool_server!(
    PythonToolServer,
    "../docs/tool-descriptions/repl_tool_python.md"
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
    experimental.insert(SANDBOX_STATE_CAPABILITY.to_string(), capability);
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
) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("starting mcp-repl server");
    crate::event_log::log(
        "server_run_begin",
        json!({
            "backend": format!("{backend:?}"),
        }),
    );
    match backend {
        Backend::R => {
            let service = RToolServer::new(backend, sandbox_plan)?;
            run_backend_server(service.clone(), service.shared.state()).await
        }
        Backend::Python => {
            let service = PythonToolServer::new(backend, sandbox_plan)?;
            run_backend_server(service.clone(), service.shared.state()).await
        }
    }
}
