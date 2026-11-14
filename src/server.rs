mod arguments;
#[cfg(unix)]
mod control;
#[cfg(windows)]
mod input_windows;
mod presentation;
mod sandbox;
pub(crate) use presentation::{materialized_tool, parameter_schema};
pub(crate) use sandbox::Configuration as SandboxConfiguration;
mod sessions;
mod startup;
use std::error::Error;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use arguments::{SendArguments, SendControl};
use rmcp::{
    ErrorData, RoleServer, ServerHandler, ServiceExt,
    handler::server::{
        common::Extension, router::tool::ToolRouter, tool::ToolCallContext, wrapper::Parameters,
    },
    model::{
        CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
        ErrorCode, Implementation, ListToolsResult, MetaObject, PaginatedRequestParams,
        ServerCapabilities, ServerConfig,
    },
    service::RequestContext,
    tool, tool_handler, tool_router,
};
use tokio::io::{AsyncRead, ReadBuf};
use tokio::sync::oneshot;

use crate::worker_client::WORKER_SHUTDOWN_GRACE;

use crate::cell::{LANGUAGES_ENV, Languages};
use futures_util::{StreamExt, stream::FuturesUnordered};

#[derive(Clone)]
struct PollAll(bool);

#[derive(Clone)]
struct ConsoleServer {
    startup: startup::Startup,
    sessions: sessions::Sessions,
    deliveries: crate::server_transport::ResponseDeliveries,
    languages: Languages,
    language_setting: &'static str,
    tool_router: ToolRouter<Self>,
    instructions: String,
    tool_settings: Arc<crate::settings::Tool>,
    inheritance: Arc<sandbox::Inheritance>,
}

impl ConsoleServer {
    #[allow(clippy::too_many_arguments)]
    fn new(
        input_closed: InputClosed,
        worker: Option<PathBuf>,
        relay: Option<PathBuf>,
        sandbox: SandboxConfiguration,
        python: Option<crate::settings::PythonChoice>,
        r: crate::settings::R,
        startup: Option<crate::settings::startup::Startup>,
        visibility: Option<Languages>,
        tool_settings: crate::settings::Tool,
        session_settings: crate::settings::Sessions,
    ) -> Result<Self, String> {
        let recording_directory = std::env::current_dir();
        let (languages, language_setting) = match visibility {
            Some(languages) => (languages, "languages"),
            None => (Languages::from_environment()?, LANGUAGES_ENV),
        };
        if worker.is_none() && relay.is_some() {
            return Err("a custom relay requires a custom worker".into());
        }
        // Presentation has no dependency on the client or its discovered capabilities.
        let (tool_router, instructions) = Self::configured_presentation(
            languages,
            worker.is_none(),
            python.as_ref(),
            &sandbox.policy,
            sandbox.no_sandbox,
            sandbox
                .profile
                .as_ref()
                .is_some_and(crate::settings::ProfileSelection::inherits_codex),
            &tool_settings,
        )?;
        let session_pool = crate::worker_client::SessionPool::new(
            session_settings.max.map(std::num::NonZeroUsize::get),
        );
        let runtime = Arc::new(startup::Runtime {
            worker: crate::worker_client::Client::pending(session_pool.default_slot()),
            transcript: crate::transcript::Transcript::pending(
                recording_directory
                    .as_ref()
                    .cloned()
                    .map_err(|error| std::io::Error::new(error.kind(), error.to_string())),
            ),
        });
        let prelaunch = worker.is_none();
        let uses_client_profile = sandbox
            .profile
            .as_ref()
            .is_some_and(crate::settings::ProfileSelection::inherits_codex);
        let inheritance = Arc::new(sandbox::Inheritance::new(sandbox.profile.clone()));
        let startup_inheritance = inheritance.clone();
        let startup = startup::Startup::new(
            input_closed,
            runtime,
            prelaunch,
            session_pool,
            move |started, diagnostics| {
                let sandbox = if uses_client_profile {
                    sandbox
                        .clone()
                        .configure(startup_inheritance.policy(), python.as_ref())?
                } else {
                    sandbox.clone()
                };
                let configuration = if let Some(program) = worker.clone() {
                    crate::worker_client::ClientConfiguration::new(
                        program,
                        relay.clone(),
                        sandbox.no_sandbox,
                        sandbox.policy,
                    )
                    .with_resolver_settings(sandbox.resolver)?
                } else {
                    crate::worker_client::ClientConfiguration::builtin(
                        sandbox.no_sandbox,
                        sandbox.policy,
                        python.clone(),
                        r.clone(),
                        sandbox.resolver,
                        diagnostics,
                        started,
                    )?
                };
                let configuration = configuration.with_startup(startup.clone());
                let transcript = crate::transcript::Transcript::configured(
                    recording_directory
                        .as_ref()
                        .cloned()
                        .map_err(|error| std::io::Error::new(error.kind(), error.to_string())),
                    configuration.dynamic_resolution(),
                    configuration.python_preparation(),
                    !configuration.python_only(),
                    configuration.startup_declaration(),
                );
                Ok(startup::PreparedRuntime {
                    configuration,
                    transcript,
                })
            },
        );
        if !uses_client_profile {
            startup.start()?;
        }
        Ok(Self {
            sessions: sessions::Sessions::new(startup.clone()),
            startup,
            deliveries: crate::server_transport::ResponseDeliveries::default(),
            languages,
            language_setting,
            tool_router,
            instructions,
            tool_settings: Arc::new(tool_settings),
            inheritance,
        })
    }
}

