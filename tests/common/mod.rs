#![allow(dead_code)]

use std::error::Error;
use std::path::PathBuf;
use std::pin::Pin;
#[cfg(target_os = "macos")]
use std::sync::OnceLock;

use rmcp::ServiceExt;
use rmcp::handler::client::ClientHandler;
use rmcp::model::{
    CallToolRequestParams, ClientNotification, ClientRequest, CustomNotification, CustomRequest,
    RawContent,
};
use rmcp::service::ServiceError;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::process::Command;

pub type TestResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

const TEST_PAGER_PAGE_CHARS: u64 = 300;
const PAGER_PAGE_CHARS_ENV: &str = "MCP_CONSOLE_PAGER_PAGE_CHARS";
#[cfg(windows)]
const WINDOWS_TEST_TIMEOUT_CAP_SECS: f64 = 60.0;

#[cfg(target_os = "macos")]
pub fn sandbox_exec_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        if std::env::var_os("CODEX_SANDBOX").is_some() {
            return false;
        }
        std::process::Command::new("/usr/bin/sandbox-exec")
            .args(["-p", "(version 1)", "--", "/usr/bin/true"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    })
}

#[cfg(target_os = "windows")]
pub fn sandbox_exec_available() -> bool {
    false
}

#[cfg(target_os = "linux")]
pub fn sandbox_exec_available() -> bool {
    true
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub fn sandbox_exec_available() -> bool {
    true
}

#[macro_export]
macro_rules! mcp_session {
    (|$session:ident| $($body:tt)*) => {
        |$session| ::std::boxed::Box::pin(async move { $($body)* })
    };
}

#[macro_export]
macro_rules! mcp_timeout_opt {
    () => {
        None
    };
    ($timeout:expr) => {
        Some($timeout)
    };
}

#[macro_export]
macro_rules! mcp_calls {
    ($session:ident, $($calls:tt)*) => {{
        $crate::mcp_calls_inner!($session, $($calls)*);
    }};
}

#[macro_export]
macro_rules! mcp_calls_inner {
    ($session:ident,) => {};
    ($session:ident) => {};

    ($session:ident, write_stdin($input:expr $(, timeout = $timeout:expr)? ); $($rest:tt)*) => {{
        $session
            .write_stdin_with($input, $crate::mcp_timeout_opt!($($timeout)?))
            .await;
        $crate::mcp_calls_inner!($session, $($rest)*);
    }};

}

#[macro_export]
macro_rules! mcp_script {
    ($($calls:tt)*) => {{
        $crate::mcp_session!(|session| {
            $crate::mcp_calls!(session, $($calls)*);
            Ok(())
        })
    }};
}

#[derive(Clone)]
struct TestClient;

impl ClientHandler for TestClient {}

#[derive(Debug, Clone, Serialize)]
struct SnapshotStep {
    call: SnapshotCall,
    response: SnapshotResponse,
}

#[derive(Debug, Clone, Serialize)]
struct SnapshotCall {
    tool: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    arguments: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SnapshotResponse {
    ToolResult(SnapshotCallToolResult),
    ServiceError(SnapshotServiceError),
}

#[derive(Debug, Clone, Serialize)]
struct SnapshotCallToolResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    is_error: Option<bool>,
    content: Vec<SnapshotContent>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SnapshotContent {
    Text { text: String },
    Image { mime_type: String, data_len: usize },
    Audio { mime_type: String, data_len: usize },
    Resource { resource: Value },
    ResourceLink { resource: Value },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SnapshotServiceError {
    McpError { error: rmcp::ErrorData },
    TransportSend { message: String },
    TransportClosed,
    UnexpectedResponse,
    Cancelled { reason: Option<String> },
    Timeout { timeout_ms: u128 },
    Other { message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TestBackend {
    R,
    Python,
}

impl TestBackend {
    fn repl_tool_name(self) -> &'static str {
        match self {
            Self::R => "r_repl",
            Self::Python => "py_repl",
        }
    }
}

fn normalize_tool_name_for_request(tool: &str) -> &str {
    match tool {
        "r_repl" | "py_repl" => "repl",
        "r_repl_reset" | "py_repl_reset" => "repl_reset",
        _ => tool,
    }
}

fn is_repl_tool_name(tool: &str) -> bool {
    matches!(tool, "repl" | "r_repl" | "py_repl")
}

fn normalize_newlines(mut text: String) -> String {
    if text.contains("\r\n") {
        text = text.replace("\r\n", "\n");
    }
    text
}

fn strip_trailing_prompt(text: &str) -> String {
    if let Some(stripped) = text.strip_suffix("\n> ") {
        return stripped.to_string();
    }
    if let Some(stripped) = text.strip_suffix("\n+ ") {
        return stripped.to_string();
    }
    if let Some(stripped) = text.strip_suffix("\n>>> ") {
        return stripped.to_string();
    }
    if let Some(stripped) = text.strip_suffix("\n... ") {
        return stripped.to_string();
    }
    text.to_string()
}

fn normalize_text_snapshot(text: &str) -> String {
    let normalized = normalize_newlines(text.to_string());
    let mut stripped = strip_trailing_prompt(&normalized);
    while stripped.ends_with('\n') {
        stripped.pop();
    }
    if let Some(rest) = stripped.strip_prefix("\nstderr:") {
        stripped = format!("stderr:{rest}");
    }
    stripped
}

fn pretty_json(value: &Value) -> String {
    normalize_newlines(serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()))
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
}

fn normalized_test_timeout(timeout: Option<f64>) -> Option<f64> {
    #[cfg(windows)]
    {
        timeout.map(|value| value.min(WINDOWS_TEST_TIMEOUT_CAP_SECS))
    }
    #[cfg(not(windows))]
    {
        timeout
    }
}

fn service_error_snapshot(err: &ServiceError) -> SnapshotServiceError {
    match err {
        ServiceError::McpError(error) => SnapshotServiceError::McpError {
            error: error.clone(),
        },
        ServiceError::TransportSend(error) => SnapshotServiceError::TransportSend {
            message: error.to_string(),
        },
        ServiceError::TransportClosed => SnapshotServiceError::TransportClosed,
        ServiceError::UnexpectedResponse => SnapshotServiceError::UnexpectedResponse,
        ServiceError::Cancelled { reason } => SnapshotServiceError::Cancelled {
            reason: reason.clone(),
        },
        ServiceError::Timeout { timeout } => SnapshotServiceError::Timeout {
            timeout_ms: timeout.as_millis(),
        },
        _ => SnapshotServiceError::Other {
            message: err.to_string(),
        },
    }
}

fn tool_result_snapshot(result: &rmcp::model::CallToolResult) -> SnapshotCallToolResult {
    let content = result
        .content
        .iter()
        .map(|content| match &content.raw {
            RawContent::Text(text) => SnapshotContent::Text {
                text: normalize_text_snapshot(&text.text),
            },
            RawContent::Image(image) => SnapshotContent::Image {
                mime_type: image.mime_type.clone(),
                data_len: image.data.len(),
            },
            RawContent::Audio(audio) => SnapshotContent::Audio {
                mime_type: audio.mime_type.clone(),
                data_len: audio.data.len(),
            },
            RawContent::Resource(resource) => SnapshotContent::Resource {
                resource: serde_json::to_value(&resource.resource)
                    .unwrap_or_else(|_| json!({"error": "failed to serialize resource"})),
            },
            RawContent::ResourceLink(resource) => SnapshotContent::ResourceLink {
                resource: serde_json::to_value(resource)
                    .unwrap_or_else(|_| json!({"error": "failed to serialize resource_link"})),
            },
        })
        .collect();

    SnapshotCallToolResult {
        is_error: result.is_error,
        content,
    }
}

pub struct McpTestSession {
    service: rmcp::service::RunningService<rmcp::service::RoleClient, TestClient>,
    steps: Vec<SnapshotStep>,
    server_pid: Option<u32>,
    backend: TestBackend,
}

impl McpTestSession {
    pub fn server_info(&self) -> Option<&rmcp::model::ServerInfo> {
        self.service.peer_info()
    }

    #[allow(dead_code)]
    pub async fn write_stdin(&mut self, input: impl Into<String>) {
        self.write_stdin_with(input, None).await;
    }

    #[allow(dead_code)]
    pub async fn write_stdin_with(&mut self, input: impl Into<String>, timeout: Option<f64>) {
        let mut input = input.into();
        if !input.ends_with('\n') {
            input.push('\n');
        }
        let timeout = normalized_test_timeout(timeout);

        let mut args = serde_json::Map::new();
        args.insert("input".to_string(), Value::String(input));
        if let Some(timeout) = timeout {
            args.insert(
                "timeout_ms".to_string(),
                json!((timeout * 1000.0).round() as i64),
            );
        }
        self.call_tool(self.repl_tool_name(), Value::Object(args))
            .await;
    }

    pub fn repl_tool_name(&self) -> &'static str {
        self.backend.repl_tool_name()
    }

    pub async fn call_tool(&mut self, tool: impl Into<String>, arguments: Value) {
        let tool = tool.into();
        let request_tool = normalize_tool_name_for_request(&tool).to_string();
        let arguments_for_snapshot = match &arguments {
            Value::Null => None,
            other => Some(other.clone()),
        };

        let arguments_for_mcp = match arguments {
            Value::Null => None,
            Value::Object(map) => Some(map.into_iter().collect()),
            other => {
                self.steps.push(SnapshotStep {
                    call: SnapshotCall {
                        tool,
                        arguments: Some(other),
                    },
                    response: SnapshotResponse::ServiceError(SnapshotServiceError::McpError {
                        error: rmcp::ErrorData::invalid_params(
                            "tool arguments must be a JSON object",
                            None,
                        ),
                    }),
                });
                return;
            }
        };

        let result = self
            .service
            .call_tool(CallToolRequestParams {
                meta: None,
                name: request_tool.into(),
                arguments: arguments_for_mcp,
                task: None,
            })
            .await;

        let response = match result {
            Ok(result) => SnapshotResponse::ToolResult(tool_result_snapshot(&result)),
            Err(err) => SnapshotResponse::ServiceError(service_error_snapshot(&err)),
        };

        self.steps.push(SnapshotStep {
            call: SnapshotCall {
                tool,
                arguments: arguments_for_snapshot,
            },
            response,
        });
    }

    pub async fn call_tool_raw(
        &mut self,
        tool: impl Into<String>,
        arguments: Value,
    ) -> Result<rmcp::model::CallToolResult, ServiceError> {
        let tool = tool.into();
        let request_tool = normalize_tool_name_for_request(&tool).to_string();
        let arguments = match arguments {
            Value::Null => None,
            Value::Object(map) => Some(map.into_iter().collect()),
            _ => {
                return Err(ServiceError::McpError(rmcp::ErrorData::invalid_params(
                    "tool arguments must be a JSON object",
                    None,
                )));
            }
        };
        self.service
            .call_tool(CallToolRequestParams {
                meta: None,
                name: request_tool.into(),
                arguments,
                task: None,
            })
            .await
    }

    pub async fn write_stdin_raw_with(
        &mut self,
        input: impl Into<String>,
        timeout: Option<f64>,
    ) -> Result<rmcp::model::CallToolResult, ServiceError> {
        let mut input = input.into();
        if !input.ends_with('\n') {
            input.push('\n');
        }
        let timeout = normalized_test_timeout(timeout);

        let mut args = serde_json::Map::new();
        args.insert("input".to_string(), Value::String(input));
        if let Some(timeout) = timeout {
            args.insert(
                "timeout_ms".to_string(),
                json!((timeout * 1000.0).round() as i64),
            );
        }
        self.call_tool_raw(self.repl_tool_name(), Value::Object(args))
            .await
    }

    pub async fn write_stdin_raw_unterminated_with(
        &mut self,
        input: impl Into<String>,
        timeout: Option<f64>,
    ) -> Result<rmcp::model::CallToolResult, ServiceError> {
        let input = input.into();
        let timeout = normalized_test_timeout(timeout);
        let mut args = serde_json::Map::new();
        args.insert("input".to_string(), Value::String(input));
        if let Some(timeout) = timeout {
            args.insert(
                "timeout_ms".to_string(),
                json!((timeout * 1000.0).round() as i64),
            );
        }
        self.call_tool_raw(self.repl_tool_name(), Value::Object(args))
            .await
    }

    pub async fn send_custom_request(
        &mut self,
        method: impl Into<String>,
        params: Value,
    ) -> Result<(), ServiceError> {
        let request = ClientRequest::CustomRequest(CustomRequest::new(method, Some(params)));
        let _ = self.service.send_request(request).await?;
        Ok(())
    }

    pub async fn send_custom_notification(
        &mut self,
        method: impl Into<String>,
        params: Value,
    ) -> Result<(), ServiceError> {
        let notification =
            ClientNotification::CustomNotification(CustomNotification::new(method, Some(params)));
        self.service.send_notification(notification).await
    }

    pub async fn cancel(self) -> TestResult<()> {
        self.service.cancel().await?;
        if let Some(pid) = self.server_pid {
            terminate_process_tree(pid);
        }
        Ok(())
    }
}

pub struct McpSnapshot {
    sessions: Vec<(String, Vec<SnapshotStep>)>,
}

impl McpSnapshot {
    pub fn new() -> Self {
        Self {
            sessions: Vec::new(),
        }
    }

    pub async fn session<F>(&mut self, name: impl Into<String>, f: F) -> TestResult<()>
    where
        F: for<'a> FnOnce(
            &'a mut McpTestSession,
        )
            -> Pin<Box<dyn std::future::Future<Output = TestResult<()>> + Send + 'a>>,
    {
        let name = name.into();
        let mut session = spawn_server().await?;
        f(&mut session).await?;
        let steps = session.steps.clone();
        session.cancel().await?;
        self.sessions.push((name, steps));
        Ok(())
    }

    pub fn render(&self) -> String {
        self.render_json()
    }

    pub fn render_json(&self) -> String {
        let mut out = String::new();
        for (index, (name, steps)) in self.sessions.iter().enumerate() {
            if index > 0 {
                out.push('\n');
            }
            out.push_str(&format!("== session: {name} ==\n"));
            for (i, step) in steps.iter().enumerate() {
                out.push_str(&format!("-- step {} --\n", i + 1));
                out.push_str("call:\n");
                out.push_str(&pretty_json(
                    &serde_json::to_value(&step.call)
                        .unwrap_or_else(|_| json!({"error":"serialize call"})),
                ));
                out.push('\n');
                out.push_str("response:\n");
                let response = normalize_snapshot_response(&step.response);
                out.push_str(&pretty_json(
                    &serde_json::to_value(&response)
                        .unwrap_or_else(|_| json!({"error":"serialize outcome"})),
                ));
                out.push('\n');
            }
        }
        out.trim_end().to_string()
    }

    pub fn render_transcript(&self) -> String {
        let mut out = String::new();
        for (session_index, (name, steps)) in self.sessions.iter().enumerate() {
            if session_index > 0 {
                out.push('\n');
            }
            out.push_str(&format!("== session: {name} ==\n"));

            for (step_index, step) in steps.iter().enumerate() {
                if step_index > 0 {
                    out.push('\n');
                }

                let response = normalize_snapshot_response(&step.response);
                let is_error = snapshot_response_is_error(&response);
                let (call_desc, input_lines) = format_snapshot_call(&step.call);

                if is_error {
                    out.push_str(&format!("{}) ! {call_desc}\n", step_index + 1));
                } else {
                    out.push_str(&format!("{}) {call_desc}\n", step_index + 1));
                }

                for line in input_lines {
                    out.push_str(&format!(">>> {line}\n"));
                }

                let response_lines = format_snapshot_response_lines(&response, &step.call.tool);
                for line in response_lines {
                    out.push_str(&format!("<<< {line}\n"));
                }
            }
        }

        out.trim_end().to_string()
    }
}

fn normalize_snapshot_response(response: &SnapshotResponse) -> SnapshotResponse {
    match response {
        SnapshotResponse::ToolResult(result) => {
            SnapshotResponse::ToolResult(SnapshotCallToolResult {
                is_error: result.is_error,
                content: result
                    .content
                    .iter()
                    .map(normalize_snapshot_content)
                    .collect(),
            })
        }
        SnapshotResponse::ServiceError(err) => SnapshotResponse::ServiceError(err.clone()),
    }
}

fn normalize_snapshot_content(content: &SnapshotContent) -> SnapshotContent {
    match content {
        SnapshotContent::Text { text } => SnapshotContent::Text {
            text: normalize_snapshot_text(text),
        },
        SnapshotContent::Image {
            mime_type,
            data_len: _,
        } => SnapshotContent::Image {
            mime_type: mime_type.clone(),
            data_len: 0,
        },
        SnapshotContent::Audio {
            mime_type,
            data_len: _,
        } => SnapshotContent::Audio {
            mime_type: mime_type.clone(),
            data_len: 0,
        },
        SnapshotContent::Resource { resource } => SnapshotContent::Resource {
            resource: resource.clone(),
        },
        SnapshotContent::ResourceLink { resource } => SnapshotContent::ResourceLink {
            resource: resource.clone(),
        },
    }
}

fn normalize_snapshot_text(text: &str) -> String {
    if text.starts_with("\n[repl] session ended") {
        return text.trim_start_matches('\n').to_string();
    }
    let text = normalize_busy_timeout_elapsed_ms(&normalize_pager_elision(text));
    if !text.contains("stderr:") {
        return text;
    }

    let lines: Vec<String> = text
        .split_inclusive('\n')
        .map(|line| line.to_string())
        .collect();
    if lines.len() <= 1 {
        if let Some(line) = lines.first()
            && is_stderr_line(line)
        {
            return normalize_stderr_line(line);
        }
        return text.to_string();
    }

    let mut stdout_lines = Vec::new();
    let mut stderr_lines = Vec::new();
    for line in lines {
        if is_stderr_line(&line) {
            stderr_lines.push(normalize_stderr_line(&line));
        } else {
            stdout_lines.push(line);
        }
    }

    if stdout_lines.is_empty() || stderr_lines.is_empty() {
        if stdout_lines.is_empty() {
            let mut out = String::new();
            for line in stderr_lines {
                out.push_str(&line);
            }
            return out;
        }
        return text.to_string();
    }

    let mut out = String::new();
    for line in stdout_lines.into_iter().chain(stderr_lines.into_iter()) {
        out.push_str(&line);
    }
    out
}

fn normalize_busy_timeout_elapsed_ms(text: &str) -> String {
    let marker = "elapsed_ms=";
    let mut out = String::with_capacity(text.len());
    let mut idx = 0;
    while let Some(pos) = text[idx..].find(marker) {
        let abs = idx + pos;
        out.push_str(&text[idx..abs]);
        out.push_str(marker);
        let mut end = abs + marker.len();
        let bytes = text.as_bytes();
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        if end > abs + marker.len() {
            out.push('N');
        }
        idx = end;
    }
    out.push_str(&text[idx..]);
    out
}

fn normalize_pager_elision(text: &str) -> String {
    let marker = "[pager] elided output: @";
    let mut out = String::with_capacity(text.len());
    let mut idx = 0;
    while let Some(pos) = text[idx..].find(marker) {
        let abs = idx + pos;
        out.push_str(&text[idx..abs]);
        out.push_str(marker);
        let mut end = abs + marker.len();
        let bytes = text.as_bytes();
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        if end + 1 < bytes.len() && bytes[end] == b'.' && bytes[end + 1] == b'.' {
            end += 2;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            out.push_str("N..N");
        } else {
            out.push('N');
        }
        idx = end;
    }
    out.push_str(&text[idx..]);
    out
}

fn snapshot_response_is_error(response: &SnapshotResponse) -> bool {
    match response {
        SnapshotResponse::ToolResult(result) => matches!(result.is_error, Some(true)),
        SnapshotResponse::ServiceError(_) => true,
    }
}

fn format_snapshot_call(call: &SnapshotCall) -> (String, Vec<String>) {
    let mut input_lines = Vec::new();
    let mut params = Vec::new();

    if let Some(Value::Object(map)) = &call.arguments {
        match call.tool.as_str() {
            tool if is_repl_tool_name(tool) => {
                if let Some(Value::String(input)) = map.get("input") {
                    input_lines = split_input_lines(input);
                }
                if let Some(timeout_ms) = map.get("timeout_ms") {
                    params.push(format!("timeout_ms={}", format_arg_value(timeout_ms)));
                }
                for (key, value) in map {
                    if key == "input" || key == "timeout_ms" {
                        continue;
                    }
                    params.push(format!("{key}={}", format_arg_value(value)));
                }
            }
            _ => {
                for (key, value) in map {
                    params.push(format!("{key}={}", format_arg_value(value)));
                }
            }
        }
    } else if let Some(other) = &call.arguments {
        params.push(format!("args={}", compact_json(other)));
    }

    let mut desc = call.tool.clone();
    if !params.is_empty() {
        desc.push(' ');
        desc.push_str(&params.join(" "));
    }
    (desc, input_lines)
}

fn split_input_lines(input: &str) -> Vec<String> {
    let trimmed = input.strip_suffix('\n').unwrap_or(input);
    trimmed.split('\n').map(|line| line.to_string()).collect()
}

fn format_arg_value(value: &Value) -> String {
    match value {
        Value::String(value) => {
            if value
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.')
            {
                value.clone()
            } else {
                serde_json::to_string(value).unwrap_or_else(|_| value.clone())
            }
        }
        Value::Number(_) | Value::Bool(_) | Value::Null => value.to_string(),
        _ => compact_json(value),
    }
}

fn format_snapshot_response_lines(response: &SnapshotResponse, tool: &str) -> Vec<String> {
    match response {
        SnapshotResponse::ToolResult(result) => {
            let mut lines = Vec::new();
            for content in &result.content {
                match content {
                    SnapshotContent::Text { text } => {
                        for line in split_text_lines(text) {
                            if is_repl_tool_name(tool) && is_prompt_line(&line) {
                                continue;
                            }
                            lines.push(line);
                        }
                    }
                    SnapshotContent::Image {
                        mime_type,
                        data_len,
                    } => {
                        lines.push(format!("[{} len={}]", mime_type, data_len));
                    }
                    SnapshotContent::Audio {
                        mime_type,
                        data_len,
                    } => {
                        lines.push(format!("[{} len={}]", mime_type, data_len));
                    }
                    SnapshotContent::Resource { resource } => {
                        lines.push(format!("[resource {}]", compact_json(resource)));
                    }
                    SnapshotContent::ResourceLink { resource } => {
                        lines.push(format!("[resource_link {}]", compact_json(resource)));
                    }
                }
            }
            lines
        }
        SnapshotResponse::ServiceError(err) => {
            vec![format!("ERROR: {}", format_service_error(err))]
        }
    }
}

fn format_service_error(err: &SnapshotServiceError) -> String {
    serde_json::to_string(err).unwrap_or_else(|_| format!("{err:?}"))
}

fn split_text_lines(text: &str) -> Vec<String> {
    text.split('\n').map(|line| line.to_string()).collect()
}

fn is_prompt_line(line: &str) -> bool {
    if line.is_empty() {
        return false;
    }
    if line.starts_with(' ') || line.starts_with('\t') {
        return false;
    }
    strip_prompt_prefix(line).is_some()
}

fn is_stderr_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    let remainder = strip_prompt_prefix(trimmed).unwrap_or(trimmed);
    remainder.trim_start().starts_with("stderr:")
}

