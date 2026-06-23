use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Content, ErrorData as McpError, JsonObject, Meta, ProtocolVersion,
    ServerCapabilities, ServerInfo,
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

use crate::backend::{Backend, WorkerLaunch};
use crate::oversized_output::OversizedOutputMode;
use crate::sandbox::{SANDBOX_STATE_META_CAPABILITY, SandboxStateUpdate};
use crate::sandbox_cli::{
    MISSING_INHERITED_SANDBOX_STATE_MESSAGE, SandboxCliPlan, sandbox_plan_requests_inherited_state,
};
use crate::worker_process::{
    WorkerError, WorkerManager, WriteStdinControlAction, WriteStdinOptions,
    is_prechecked_follow_up_requires_meta, split_write_stdin_control_prefix,
};

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
    python_requirements_manifest: crate::python_prepare::PythonRequirementsManifest,
}

impl SharedServer {
    fn new(
        worker_launch: WorkerLaunch,
        sandbox_plan: SandboxCliPlan,
        oversized_output: OversizedOutputMode,
    ) -> Result<Self, WorkerError> {
        let accepts_sandbox_state_meta = sandbox_plan_requests_inherited_state(&sandbox_plan);
        Ok(Self {
            accepts_sandbox_state_meta,
            state: Arc::new(Mutex::new(ServerState {
                worker: WorkerManager::new_with_launch(
                    worker_launch,
                    sandbox_plan,
                    oversized_output,
                )?,
                response: ResponseState::new()?,
                oversized_output,
                python_requirements_manifest:
                    crate::python_prepare::PythonRequirementsManifest::default(),
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

    fn optional_sandbox_state_update_for_tool_call_meta(
        accepts_sandbox_state_meta: bool,
        meta: &Meta,
    ) -> Result<Option<SandboxStateUpdate>, WorkerError> {
        if !accepts_sandbox_state_meta {
            return Ok(None);
        }

        let Some(raw_meta) = meta.get(SANDBOX_STATE_META_CAPABILITY) else {
            return Ok(None);
        };

        match crate::sandbox::sandbox_state_update_from_codex_meta(raw_meta) {
            Ok(update) => Ok(Some(update)),
            Err(_) => Ok(None),
        }
    }

    fn apply_tool_call_sandbox_state(
        state: &mut ServerState,
        update: Option<SandboxStateUpdate>,
    ) -> Result<bool, WorkerError> {
        let Some(update) = update else {
            return Ok(false);
        };

        state
            .worker
            .update_sandbox_state(update, SANDBOX_UPDATE_TIMEOUT)
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
            let mut raw_input = input;
            let use_inline_pager_materialization =
                matches!(state.oversized_output, OversizedOutputMode::Pager);
            state.worker.refresh_timeout_marker();
            let parse_tool_call_sandbox_state = || {
                SharedServer::sandbox_state_update_for_tool_call_meta(
                    accepts_sandbox_state_meta,
                    &meta,
                )
            };
            let parse_optional_tool_call_sandbox_state = || {
                SharedServer::optional_sandbox_state_update_for_tool_call_meta(
                    accepts_sandbox_state_meta,
                    &meta,
                )
            };
            let mut suppress_session_end_reset = false;
            let (sandbox_state_result, local_error_is_mcp_error) = if raw_input.is_empty() {
                // Empty-input polls only skip metadata when they are truly
                // draining existing output. In pager mode, empty input can
                // also be a pure local navigation command and should ignore
                // inherit metadata until a later worker interaction.
                let needs_post_poll_reset = state.worker.empty_input_may_auto_reset_after_poll();
                if state.worker.empty_input_uses_local_pager_state() {
                    (
                        match parse_tool_call_sandbox_state() {
                            Ok(update) => Ok(update),
                            Err(_) => {
                                suppress_session_end_reset = true;
                                Ok(None)
                            }
                        },
                        false,
                    )
                } else {
                    match state.worker.empty_input_requires_spawn() {
                        Ok(true) => (
                            parse_tool_call_sandbox_state().and_then(|update| {
                                let respawned =
                                    SharedServer::apply_tool_call_sandbox_state(state, update)?;
                                if respawned {
                                    state.response.retire_disclosed_timeout_bundle();
                                }
                                Ok(None)
                            }),
                            true,
                        ),
                        Ok(false) if needs_post_poll_reset => (
                            match parse_tool_call_sandbox_state() {
                                Ok(update) => Ok(update),
                                Err(_) => {
                                    suppress_session_end_reset = true;
                                    Ok(None)
                                }
                            },
                            false,
                        ),
                        Ok(false) => (parse_optional_tool_call_sandbox_state(), false),
                        Err(err) => (Err(err), true),
                    }
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
                let restart_control = matches!(
                    split_write_stdin_control_prefix(&raw_input),
                    Some((WriteStdinControlAction::Restart, _))
                );
                let bare_interrupt = matches!(
                    split_write_stdin_control_prefix(&raw_input),
                    Some((WriteStdinControlAction::Interrupt, remaining)) if remaining.is_empty()
                );
                let needs_initial_state =
                    restart_control && state.worker.missing_inherited_state_without_worker();
                if needs_initial_state {
                    (parse_tool_call_sandbox_state(), true)
                } else if state.worker.pending_request() && bare_interrupt {
                    (
                        match parse_tool_call_sandbox_state() {
                            Ok(update) => Ok(update),
                            Err(_) => {
                                suppress_session_end_reset = true;
                                Ok(None)
                            }
                        },
                        false,
                    )
                } else if state.worker.pending_request() {
                    let local_pager_follow_up = input_uses_local_pager_state(state, &raw_input);
                    if local_pager_follow_up
                        && split_write_stdin_control_prefix(&raw_input).is_none()
                    {
                        (Ok(None), false)
                    } else {
                        (
                            match parse_tool_call_sandbox_state().and_then(|update| {
                                SharedServer::apply_tool_call_sandbox_state(state, update)
                            }) {
                                Ok(respawned) => {
                                    if respawned {
                                        state.response.retire_disclosed_timeout_bundle();
                                        raw_input = normalize_input_after_sandbox_respawn(
                                            &raw_input,
                                            local_pager_follow_up,
                                        );
                                    }
                                    Ok(None)
                                }
                                Err(err) => Err(err),
                            },
                            true,
                        )
                    }
                } else {
                    match state
                        .worker
                        .nonexecuting_follow_up_uses_existing_state(&raw_input)
                    {
                        Ok(true) => (
                            match parse_tool_call_sandbox_state() {
                                Ok(update) => Ok(update),
                                Err(_) => {
                                    suppress_session_end_reset = true;
                                    Ok(None)
                                }
                            },
                            false,
                        ),
                        Ok(false) => {
                            let local_pager_follow_up =
                                input_uses_local_pager_state(state, &raw_input);
                            (
                                match parse_tool_call_sandbox_state().and_then(|update| {
                                    SharedServer::apply_tool_call_sandbox_state(state, update)
                                }) {
                                    Ok(respawned) => {
                                        if respawned {
                                            state.response.retire_disclosed_timeout_bundle();
                                            raw_input = normalize_input_after_sandbox_respawn(
                                                &raw_input,
                                                local_pager_follow_up,
                                            );
                                        }
                                        Ok(None)
                                    }
                                    Err(err) => Err(err),
                                },
                                true,
                            )
                        }
                        Err(err) => (Err(err), true),
                    }
                }
            };
            let deferred_sandbox_state_update = match sandbox_state_result {
                Ok(update) => update,
                Err(err) => {
                    let mut result = state
                        .response
                        .finalize_local_error(err, local_error_is_mcp_error);
                    strip_text_stream_meta(&mut result);
                    return result;
                }
            };
            let prior_disclosed_timeout_bundle_id = state.response.disclosed_timeout_bundle_id();
            let mut timeout_bundle_reuse = timeout_bundle_reuse_for_input(&raw_input);
            let mut write_options = WriteStdinOptions {
                pending_state_prechecked: true,
                deferred_sandbox_state_update,
                suppress_session_end_reset,
                ..WriteStdinOptions::default()
            };
            let mut retried_after_meta_refresh = false;
            let result = loop {
                let result = state.worker.write_stdin(
                    raw_input.clone(),
                    worker_timeout,
                    server_timeout,
                    write_options.clone(),
                );
                match result {
                    Err(err)
                        if !retried_after_meta_refresh
                            && is_prechecked_follow_up_requires_meta(&err) =>
                    {
                        let local_pager_follow_up = input_uses_local_pager_state(state, &raw_input);
                        match parse_tool_call_sandbox_state().and_then(|update| {
                            SharedServer::apply_tool_call_sandbox_state(state, update)
                        }) {
                            Ok(respawned) => {
                                if respawned {
                                    state.response.retire_disclosed_timeout_bundle();
                                    raw_input = normalize_input_after_sandbox_respawn(
                                        &raw_input,
                                        local_pager_follow_up,
                                    );
                                    timeout_bundle_reuse =
                                        timeout_bundle_reuse_for_input(&raw_input);
                                }
                                retried_after_meta_refresh = true;
                                write_options.pending_state_prechecked = false;
                                write_options.deferred_sandbox_state_update = None;
                                write_options.suppress_session_end_reset = false;
                                continue;
                            }
                            Err(err) => break Err(err),
                        }
                    }
                    other => break other,
                }
            };
            let pending_request_after = state.worker.pending_request();
            let detached_prefix_item_count = state.worker.detached_prefix_item_count();
            let respawned_during_write = state.worker.respawned_during_last_write();
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
            if respawned_during_write {
                state
                    .response
                    .retire_timeout_bundle_if_matches(prior_disclosed_timeout_bundle_id);
            }
            strip_text_stream_meta(&mut result);
            result
        })
        .await
    }

    async fn run_reset(&self, meta: Meta) -> Result<CallToolResult, McpError> {
        let timeout = parse_timeout(None, "repl_reset", false)?;
        let worker_timeout = apply_tool_call_margin(timeout);
        let sandbox_state_update = self.sandbox_state_update_for_tool_call(&meta);
        let result = self
            .run_state(move |state| {
                let sandbox_state_result = match &sandbox_state_update {
                    Ok(update) => {
                        SharedServer::stage_tool_call_sandbox_state_for_reset(state, update.clone())
                    }
                    Err(WorkerError::Sandbox(message)) => {
                        Err(WorkerError::Sandbox(message.clone()))
                    }
                    Err(err) => Err(WorkerError::Sandbox(err.to_string())),
                };
                if let Err(err) = sandbox_state_result {
                    let mut result = state.response.finalize_local_error(err, true);
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

    async fn run_prepare_python(
        &self,
        args: crate::python_prepare::ReplPrepareArgs,
        meta: Meta,
    ) -> Result<CallToolResult, McpError> {
        let request = crate::python_prepare::validate_prepare_args(args)
            .map_err(|message| McpError::invalid_params(message, None))?;
        let accepts_sandbox_state_meta = self.accepts_sandbox_state_meta();
        self.run_state(move |state| match request {
            crate::python_prepare::ValidatedPrepareRequest::Requirements(operation) => {
                let sandbox_state_update = || {
                    SharedServer::sandbox_state_update_for_tool_call_meta(
                        accepts_sandbox_state_meta,
                        &meta,
                    )
                };
                Self::run_prepare_python_requirements(state, operation, &sandbox_state_update)
            }
            crate::python_prepare::ValidatedPrepareRequest::PythonExecutable(executable) => {
                let sandbox_state_update = || {
                    SharedServer::sandbox_state_update_for_tool_call_meta(
                        accepts_sandbox_state_meta,
                        &meta,
                    )
                };
                Self::run_prepare_python_executable(state, executable, &sandbox_state_update)
            }
        })
        .await
    }

    fn run_prepare_python_requirements(
        state: &mut ServerState,
        operation: crate::python_prepare::PrepareRequirementsOperation,
        sandbox_state_update: &dyn Fn() -> Result<Option<SandboxStateUpdate>, WorkerError>,
    ) -> CallToolResult {
        let current_manifest = state.python_requirements_manifest.clone();
        let candidate_manifest =
            crate::python_prepare::apply_requirements_operation(&current_manifest, &operation);
        let target = match crate::python_prepare::resolve_requirements_manifest(&candidate_manifest)
        {
            Ok(target) => target,
            Err(err) => {
                return Self::prepare_python_error_reply(
                    format!("repl_prepare failed: {err}"),
                    "session unchanged",
                    "no user state discarded",
                    &current_manifest,
                );
            }
        };

        let active_matches = state.worker.python_executable_matches(&target.executable);
        let restart_required = match operation.restart {
            crate::python_prepare::PrepareRestartPolicy::IfNeeded => !active_matches,
            crate::python_prepare::PrepareRestartPolicy::Yes => true,
            crate::python_prepare::PrepareRestartPolicy::No => !active_matches,
        };
        let had_pending_work = state.worker.pending_request();
        let user_state_may_exist = state.worker.user_state_may_exist() || had_pending_work;

        if matches!(
            operation.restart,
            crate::python_prepare::PrepareRestartPolicy::No
        ) && restart_required
            && user_state_may_exist
        {
            return Self::prepare_python_error_reply(
                "repl_prepare failed: satisfying the requirements would require restarting the current Python session",
                "session unchanged",
                "no user state discarded",
                &current_manifest,
            );
        }

        if !restart_required {
            state.python_requirements_manifest = candidate_manifest;
            return Self::prepare_python_success_reply(
                "session unchanged",
                "no user state discarded",
                &state.python_requirements_manifest,
            );
        }

        let discarded = prepare_discard_status(had_pending_work, user_state_may_exist);
        if let Err(err) = Self::stage_prepare_sandbox_state(state, sandbox_state_update()) {
            return Self::prepare_python_local_error_reply(state, err);
        }
        match state
            .worker
            .replace_worker_launch(WorkerLaunch::PythonExecutable {
                executable: target.executable,
                module_search_paths: target.module_search_paths,
            }) {
            Ok(()) => {
                Self::clear_prepare_timeout_state_if_discarded(state, had_pending_work);
                state.python_requirements_manifest = candidate_manifest;
                Self::prepare_python_success_reply(
                    "session restarted",
                    discarded,
                    &state.python_requirements_manifest,
                )
            }
            Err(err) => {
                if had_pending_work && !state.worker.pending_request() {
                    Self::clear_prepare_timeout_state_if_discarded(state, had_pending_work);
                }
                Self::prepare_python_local_error_reply(state, err)
            }
        }
    }

    fn run_prepare_python_executable(
        state: &mut ServerState,
        executable: std::path::PathBuf,
        sandbox_state_update: &dyn Fn() -> Result<Option<SandboxStateUpdate>, WorkerError>,
    ) -> CallToolResult {
        let target = match crate::python_prepare::resolve_prepare_target(
            &crate::python_prepare::ValidatedPrepareRequest::PythonExecutable(executable),
        ) {
            Ok(target) => target,
            Err(err) => {
                return Self::prepare_python_error_reply(
                    format!("repl_prepare failed: {err}"),
                    "session unchanged",
                    "no user state discarded",
                    &state.python_requirements_manifest,
                );
            }
        };
        let active_matches = state.worker.python_executable_matches(&target.executable);
        if active_matches {
            return Self::prepare_python_success_reply(
                "session unchanged",
                "no user state discarded",
                &state.python_requirements_manifest,
            );
        }

        let had_pending_work = state.worker.pending_request();
        let user_state_may_exist = state.worker.user_state_may_exist() || had_pending_work;
        let discarded = prepare_discard_status(had_pending_work, user_state_may_exist);
        if let Err(err) = Self::stage_prepare_sandbox_state(state, sandbox_state_update()) {
            return Self::prepare_python_local_error_reply(state, err);
        }
        match state
            .worker
            .replace_worker_launch(WorkerLaunch::PythonExecutable {
                executable: target.executable,
                module_search_paths: target.module_search_paths,
            }) {
            Ok(()) => {
                Self::clear_prepare_timeout_state_if_discarded(state, had_pending_work);
                Self::prepare_python_success_reply(
                    "session restarted",
                    discarded,
                    &state.python_requirements_manifest,
                )
            }
            Err(err) => {
                if had_pending_work && !state.worker.pending_request() {
                    Self::clear_prepare_timeout_state_if_discarded(state, had_pending_work);
                }
                Self::prepare_python_local_error_reply(state, err)
            }
        }
    }

    fn stage_prepare_sandbox_state(
        state: &mut ServerState,
        update: Result<Option<SandboxStateUpdate>, WorkerError>,
    ) -> Result<(), WorkerError> {
        SharedServer::stage_tool_call_sandbox_state_for_reset(state, update?)
    }

    fn clear_prepare_timeout_state_if_discarded(state: &mut ServerState, had_pending_work: bool) {
        if had_pending_work && let Err(err) = state.response.clear_active_timeout_bundle() {
            eprintln!("dropping discarded timeout bundle after repl_prepare replacement: {err}");
        }
    }

    fn prepare_python_local_error_reply(
        state: &mut ServerState,
        err: WorkerError,
    ) -> CallToolResult {
        let mut result = state.response.finalize_local_error(err, true);
        strip_text_stream_meta(&mut result);
        result
    }

    fn prepare_python_success_reply(
        session_status: &str,
        discard_status: &str,
        manifest: &crate::python_prepare::PythonRequirementsManifest,
    ) -> CallToolResult {
        CallToolResult::success(vec![Content::text(format!(
            "repl_prepare: {session_status}; {discard_status}\n{}\n",
            crate::python_prepare::format_requirements_manifest(manifest)
        ))])
    }

    fn prepare_python_error_reply(
        message: impl Into<String>,
        session_status: &str,
        discard_status: &str,
        manifest: &crate::python_prepare::PythonRequirementsManifest,
    ) -> CallToolResult {
        CallToolResult::error(vec![Content::text(format!(
            "{}\nrepl_prepare: {session_status}; {discard_status}\n{}\n",
            message.into(),
            crate::python_prepare::format_requirements_manifest(manifest)
        ))])
    }
}

fn prepare_discard_status(had_pending_work: bool, user_state_may_exist: bool) -> &'static str {
    if had_pending_work {
        "pending work discarded"
    } else if user_state_may_exist {
        "user state discarded"
    } else {
        "no user state discarded"
    }
}

fn input_uses_local_pager_state(state: &ServerState, input: &str) -> bool {
    if let Some((_control, remaining)) = split_write_stdin_control_prefix(input) {
        state
            .worker
            .local_pager_follow_up_uses_existing_state(remaining)
    } else {
        state
            .worker
            .local_pager_follow_up_uses_existing_state(input)
    }
}

fn normalize_input_after_sandbox_respawn(input: &str, local_pager_follow_up: bool) -> String {
    if let Some((control, remaining)) = split_write_stdin_control_prefix(input) {
        if matches!(control, WriteStdinControlAction::Restart) && remaining.is_empty() {
            input.to_string()
        } else if local_pager_follow_up {
            String::new()
        } else {
            remaining.to_string()
        }
    } else if local_pager_follow_up {
        String::new()
    } else {
        input.to_string()
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

macro_rules! define_repl_only_tool_server {
    ($server_ty:ident, $repl_doc_path:literal) => {
        #[derive(Clone)]
        struct $server_ty {
            shared: SharedServer,
            tool_router: ToolRouter<Self>,
        }

        #[tool_router]
        impl $server_ty {
            fn new(
                worker_launch: WorkerLaunch,
                sandbox_plan: SandboxCliPlan,
                oversized_output: OversizedOutputMode,
            ) -> Result<Self, WorkerError> {
                Ok(Self {
                    shared: SharedServer::new(worker_launch, sandbox_plan, oversized_output)?,
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
        }

        #[tool_handler(router = self.logged_tool_router())]
        impl ServerHandler for $server_ty {
            fn get_info(&self) -> ServerInfo {
                $server_ty::get_info(self)
            }
        }
    };
}

macro_rules! define_repl_reset_tool_server {
    ($server_ty:ident, $repl_doc_path:literal) => {
        #[derive(Clone)]
        struct $server_ty {
            shared: SharedServer,
            tool_router: ToolRouter<Self>,
        }

        #[tool_router]
        impl $server_ty {
            fn new(
                worker_launch: WorkerLaunch,
                sandbox_plan: SandboxCliPlan,
                oversized_output: OversizedOutputMode,
            ) -> Result<Self, WorkerError> {
                Ok(Self {
                    shared: SharedServer::new(worker_launch, sandbox_plan, oversized_output)?,
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
                self.shared.run_reset(meta).await
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

macro_rules! define_python_prepare_tool_server {
    ($server_ty:ident, $repl_doc_path:literal) => {
        #[derive(Clone)]
        struct $server_ty {
            shared: SharedServer,
            tool_router: ToolRouter<Self>,
        }

        #[tool_router]
        impl $server_ty {
            fn new(
                worker_launch: WorkerLaunch,
                sandbox_plan: SandboxCliPlan,
                oversized_output: OversizedOutputMode,
            ) -> Result<Self, WorkerError> {
                Ok(Self {
                    shared: SharedServer::new(worker_launch, sandbox_plan, oversized_output)?,
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

            #[doc = include_str!("../docs/tool-descriptions/repl_prepare_python.md")]
            #[tool(
                name = "repl_prepare",
                annotations(
                    read_only_hint = false,
                    destructive_hint = false,
                    open_world_hint = false
                )
            )]
            async fn repl_prepare(
                &self,
                meta: Meta,
                params: Parameters<crate::python_prepare::ReplPrepareArgs>,
            ) -> Result<CallToolResult, McpError> {
                self.shared.run_prepare_python(params.0, meta).await
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

define_repl_reset_tool_server!(RFilesToolServer, "../docs/tool-descriptions/repl_tool_r.md");
define_repl_reset_tool_server!(
    RPagerToolServer,
    "../docs/tool-descriptions/repl_tool_r_pager.md"
);
define_repl_only_tool_server!(
    PythonFilesToolServer,
    "../docs/tool-descriptions/repl_tool_python.md"
);
define_repl_only_tool_server!(
    PythonPagerToolServer,
    "../docs/tool-descriptions/repl_tool_python_pager.md"
);
define_python_prepare_tool_server!(
    PythonPrepareFilesToolServer,
    "../docs/tool-descriptions/repl_tool_python.md"
);
define_python_prepare_tool_server!(
    PythonPreparePagerToolServer,
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
    worker_launch: WorkerLaunch,
    sandbox_plan: SandboxCliPlan,
    oversized_output: OversizedOutputMode,
) -> Result<(), Box<dyn std::error::Error>> {
    let backend = worker_launch.builtin_backend().unwrap_or(Backend::R);
    crate::event_log::log(
        "server_run_begin",
        json!({
            "backend": worker_launch.label(),
        }),
    );
    match backend {
        Backend::R => match oversized_output {
            OversizedOutputMode::Files => {
                let service = RFilesToolServer::new(worker_launch, sandbox_plan, oversized_output)?;
                run_backend_server(service.clone(), service.shared.state()).await
            }
            OversizedOutputMode::Pager => {
                let service = RPagerToolServer::new(worker_launch, sandbox_plan, oversized_output)?;
                run_backend_server(service.clone(), service.shared.state()).await
            }
        },
        Backend::Python => match oversized_output {
            OversizedOutputMode::Files => {
                if crate::python_prepare::uv_available() {
                    let service = PythonPrepareFilesToolServer::new(
                        worker_launch,
                        sandbox_plan,
                        oversized_output,
                    )?;
                    run_backend_server(service.clone(), service.shared.state()).await
                } else {
                    let service =
                        PythonFilesToolServer::new(worker_launch, sandbox_plan, oversized_output)?;
                    run_backend_server(service.clone(), service.shared.state()).await
                }
            }
            OversizedOutputMode::Pager => {
                if crate::python_prepare::uv_available() {
                    let service = PythonPreparePagerToolServer::new(
                        worker_launch,
                        sandbox_plan,
                        oversized_output,
                    )?;
                    run_backend_server(service.clone(), service.shared.state()).await
                } else {
                    let service =
                        PythonPagerToolServer::new(worker_launch, sandbox_plan, oversized_output)?;
                    run_backend_server(service.clone(), service.shared.state()).await
                }
            }
        },
    }
}