#[tool_router]
impl ConsoleServer {
    // The static BaseMetadata title keeps aggregated client displays (such as
    // shinychat multi-call groups) stable; per-call result titles still vary.
    // Presentation names the language when the interface exposes exactly one.
    #[tool(title = "Run code")]
    async fn send(
        &self,
        Extension(poll_all): Extension<PollAll>,
        Extension(call): Extension<crate::transcript::Call>,
        Extension(delivery): Extension<crate::server_transport::ResponseDeliveryCall>,
        Extension(started): Extension<Instant>,
        Parameters(SendArguments {
            r,
            python,
            sql,
            note,
            session,
            control,
            requirements,
            stdin,
            timeout_ms,
        }): Parameters<SendArguments>,
    ) -> Result<CallToolResult, String> {
        // The request recorder already owns the note. Standalone annotations
        // neither claim a worker operation nor consume its pending output.
        if note.is_some()
            && r.is_none()
            && python.is_none()
            && sql.is_none()
            && control.is_none()
            && requirements.is_none()
            && stdin.is_none()
        {
            return Ok(CallToolResult::success(vec![ContentBlock::text("[done]")]));
        }
        let cell = match (r, python, sql) {
            (Some(source), None, None) => Some(crate::cell::Cell {
                language: crate::cell::Language::R,
                source,
            }),
            (None, Some(source), None) => Some(crate::cell::Cell {
                language: crate::cell::Language::Python,
                source,
            }),
            (None, None, Some(source)) => Some(crate::cell::Cell {
                language: crate::cell::Language::Sql,
                source,
            }),
            (None, None, None) => None,
            _ => {
                return Err(format!(
                    "only one of {} may be supplied",
                    self.languages.cell_fields()
                ));
            }
        };
        if let Some(requirements) = &requirements {
            use crate::worker_client::RequirementsAction::{Get, Reset};
            if matches!(requirements.action, Get | Reset)
                && (requirements.r.is_some()
                    || requirements.python.is_some()
                    || requirements.duckdb.is_some()
                    || requirements.python_version.is_some()
                    || requirements.exclude_newer.is_some())
            {
                return Err(
                    "requirements.action=get/reset takes no package lists or constraint fields"
                        .into(),
                );
            }
            if requirements.action == Get
                && (cell.is_some() || stdin.is_some() || control.is_some())
            {
                return Err(
                    "requirements.action=get cannot be combined with code, stdin, or control"
                        .into(),
                );
            }
        }
        let inspect = requirements.as_ref().is_some_and(|requirements| {
            requirements.action == crate::worker_client::RequirementsAction::Get
        });
        let requirements = requirements.map(|requirements| requirements.into_worker(call.id()));
        if poll_all.0 {
            return self
                .poll_sessions(timeout_ms, started, &call, &delivery)
                .await;
        }
        let transcript = self.startup.runtime().transcript.for_session(&session);
        let request = crate::worker_client::SendRequest {
            cell,
            stdin,
            requirements,
            control: control.map(|control| match control {
                SendControl::Interrupt => crate::worker_client::SendControl::Interrupt,
                SendControl::Restart => crate::worker_client::SendControl::Restart,
            }),
            deadline: started
                .checked_add(Duration::from_millis(timeout_ms))
                .unwrap_or(started),
            transcript,
            call_id: call.id(),
        };
        let (startup, _activation) = match request
            .validate(true)
            .and_then(|()| self.sessions.select(&session, request.cell.is_some()))
            .and_then(|startup| {
                let worker = startup.runtime().worker.clone();
                let activating = request.cell.is_some()
                    || request
                        .stdin
                        .as_ref()
                        .is_some_and(|input| !input.is_empty())
                    || request.requirements.as_ref().is_some_and(|requirements| {
                        requirements.action != crate::worker_client::RequirementsAction::Get
                    })
                    || (matches!(
                        request.control,
                        Some(crate::worker_client::SendControl::Restart)
                    ) && !worker.is_configured());
                let activation = activating.then(|| worker.activate_session()).transpose()?;
                startup.start()?;
                Ok((startup, activation))
            }) {
            Ok(startup) => startup,
            Err(error) => {
                return Ok(response_to_tool_result(
                    crate::worker_client::Response::tool_error(error),
                    &call,
                    &request.transcript,
                    &self.deliveries,
                    &delivery,
                )
                .await);
            }
        };
        let runtime = startup.runtime();
        if inspect {
            match tokio::time::timeout(
                Duration::from_millis(timeout_ms).saturating_sub(started.elapsed()),
                runtime.worker.ready(),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    return Ok(response_to_tool_result(
                        runtime.worker.startup_failure_response(error),
                        &call,
                        &runtime.transcript,
                        &self.deliveries,
                        &delivery,
                    )
                    .await);
                }
                Err(_) => {
                    let response = runtime.worker.starting_response();
                    return Ok(response_to_tool_result(
                        response,
                        &call,
                        &runtime.transcript,
                        &self.deliveries,
                        &delivery,
                    )
                    .await);
                }
            }
            if let Some(response) = runtime.worker.take_prelaunch_failure()? {
                return Ok(response_to_tool_result(
                    response,
                    &call,
                    &runtime.transcript,
                    &self.deliveries,
                    &delivery,
                )
                .await);
            }
            let snapshot = runtime.worker.inspect_requirements();
            let json = serde_json::to_string_pretty(&snapshot).expect("requirements JSON");
            let text = if json.len() <= 8 * 1024 {
                json
            } else {
                "The complete requirements declaration is in structuredContent.requirements; supply that object with action=\"set\". The manifest exceeds the text preview limit and is not reproduced here.".into()
            };
            let mut result = CallToolResult::success(vec![rmcp::model::ContentBlock::text(text)]);
            result.structured_content = Some(snapshot);
            return Ok(result);
        }
        let response = async {
            let retry = matches!(
                request.control,
                Some(crate::worker_client::SendControl::Restart)
            ) || (session != "default" && request.cell.is_some());
            let initial_restart = retry && startup.retry_failed()?;
            runtime.worker.send(request, initial_restart).await
        }
        .await
        .unwrap_or_else(crate::worker_client::Response::tool_error);
        Ok(response_to_tool_result(
            response,
            &call,
            &runtime.transcript,
            &self.deliveries,
            &delivery,
        )
        .await)
    }
}