fn normalize_stderr_line(line: &str) -> String {
    let (body, newline) = match line.strip_suffix('\n') {
        Some(body) => (body, "\n"),
        None => (line, ""),
    };
    let trimmed = body.trim_start();
    let remainder = strip_prompt_prefix(trimmed).unwrap_or(trimmed);
    if remainder.trim_start().starts_with("stderr:") {
        format!("{}{}", remainder.trim_start(), newline)
    } else {
        line.to_string()
    }
}

fn strip_prompt_prefix(line: &str) -> Option<&str> {
    if let Some(rest) = line.strip_prefix(">") {
        return Some(rest.strip_prefix(' ').unwrap_or(rest));
    }
    if let Some(rest) = line.strip_prefix("+") {
        return Some(rest.strip_prefix(' ').unwrap_or(rest));
    }
    if let Some(rest) = line.strip_prefix(">>>") {
        return Some(rest.strip_prefix(' ').unwrap_or(rest));
    }
    if let Some(rest) = line.strip_prefix("...") {
        return Some(rest.strip_prefix(' ').unwrap_or(rest));
    }
    if let Some(rest) = line.strip_prefix("Browse[") {
        let close = rest.find(']')?;
        let suffix = &rest[close + 1..];
        if let Some(after) = suffix.strip_prefix('>') {
            return Some(after.strip_prefix(' ').unwrap_or(after));
        }
        if let Some(after) = suffix.strip_prefix('+') {
            return Some(after.strip_prefix(' ').unwrap_or(after));
        }
    }
    None
}

