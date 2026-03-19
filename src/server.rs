use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Content, CustomNotification, CustomRequest, CustomResult, ErrorCode,
    ErrorData as McpError, JsonObject, ProtocolVersion, ServerCapabilities, ServerInfo,
};
use rmcp::{RoleServer, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

mod response;
#[cfg(test)]
mod tests;
mod timeouts;

use self::response::{finalize_batch, worker_reply_to_contents};
use self::timeouts::{
    SANDBOX_UPDATE_TIMEOUT, apply_safety_margin, apply_tool_call_margin, parse_timeout,
};

use crate::backend::Backend;
use crate::claude::{ClaudeClearBinding, ClaudeClientContext};
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
    backend: Backend,
    worker: Arc<Mutex<WorkerManager>>,
    claude_client_context: Option<ClaudeClientContext>,
    claude_clear_binding: Arc<Mutex<Option<ClaudeClearBinding>>>,
    claude_prune_started: Arc<AtomicBool>,
}

impl SharedServer {
    fn new(backend: Backend, sandbox_plan: SandboxCliPlan) -> Result<Self, WorkerError> {
        Ok(Self {
            backend,
            worker: Arc::new(Mutex::new(WorkerManager::new(backend, sandbox_plan)?)),
            claude_client_context: crate::claude::detect_client_context(),
            claude_clear_binding: Arc::new(Mutex::new(None)),
            claude_prune_started: Arc::new(AtomicBool::new(false)),
        })
    }

    fn worker(&self) -> Arc<Mutex<WorkerManager>> {
        Arc::clone(&self.worker)
    }

    async fn run_worker<T, F>(&self, f: F) -> Result<T, McpError>
    where
        F: FnOnce(&mut WorkerManager) -> T + Send + 'static,
        T: Send + 'static,
    {
        let worker = self.worker.clone();
        let claude_client_context = self.claude_client_context.clone();
        let claude_clear_binding = self.claude_clear_binding.clone();
        let backend = self.backend;
        tokio::task::spawn_blocking(move || {
            let mut worker = worker.lock().unwrap();
            let awaiting_initial_sandbox_state_update =
                worker.awaiting_initial_sandbox_state_update();
            if let Some(claude_client_context) = claude_client_context.as_ref() {
                let mut claude_clear_binding = claude_clear_binding
                    .lock()
                    .expect("claude clear binding mutex poisoned");
                if let Some(binding) = claude_clear_binding.as_ref() {
                    binding.refresh(claude_client_context)?;
                } else {
                    *claude_clear_binding =
                        ClaudeClearBinding::maybe_register_late(backend, claude_client_context)?;
                }
                if !awaiting_initial_sandbox_state_update
                    && let Some(binding) = claude_clear_binding.as_ref()
                {
                    binding.sync(&mut worker)?;
                }
            }
            Ok::<T, WorkerError>(f(&mut worker))
        })
        .await
        .map_err(|err| McpError::internal_error(err.to_string(), None))
        .and_then(|result| result.map_err(|err| McpError::internal_error(err.to_string(), None)))
    }

    fn on_initialized(&self) {
        if self.claude_client_context.is_none() {
            return;
        }
        if self.claude_prune_started.swap(true, Ordering::Relaxed) {
            return;
        }

        tokio::task::spawn_blocking(|| {
            crate::event_log::log("claude_state_prune_begin", json!({}));
            match crate::claude::prune_stale_state() {
                Ok(()) => crate::event_log::log("claude_state_prune_end", json!({"status": "ok"})),
                Err(err) => {
                    eprintln!("claude state prune error: {err}");
                    crate::event_log::log(
                        "claude_state_prune_end",
                        json!({
                            "status": "error",
                            "error": err.to_string(),
                        }),
                    );
                }
            }
        });
    }

    async fn run_write_input(
        &self,
        input: String,
        timeout: Duration,
    ) -> Result<CallToolResult, McpError> {
        let worker_timeout = apply_tool_call_margin(timeout);
        let server_timeout = apply_safety_margin(timeout);
        let result = self
            .run_worker(move |worker| {
                worker.write_stdin(input, worker_timeout, server_timeout, None, false)
            })
            .await?;
        worker_result_to_call_tool_result(result)
    }

    async fn on_custom_request(&self, request: CustomRequest) -> Result<CustomResult, McpError> {
        if std::env::var_os("MCP_CONSOLE_DEBUG_MCP").is_some() {
            eprintln!("custom request: {}", request.method);
        }
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
            .run_worker(move |worker| worker.update_sandbox_state(update, SANDBOX_UPDATE_TIMEOUT))
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
            .run_worker(move |worker| worker.update_sandbox_state(update, SANDBOX_UPDATE_TIMEOUT))
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

fn server_info() -> ServerInfo {
    ServerInfo {
        protocol_version: ProtocolVersion::V_2025_06_18,
        capabilities: ServerCapabilities::builder()
            .enable_tools()
            .enable_experimental_with(sandbox_capabilities())
            .build(),
        ..ServerInfo::default()
    }
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
                    .run_worker(move |worker| worker.restart(worker_timeout))
                    .await?;
                worker_result_to_call_tool_result(result)
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

            async fn on_initialized(
                &self,
                _context: rmcp::service::NotificationContext<RoleServer>,
            ) {
                self.shared.on_initialized();
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

fn worker_result_to_call_tool_result(
    result: Result<crate::worker_protocol::WorkerReply, WorkerError>,
) -> Result<CallToolResult, McpError> {
    let mut contents = Vec::new();
    let mut is_error = false;
    match result {
        Ok(reply) => {
            let (mut reply_contents, reply_error) = worker_reply_to_contents(reply);
            is_error |= reply_error;
            contents.append(&mut reply_contents);
            Ok(finalize_batch(contents, is_error))
        }
        Err(err) => {
            eprintln!("worker write stdin error: {err}");
            contents.push(Content::text(format!("worker error: {err}")));
            is_error = true;
            Ok(finalize_batch(contents, is_error))
        }
    }
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
    shutdown_worker: Arc<Mutex<WorkerManager>>,
) -> Result<(), Box<dyn std::error::Error>>
where
    S: ServerHandler + Send + Sync + Clone + 'static,
{
    let warm_worker = shutdown_worker.clone();
    thread::spawn(move || {
        crate::event_log::log("worker_warm_start_begin", json!({}));
        let mut worker = warm_worker.lock().unwrap();
        if let Err(err) = worker.warm_start() {
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
        let mut worker = shutdown_worker.lock().unwrap();
        worker.shutdown();
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
            run_backend_server(service.clone(), service.shared.worker()).await
        }
        Backend::Python => {
            let service = PythonToolServer::new(backend, sandbox_plan)?;
            run_backend_server(service.clone(), service.shared.worker()).await
        }
    }
}