impl ConsoleServer {
    async fn poll_sessions(
        &self,
        timeout_ms: u64,
        started: Instant,
        call: &crate::transcript::Call,
        delivery: &crate::server_transport::ResponseDeliveryCall,
    ) -> Result<CallToolResult, String> {
        let deadline = started
            .checked_add(Duration::from_millis(timeout_ms))
            .unwrap_or(started);
        // Local futures release every observation claim synchronously when the
        // aggregate call is cancelled; session execution has separate owners.
        let mut polls = FuturesUnordered::new();
        for (name, startup) in self.sessions.snapshot() {
            let runtime = startup.runtime();
            let call_id = call.id();
            polls.push(async move {
                let response = runtime
                    .worker
                    .poll_active(crate::worker_client::SendRequest {
                        cell: None,
                        stdin: None,
                        requirements: None,
                        control: None,
                        deadline,
                        transcript: runtime.transcript.clone(),
                        call_id,
                    })
                    .await
                    .unwrap_or_else(crate::worker_client::Response::tool_error);
                (name, runtime, response)
            });
        }
        let mut observed = std::collections::BTreeMap::new();
        while let Some((name, runtime, mut response)) = polls.next().await {
            if response.is_empty_stopped() {
                continue;
            }
            observed.insert(name, (runtime, response));
        }
        let responses = observed
            .into_iter()
            .map(|(name, (runtime, response))| {
                let current = runtime.worker.session_status();
                let status = if current == "stopped" {
                    current
                } else {
                    response.session_status().unwrap_or(current)
                };
                (name, status, response)
            })
            .collect::<Vec<_>>();
        Ok(responses_to_tool_result(
            responses,
            call,
            &self.startup.runtime().transcript,
            &self.deliveries,
            delivery,
        )
        .await)
    }
}