pub async fn spawn_server() -> TestResult<McpTestSession> {
    spawn_server_with_pager_page_chars(TEST_PAGER_PAGE_CHARS).await
}

pub async fn spawn_server_with_pager_page_chars(page_bytes: u64) -> TestResult<McpTestSession> {
    spawn_server_with_args_env_and_pager_page_chars(Vec::new(), Vec::new(), page_bytes).await
}

pub async fn spawn_server_with_env_vars(
    env_vars: Vec<(String, String)>,
) -> TestResult<McpTestSession> {
    spawn_server_with_args_env_and_pager_page_chars(Vec::new(), env_vars, TEST_PAGER_PAGE_CHARS)
        .await
}

pub async fn spawn_server_with_args(args: Vec<String>) -> TestResult<McpTestSession> {
    spawn_server_with_args_env_and_pager_page_chars(args, Vec::new(), TEST_PAGER_PAGE_CHARS).await
}

pub async fn spawn_python_server() -> TestResult<McpTestSession> {
    spawn_server_with_args(vec![
        "--interpreter".to_string(),
        "python".to_string(),
        "--sandbox".to_string(),
        "danger-full-access".to_string(),
    ])
    .await
}

pub fn python_available() -> bool {
    python_program().is_some()
}

pub(crate) fn python_program_with_checker(
    mut ok: impl FnMut(&str) -> bool,
) -> Option<&'static str> {
    if ok("python3") {
        return Some("python3");
    }
    if ok("python") {
        return Some("python");
    }
    None
}

pub fn python_program() -> Option<&'static str> {
    python_program_with_checker(|program| {
        std::process::Command::new(program)
            .args(["-c", "import sys; sys.exit(0)"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    })
}

pub async fn spawn_server_with_args_env_and_pager_page_chars(
    args: Vec<String>,
    env_vars: Vec<(String, String)>,
    page_bytes: u64,
) -> TestResult<McpTestSession> {
    let exe = resolve_server_path()?;
    let env_vars = env_vars.clone();
    let backend = parse_backend_from_args(&args);
    let mut args = args.clone();
    if !sandbox_exec_available()
        && !args
            .iter()
            .any(|arg| arg == "--sandbox" || arg.starts_with("--sandbox="))
    {
        args.push("--sandbox".to_string());
        args.push("danger-full-access".to_string());
    }
    let transport = TokioChildProcess::new(Command::new(exe).configure(|cmd| {
        cmd.env_remove("R_PROFILE_USER");
        cmd.env_remove("R_PROFILE_SITE");
        cmd.env_remove("R_ENVIRON");
        cmd.env_remove("R_ENVIRON_USER");
        cmd.env_remove("MCP_CONSOLE_UPDATE_PLOT_IMAGES");
        cmd.env(PAGER_PAGE_CHARS_ENV, page_bytes.to_string());
        cmd.args(&args);
        for (key, value) in &env_vars {
            cmd.env(key, value);
        }
    }))?;

    let server_pid = transport.id();
    let service = TestClient.serve(transport).await?;
    Ok(McpTestSession {
        service,
        steps: Vec::new(),
        server_pid,
        backend,
    })
}

fn parse_backend_from_args(args: &[String]) -> TestBackend {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--interpreter" || arg == "--backend" {
            if iter.next().is_some_and(|value| value == "python") {
                return TestBackend::Python;
            }
            continue;
        }
        if arg
            .strip_prefix("--interpreter=")
            .is_some_and(|value| value == "python")
        {
            return TestBackend::Python;
        }
        if arg
            .strip_prefix("--backend=")
            .is_some_and(|value| value == "python")
        {
            return TestBackend::Python;
        }
    }
    TestBackend::R
}