async fn response_to_tool_result(
    response: crate::worker_client::Response,
    call: &crate::transcript::Call,
    transcript: &crate::transcript::Transcript,
    deliveries: &crate::server_transport::ResponseDeliveries,
    delivery: &crate::server_transport::ResponseDeliveryCall,
) -> CallToolResult {
    aggregate_to_tool_result(response, Vec::new(), call, transcript, deliveries, delivery).await
}

/// `sessions` are an aggregate reply's sessions. They stay here, unregistered,
/// through the wait for the recording writer: a call cancelled during that
/// wait drops them, which returns each session's copy to its session.
async fn aggregate_to_tool_result(
    mut response: crate::worker_client::Response,
    mut sessions: Vec<crate::worker_client::ResponseDelivery>,
    call: &crate::transcript::Call,
    transcript: &crate::transcript::Transcript,
    deliveries: &crate::server_transport::ResponseDeliveries,
    delivery: &crate::server_transport::ResponseDeliveryCall,
) -> CallToolResult {
    response.settle_recording(transcript, &mut sessions).await;
    for session in sessions {
        deliveries.register(delivery, session);
    }
    let (blocks, is_error, response_delivery, parts) = response.into_parts();
    if let Some(response_delivery) = response_delivery {
        deliveries.register(delivery, response_delivery);
    }
    call.record_result_parts(parts);
    let mut images = Vec::new();
    let content = blocks
        .into_iter()
        .map(|block| match block {
            crate::worker_client::Content::Text(text) => ContentBlock::text(text),
            crate::worker_client::Content::Image {
                data,
                mime_type,
                artifact,
            } => {
                images.push(artifact);
                ContentBlock::image(data, mime_type)
            }
        })
        .collect();
    if let Err(error) = call.record_result_images(images) {
        transcript.disable(error);
    }
    if is_error {
        CallToolResult::error(content)
    } else {
        CallToolResult::success(content)
    }
}

async fn responses_to_tool_result(
    mut responses: Vec<(String, &'static str, crate::worker_client::Response)>,
    call: &crate::transcript::Call,
    transcript: &crate::transcript::Transcript,
    deliveries: &crate::server_transport::ResponseDeliveries,
    delivery: &crate::server_transport::ResponseDeliveryCall,
) -> CallToolResult {
    if responses.len() == 1 {
        let (_, _, response) = responses.pop().expect("one visible session");
        return response_to_tool_result(response, call, transcript, deliveries, delivery).await;
    }
    // Poll output belongs in content. Status-only structured content can hide
    // text, language errors, and images in hosts that prefer structured results.
    let mut combined = crate::worker_client::Response::default();
    let mut sessions = Vec::new();
    for (name, status, response) in responses {
        sessions.extend(combined.append_session(&name, status, response));
    }
    aggregate_to_tool_result(combined, sessions, call, transcript, deliveries, delivery).await
}

/// Console's identity in legacy handshakes and modern result metadata.
fn implementation() -> Implementation {
    Implementation::new("mcp-console", env!("MCP_CONSOLE_VERSION"))
        .with_title("MCP Console")
        .with_description("An interactive workbench for agents working in R, Python, and SQL.")
        .with_website_url("https://github.com/posit-dev/mcp-console")
}

/// Modern results identify their sender independently of connection state.
/// Keep legacy result shapes unchanged; rmcp owns version negotiation.
fn response_metadata(context: &RequestContext<RoleServer>) -> Option<MetaObject> {
    context
        .protocol_version()
        .filter(|version| !version.has_initialize())
        .map(|_| {
            let mut meta = MetaObject::new();
            meta.insert(
                "io.modelcontextprotocol/serverInfo".into(),
                serde_json::to_value(implementation()).expect("server implementation"),
            );
            meta
        })
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for ConsoleServer {
    fn get_info(&self) -> ServerConfig {
        let mut capabilities = ServerCapabilities::builder().enable_tools().build();
        if self.inheritance.enabled() {
            capabilities.experimental = Some(std::collections::BTreeMap::from_iter([
                (sandbox::CAPABILITY.to_string(), serde_json::Map::new()),
                (
                    sandbox::LEGACY_CAPABILITY.to_string(),
                    serde_json::Map::new(),
                ),
            ]));
        }
        ServerConfig::new(capabilities)
            .with_server_info(implementation())
            .with_instructions(self.instructions.clone())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let mut result = ListToolsResult::with_all_items(self.tool_router.list_all());
        result.meta = response_metadata(&context);
        if result.meta.is_some() {
            // Discovery depends only on launch configuration, never a client session.
            result = result.with_ttl_ms(0).with_cache_scope(CacheScope::Public);
        }
        Ok(result)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        mut context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let response_metadata = response_metadata(&context);
        context.extensions.insert(Instant::now());
        if request.name.as_ref() != "send" {
            return self
                .tool_router
                .call(ToolCallContext::new(self, request, context))
                .await;
        }
        // A response can be visible before its write future settles. Delay only
        // console operations so transport receipt remains live for cancellation and EOF.
        let admission = context
            .extensions
            .remove::<crate::server_transport::ResponseDeliveryAdmission>()
            .expect("console operations must carry transport admission");
        let recording = context
            .extensions
            .remove::<crate::server_transport::RecordingCall>()
            .expect("send carries its recording call");
        let call = recording.call();
        let session = request
            .arguments
            .as_ref()
            .and_then(|arguments| arguments.get("session"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("default");
        let transcript = self.startup.runtime().transcript.for_session(session);
        let poll_all = request.arguments.as_ref().is_none_or(|arguments| {
            arguments
                .iter()
                .all(|(key, value)| key == "timeout_ms" || (key == "note" && value.is_null()))
        });
        let selected = self.sessions.get(session);
        context.extensions.insert(PollAll(poll_all));
        let admitted = tokio::select! {
            biased;
            _ = context.ct.cancelled() => Err(ErrorData::internal_error(
                    "request cancelled before execution",
                    None,
                )),
            delivery = admission.admit() => {
                match delivery {
                    Ok(delivery) => Ok(delivery),
                    Err(crate::server_transport::ResponseDeliveryAdmissionError::Cancelled) => {
                        Err(ErrorData::internal_error(
                            "request cancelled before execution",
                            None,
                        ))
                    }
                    Err(crate::server_transport::ResponseDeliveryAdmissionError::Closed) => {
                        Err(ErrorData::internal_error(
                            "MCP input closed before request execution",
                            None,
                        ))
                    }
                }
            }
        };
        let (delivery, operation) = match admitted {
            Ok(admitted) => admitted,
            Err(error) => {
                let result = Err(error);
                transcript.finish(call, &result);
                return recording.response(result);
            }
        };
        let sandbox::CaptureResult(inherited) = context
            .extensions
            .remove::<sandbox::CaptureResult>()
            .expect("console operations must carry transport sandbox capture");
        if self.inheritance.enabled()
            && inherited.is_ok()
            && let Err(error) = self.startup.start()
        {
            self.startup.runtime().worker.finish_startup(Err(error));
        }
        context.extensions.insert(delivery.clone());
        let runtime = self.startup.runtime();
        let runtime_probe =
            context.meta.get(crate::warmup::RUNTIME_META) == Some(&serde_json::Value::Bool(true));
        let runtime_worker = runtime.worker.clone();
        // A restart can reset completed failed readiness later in this call.
        // Its wait stays cancellable until initial configuration is accepted.
        let cancellable_observation = poll_all
            || selected.is_none_or(|startup| {
                let runtime = startup.runtime();
                !runtime.worker.startup_finished() || !runtime.worker.is_configured()
            });
        context.extensions.insert(call.clone());
        // Call the known send route directly so argument decoding errors enter
        // the bounded renderer before the router converts them to plain text.
        let send = self
            .tool_router
            .map
            .get("send")
            .expect("send tool must be registered");
        let cancellation = context.ct.clone();
        let validation = inherited
            .and_then(|()| {
                SendArguments::validate_fields(
                    request.arguments.as_ref(),
                    self.languages,
                    self.language_setting,
                )
            })
            .and_then(|()| {
                self.tool_settings
                    .validate_arguments(request.arguments.as_ref())
            });
        let call_future = async {
            validation.map_err(|message| ErrorData::invalid_params(message, None))?;
            (send.call)(ToolCallContext::new(self, request, context)).await
        };
        let result = if cancellable_observation {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(ErrorData::internal_error("request cancelled; poll with an empty send for any admitted cell", None)),
                result = call_future => result,
            }
        } else {
            call_future.await
        };
        let mut result = match result {
            Err(error) if error.code == ErrorCode::INVALID_PARAMS => Ok(response_to_tool_result(
                crate::worker_client::Response::tool_error(error.message.into_owned()),
                &call,
                &transcript,
                &self.deliveries,
                &delivery,
            )
            .await
            .into()),
            result => result,
        };
        // The warmup driver asks the established discovery owner, not the host
        // PATH or the wording of a runtime error. Ordinary replies are unchanged.
        if runtime_probe
            && let Ok(CallToolResponse::Complete(result)) = &mut result
            && !result.is_error.unwrap_or(false)
            && let Some(available) = runtime_worker.r_available()
        {
            result.meta.get_or_insert_default().insert(
                crate::warmup::RUNTIME_META.into(),
                serde_json::json!({ "r": available }),
            );
        }
        // Add identity before recording and response-delivery wrapping so the
        // canonical tool result retains the same metadata as the wire result.
        if let Some(metadata) = response_metadata
            && let Ok(CallToolResponse::Complete(result)) = &mut result
        {
            result.meta.get_or_insert_default().extend(metadata);
        }
        transcript.finish(call, &result);
        operation.complete();
        recording.response(result)
    }
}

/// Runs the MCP stdio server and owns the selected worker.
///
/// Closing MCP input also stops a worker whose evaluation is still running.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    worker: Option<PathBuf>,
    relay: Option<PathBuf>,
    sandbox: SandboxConfiguration,
    python: Option<crate::settings::PythonChoice>,
    r: crate::settings::R,
    startup: Option<crate::settings::startup::Startup>,
    visibility: Option<Languages>,
    tool_settings: crate::settings::Tool,
    session_settings: crate::settings::Sessions,
    control_socket: Option<PathBuf>,
) -> Result<(), Box<dyn Error>> {
    #[cfg(unix)]
    let control_listener = control_socket.map(control::Listener::bind).transpose()?;
    #[cfg(not(unix))]
    let _ = control_socket;
    let (input_closed, wait_for_input_close) = oneshot::channel();
    let input_closed = InputClosed {
        sender: Arc::new(Mutex::new(Some(input_closed))),
        #[cfg(windows)]
        physical_eof: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    #[cfg(not(windows))]
    let input = tokio::io::stdin();
    let server = ConsoleServer::new(
        input_closed.clone(),
        worker,
        relay,
        sandbox,
        python,
        r,
        startup,
        visibility,
        tool_settings,
        session_settings,
    )
    .map_err(std::io::Error::other)?;
    #[cfg(unix)]
    let control = control_listener.map(|listener| listener.start(server.clone()));
    let startup = server.startup.clone();
    let deliveries = server.deliveries.clone();
    let sessions = server.sessions.clone();
    #[cfg(windows)]
    let input = {
        let sessions = sessions.clone();
        let closed = input_closed.clone();
        input_windows::Input::new(move || {
            // Physical EOF can cancel unfinished preparation even if protocol
            // output is blocked. Remember it before inspecting registered names:
            // a queued request can create another startup after this snapshot.
            closed.note_physical_eof();
            if sessions
                .snapshot()
                .iter()
                .any(|(_, startup)| !startup.runtime().worker.startup_finished())
            {
                closed.close();
            }
        })?
    };
    let input = ShutdownReader::new(input, input_closed);
    let inheritance = server.inheritance.clone();
    let transport = crate::server_transport::ServerTransport::new(
        input,
        tokio::io::stdout(),
        server.deliveries.clone(),
    )
    .with_recording(server.startup.runtime().transcript.clone())
    .with_send_observer(move |call| {
        // Pending response reservations do not order runnable handlers. Capture
        // before dispatch so cancellation or scheduling cannot select a later policy.
        let inherited = inheritance.capture(call.extensions.get_or_insert_default());
        call.extensions.insert(sandbox::CaptureResult(inherited));
    });
    let recordings = transport.recording_queue();
    let startup_responses = deliveries.clone();
    let shutdown = async move {
        let shutdown_started = wait_for_input_close
            .await
            .unwrap_or_else(|_| Instant::now());
        let deadline = shutdown_started + WORKER_SHUTDOWN_GRACE;
        #[cfg(unix)]
        if let Some(control) = &control {
            control.close();
        }
        let result = sessions.shutdown(deadline).await;
        deliveries
            .settle_before_close(Instant::now() + WORKER_SHUTDOWN_GRACE)
            .await;
        // Cancellation releases response admission, not capture ownership.
        // Settle every received send before finalizing its journal and projections.
        recordings.close_and_settle().await;
        #[cfg(unix)]
        if let Some(control) = control {
            control.finish().await;
        }
        let runtime = startup.runtime();
        let diagnostics = runtime.transcript.clone();
        if let Err(error) = tokio::task::spawn_blocking(move || {
            sessions.finish_recording();
            runtime.transcript.finalize();
        })
        .await
        {
            diagnostics.disable(format!("recording finalization task failed: {error}"));
        }
        result?;
        Ok::<(), String>(())
    };

    tokio::pin!(shutdown);
    const CLOSED_BEFORE_INITIALIZATION: &str = "server startup cancelled because MCP input closed";
    let service = tokio::select! {
        result = server.serve(transport) => match result {
            Ok(service) => service,
            Err(error) => {
                // Shutdown can wake a blocked initialization write before this
                // branch settles. Preserve its input-closure diagnosis.
                let closed_by_shutdown = startup_responses.is_closed();
                shutdown.await.map_err(std::io::Error::other)?;
                if matches!(&error, rmcp::service::ServerInitializeError::ConnectionClosed(_))
                    && startup_responses.bootstrap_complete()
                {
                    return Ok(());
                }
                return Err(match error {
                    rmcp::service::ServerInitializeError::ConnectionClosed(_) =>
                        std::io::Error::other(CLOSED_BEFORE_INITIALIZATION).into(),
                    _ if closed_by_shutdown =>
                        std::io::Error::other(CLOSED_BEFORE_INITIALIZATION).into(),
                    error => error.into(),
                });
            }
        },
        result = &mut shutdown => {
            result.map_err(std::io::Error::other)?;
            if startup_responses.bootstrap_complete() {
                return Ok(());
            }
            return Err(std::io::Error::other(CLOSED_BEFORE_INITIALIZATION).into());
        }
    };
    // Once owned preparation/worker retirement and response settling finish,
    // a blocked protocol write must not keep the process alive.
    tokio::select! {
        result = service.waiting() => {
            shutdown.await.map_err(std::io::Error::other)?;
            result?;
        },
        result = &mut shutdown => result.map_err(std::io::Error::other)?,
    }
    Ok(())
}

#[derive(Clone)]
struct InputClosed {
    sender: Arc<Mutex<Option<oneshot::Sender<Instant>>>>,
    #[cfg(windows)]
    physical_eof: Arc<std::sync::atomic::AtomicBool>,
}

impl InputClosed {
    fn close(&self) {
        if let Some(sender) = self.sender.lock().expect("input closure lock").take() {
            let _ = sender.send(Instant::now());
        }
    }

    #[cfg(windows)]
    fn note_physical_eof(&self) {
        self.physical_eof
            .store(true, std::sync::atomic::Ordering::Release);
    }

    #[cfg(windows)]
    fn cancel_startup_after_physical_eof(&self) {
        if self.physical_eof.load(std::sync::atomic::Ordering::Acquire) {
            self.close();
        }
    }
}

/// Reports EOF or reader loss to the owner, sharing one notification with the
/// non-consuming startup observer.
struct ShutdownReader<R> {
    inner: R,
    input_closed: InputClosed,
}

impl<R> ShutdownReader<R> {
    fn new(inner: R, input_closed: InputClosed) -> Self {
        Self {
            inner,
            input_closed,
        }
    }
}

impl<R> Drop for ShutdownReader<R> {
    fn drop(&mut self) {
        self.input_closed.close();
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for ShutdownReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let filled = buffer.filled().len();
        let had_capacity = buffer.remaining() > 0;
        let poll = Pin::new(&mut self.inner).poll_read(context, buffer);
        match poll {
            Poll::Ready(Ok(())) if had_capacity && buffer.filled().len() == filled => {
                self.input_closed.close();
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(error)) => {
                self.input_closed.close();
                Poll::Ready(Err(error))
            }
            poll => poll,
        }
    }
}