fn resolve_server_path() -> TestResult<PathBuf> {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_mcp-repl") {
        return Ok(PathBuf::from(path));
    }
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_mcp-console") {
        return Ok(PathBuf::from(path));
    }

    let mut path = std::env::current_exe()?;
    path.pop();
    path.pop();
    for candidate in ["mcp-repl", "mcp-console"] {
        let mut candidate_path = path.clone();
        candidate_path.push(candidate);
        if cfg!(windows) {
            candidate_path.set_extension("exe");
        }
        if candidate_path.exists() {
            return Ok(candidate_path);
        }
    }
    Err("unable to locate mcp-repl test binary".into())
}

#[cfg(unix)]
fn terminate_process_tree(pid: u32) {
    let pid_str = pid.to_string();
    let _ = std::process::Command::new("pkill")
        .args(["-TERM", "-P", &pid_str])
        .status();
    unsafe {
        let _ = libc::kill(pid as i32, libc::SIGTERM);
    }
    std::thread::sleep(std::time::Duration::from_millis(200));
    let alive = unsafe { libc::kill(pid as i32, 0) == 0 };
    if alive {
        unsafe {
            let _ = libc::kill(pid as i32, libc::SIGKILL);
        }
    }
}

#[cfg(windows)]
fn terminate_process_tree(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}
