#![allow(dead_code)]

use std::error::Error;
#[cfg(windows)]
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
#[cfg(target_os = "macos")]
use std::sync::OnceLock;
#[cfg(windows)]
use std::{ffi::OsStr, os::windows::ffi::OsStrExt};

use regex_lite::Regex;
use rmcp::ServiceExt;
use rmcp::handler::client::ClientHandler;
use rmcp::model::{
    CallToolRequestParams, ClientNotification, ClientRequest, CustomNotification, CustomRequest,
    Meta, RawContent,
};
use rmcp::service::ServiceError;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::process::Command;
#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{CloseHandle, WAIT_ABANDONED, WAIT_OBJECT_0},
    System::Threading::{CreateMutexW, INFINITE, ReleaseMutex, WaitForSingleObject},
};

pub type TestResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

const TEST_PAGER_PAGE_CHARS: u64 = 300;
#[cfg(windows)]
const WINDOWS_TEST_TIMEOUT_CAP_SECS: f64 = 60.0;
#[cfg(windows)]
const WINDOWS_TEST_SERVER_MUTEX_NAME: &str = "Local\\mcp_repl_test_server_mutex";

#[cfg(windows)]
struct WindowsSuiteServerMutexOwner {
    release_tx: std::sync::mpsc::Sender<()>,
    join_handle: std::thread::JoinHandle<()>,
}

#[cfg(windows)]
fn windows_suite_server_lock_path_seed(repo_root: &Path) -> String {
    let normalized = std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    let mut seed = normalized.to_string_lossy().into_owned();
    seed.make_ascii_lowercase();
    seed
}

#[cfg(windows)]
fn windows_suite_server_lock_hash(seed: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in seed.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(windows)]
fn windows_suite_server_mutex_name_for_checkout(repo_root: &Path) -> String {
    let seed = windows_suite_server_lock_path_seed(repo_root);
    let hash = windows_suite_server_lock_hash(&seed);
    format!("{WINDOWS_TEST_SERVER_MUTEX_NAME}_{hash:016x}")
}

#[cfg(windows)]
fn windows_suite_server_mutex_name_wide() -> Vec<u16> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let name = windows_suite_server_mutex_name_for_checkout(&repo_root);
    let mut name: Vec<u16> = OsStr::new(&name).encode_wide().collect();
    name.push(0);
    name
}

#[cfg(windows)]
pub(crate) fn suite_server_lock_name_for_tests(repo_root: &Path) -> String {
    windows_suite_server_mutex_name_for_checkout(repo_root)
}

#[cfg(windows)]
fn acquire_windows_suite_server_mutex_handle() -> TestResult<usize> {
    let name = windows_suite_server_mutex_name_wide();
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error().into());
    }

    let wait = unsafe { WaitForSingleObject(handle, INFINITE) };
    if wait != WAIT_OBJECT_0 && wait != WAIT_ABANDONED {
        unsafe {
            CloseHandle(handle);
        }
        return Err(std::io::Error::other(format!("unexpected mutex wait result: {wait}")).into());
    }

    Ok(handle as usize)
}

#[cfg(windows)]
fn release_windows_suite_server_mutex_handle(handle: usize) {
    let handle = handle as *mut std::ffi::c_void;
    unsafe {
        let _ = ReleaseMutex(handle);
        let _ = CloseHandle(handle);
    }
}

#[cfg(windows)]
fn close_windows_suite_server_mutex_handle(handle: usize) {
    let handle = handle as *mut std::ffi::c_void;
    unsafe {
        let _ = CloseHandle(handle);
    }
}

#[cfg(windows)]
impl WindowsSuiteServerMutexOwner {
    fn start() -> TestResult<Self> {
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let join_handle = std::thread::spawn(move || {
            let handle = match acquire_windows_suite_server_mutex_handle() {
                Ok(handle) => handle,
                Err(err) => {
                    let _ = ready_tx.send(Err(err));
                    return;
                }
            };
            if ready_tx.send(Ok(())).is_err() {
                release_windows_suite_server_mutex_handle(handle);
                return;
            }
            let _ = release_rx.recv();
            release_windows_suite_server_mutex_handle(handle);
        });

        ready_rx.recv().map_err(|_| {
            std::io::Error::other("suite lock owner thread exited before acquiring")
        })??;

        Ok(Self {
            release_tx,
            join_handle,
        })
    }

    fn release(self) {
        let _ = self.release_tx.send(());
        let _ = self.join_handle.join();
    }
}

#[cfg(windows)]
pub(crate) fn acquire_suite_server_lock_handle_for_tests() -> TestResult<usize> {
    acquire_windows_suite_server_mutex_handle()
}

#[cfg(windows)]
pub(crate) fn release_suite_server_lock_handle_for_tests(handle: usize) {
    release_windows_suite_server_mutex_handle(handle);
}

#[cfg(windows)]
pub(crate) fn close_suite_server_lock_handle_for_tests(handle: usize) {
    close_windows_suite_server_mutex_handle(handle);
}

#[cfg(windows)]
pub(crate) struct SuiteServerLockToken {
    owner: Option<WindowsSuiteServerMutexOwner>,
}

#[cfg(not(windows))]
pub(crate) struct SuiteServerLockToken;

#[cfg(windows)]
fn acquire_suite_server_lock() -> TestResult<SuiteServerLockToken> {
    Ok(SuiteServerLockToken {
        owner: Some(WindowsSuiteServerMutexOwner::start()?),
    })
}

#[cfg(not(windows))]
fn acquire_suite_server_lock() -> TestResult<SuiteServerLockToken> {
    Ok(SuiteServerLockToken)
}

#[cfg(windows)]
pub(crate) fn acquire_suite_server_lock_for_tests() -> TestResult<SuiteServerLockToken> {
    acquire_suite_server_lock()
}

#[cfg(windows)]
impl Drop for SuiteServerLockToken {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.take() {
            owner.release();
        }
    }
}

#[cfg(not(windows))]
impl Drop for SuiteServerLockToken {
    fn drop(&mut self) {}
}

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
    let normalized = normalize_output_bundle_paths(&normalize_newlines(text.to_string()));
    let mut stripped = strip_trailing_prompt(&normalized);
    while stripped.ends_with('\n') {
        stripped.pop();
    }
    if let Some(rest) = stripped.strip_prefix("\nstderr:") {
        stripped = format!("stderr:{rest}");
    }
    stripped
}

fn normalize_output_bundle_paths(text: &str) -> String {
    static OUTPUT_BUNDLE_PATH_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = OUTPUT_BUNDLE_PATH_RE.get_or_init(|| {
        Regex::new(
            r#"(?x)
            (?:[A-Za-z]:)?(?:[/\\][^\s\]"')]+)*[/\\]
            mcp-repl-output(?:-[A-Za-z0-9]+)?[/\\]
            output-\d{4}[/\\]
            (?:transcript\.txt|events\.log)
        "#,
        )
        .expect("output bundle path regex")
    });
    re.replace_all(text, |captures: &regex_lite::Captures<'_>| {
        let matched = captures.get(0).expect("full match").as_str();
        let leaf = if matched.ends_with("events.log") {
            "events.log"
        } else {
            "transcript.txt"
        };
        format!("<mcp-repl-output>/output-0001/{leaf}")
    })
    .into_owned()
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

pub(crate) fn result_text(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|item| match &item.raw {
            RawContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

pub(crate) fn backend_unavailable(text: &str) -> bool {
    text.contains("Fatal error: cannot create 'R_TempDir'")
        || text.contains("failed to start R session")
        || text.contains("worker exited with status")
        || text.contains("worker exited with signal")
        || text.contains("worker io error: Broken pipe")
        || text.contains("unable to initialize the JIT")
        || text.contains("libR.so: cannot open shared object file")
        || text.contains("options(\"defaultPackages\") was not found")
}

pub(crate) fn is_busy_response(text: &str) -> bool {
    text.contains("<<repl status: busy")
        || text.contains("worker is busy")
        || text.contains("request already running")
        || text.contains("input discarded while worker busy")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BusyResponseKind {
    NotBusy,
    Running,
    RejectedInput,
}

fn busy_response_kind(text: &str) -> BusyResponseKind {
    if text.contains("input discarded while worker busy")
        || text.contains("worker is busy")
        || text.contains("request already running")
    {
        BusyResponseKind::RejectedInput
    } else if text.contains("<<repl status: busy") {
        BusyResponseKind::Running
    } else {
        BusyResponseKind::NotBusy
    }
}

pub(crate) async fn wait_until_not_busy(
    session: &mut McpTestSession,
    initial: rmcp::model::CallToolResult,
    poll_interval: std::time::Duration,
    timeout: std::time::Duration,
) -> TestResult<rmcp::model::CallToolResult> {
    let mut result = initial;
    let mut text = result_text(&result);
    if busy_response_kind(&text) != BusyResponseKind::Running {
        return Ok(result);
    }

    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(poll_interval).await;
        let next = session
            .write_stdin_raw_unterminated_with("", Some(2.0))
            .await?;
        text = result_text(&next);
        result = next;
        if busy_response_kind(&text) != BusyResponseKind::Running {
            return Ok(result);
        }
    }

    Err(format!("worker remained busy after polling: {text:?}").into())
}

pub(crate) async fn wait_until_ready_with_input_retry(
    session: &mut McpTestSession,
    input: &str,
    initial: rmcp::model::CallToolResult,
    attempt_timeout_secs: f64,
    poll_interval: std::time::Duration,
    timeout: std::time::Duration,
) -> TestResult<rmcp::model::CallToolResult> {
    let mut result = initial;
    let mut text = result_text(&result);
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        match busy_response_kind(&text) {
            BusyResponseKind::NotBusy => return Ok(result),
            BusyResponseKind::Running => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(format!("worker remained busy after polling: {text:?}").into());
                }
                tokio::time::sleep(poll_interval).await;
                result = session
                    .write_stdin_raw_unterminated_with("", Some(2.0))
                    .await?;
            }
            // Re-send the original command when the server reported that it dropped the input.
            BusyResponseKind::RejectedInput => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(format!("worker kept rejecting input while busy: {text:?}").into());
                }
                tokio::time::sleep(poll_interval).await;
                result = session
                    .write_stdin_raw_with(input, Some(attempt_timeout_secs))
                    .await?;
            }
        }
        text = result_text(&result);
    }
}

pub(crate) async fn assert_eventually_contains(
    session: &mut McpTestSession,
    input: &str,
    expected: &str,
    label: &str,
    attempt_timeout_secs: f64,
    timeout: std::time::Duration,
    poll_interval: std::time::Duration,
) -> TestResult<bool> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last_text = String::new();
    while tokio::time::Instant::now() < deadline {
        let result = session
            .write_stdin_raw_with(input, Some(attempt_timeout_secs))
            .await?;
        last_text = result_text(&result);
        if backend_unavailable(&last_text) {
            return Ok(false);
        }
        if last_text.contains(expected) {
            return Ok(true);
        }
        tokio::time::sleep(poll_interval).await;
    }
    eprintln!("{label} did not stabilize: {last_text}");
    Ok(false)
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

        let request = match arguments_for_mcp {
            Some(arguments) => CallToolRequestParams::new(request_tool).with_arguments(arguments),
            None => CallToolRequestParams::new(request_tool),
        };

        let result = self.service.call_tool(request).await;

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
        &self,
        tool: impl Into<String>,
        arguments: Value,
    ) -> Result<rmcp::model::CallToolResult, ServiceError> {
        self.call_tool_raw_with_meta(tool, arguments, None).await
    }

    pub async fn call_tool_raw_with_meta(
        &self,
        tool: impl Into<String>,
        arguments: Value,
        meta: Option<Value>,
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
        let mut request = match arguments {
            Some(arguments) => CallToolRequestParams::new(request_tool).with_arguments(arguments),
            None => CallToolRequestParams::new(request_tool),
        };
        if let Some(meta) = meta {
            let Value::Object(meta_map) = meta else {
                return Err(ServiceError::McpError(rmcp::ErrorData::invalid_params(
                    "tool metadata must be a JSON object",
                    None,
                )));
            };
            request.meta = Some(Meta(meta_map.into_iter().collect()));
        }

        self.service.call_tool(request).await
    }

    pub async fn write_stdin_raw_with(
        &self,
        input: impl Into<String>,
        timeout: Option<f64>,
    ) -> Result<rmcp::model::CallToolResult, ServiceError> {
        self.write_stdin_raw_with_meta(input, timeout, None).await
    }

    pub async fn write_stdin_raw_with_meta(
        &self,
        input: impl Into<String>,
        timeout: Option<f64>,
        meta: Option<Value>,
    ) -> Result<rmcp::model::CallToolResult, ServiceError> {
        let mut input = input.into();
        if !input.is_empty() && !input.ends_with('\n') {
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
        self.call_tool_raw_with_meta(self.repl_tool_name(), Value::Object(args), meta)
            .await
    }

    pub async fn write_stdin_raw_unterminated_with(
        &self,
        input: impl Into<String>,
        timeout: Option<f64>,
    ) -> Result<rmcp::model::CallToolResult, ServiceError> {
        self.write_stdin_raw_unterminated_with_meta(input, timeout, None)
            .await
    }

    pub async fn write_stdin_raw_unterminated_with_meta(
        &self,
        input: impl Into<String>,
        timeout: Option<f64>,
        meta: Option<Value>,
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
        self.call_tool_raw_with_meta(self.repl_tool_name(), Value::Object(args), meta)
            .await
    }

    pub async fn send_custom_request(
        &self,
        method: impl Into<String>,
        params: Value,
    ) -> Result<(), ServiceError> {
        let request = ClientRequest::CustomRequest(CustomRequest::new(method, Some(params)));
        let _ = self.service.send_request(request).await?;
        Ok(())
    }

    pub async fn send_custom_notification(
        &self,
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

    pub async fn files_session<F>(&mut self, name: impl Into<String>, f: F) -> TestResult<()>
    where
        F: for<'a> FnOnce(
            &'a mut McpTestSession,
        )
            -> Pin<Box<dyn std::future::Future<Output = TestResult<()>> + Send + 'a>>,
    {
        let name = name.into();
        let mut session = spawn_server_with_files().await?;
        f(&mut session).await?;
        let steps = session.steps.clone();
        session.cancel().await?;
        self.sessions.push((name, steps));
        Ok(())
    }

    pub async fn python_help_files_session<F>(
        &mut self,
        name: impl Into<String>,
        f: F,
    ) -> TestResult<()>
    where
        F: for<'a> FnOnce(
            &'a mut McpTestSession,
        )
            -> Pin<Box<dyn std::future::Future<Output = TestResult<()>> + Send + 'a>>,
    {
        let name = name.into();
        let mut session =
            spawn_python_server_with_files_env_vars(python_plain_help_env_vars()).await?;
        f(&mut session).await?;
        let steps = session.steps.clone();
        session.cancel().await?;
        self.sessions.push((name, steps));
        Ok(())
    }

    pub async fn pager_session<F>(
        &mut self,
        name: impl Into<String>,
        page_chars: u64,
        f: F,
    ) -> TestResult<()>
    where
        F: for<'a> FnOnce(
            &'a mut McpTestSession,
        )
            -> Pin<Box<dyn std::future::Future<Output = TestResult<()>> + Send + 'a>>,
    {
        let name = name.into();
        let mut session = spawn_server_with_pager_page_chars(page_chars).await?;
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
                let response = normalize_snapshot_response(&step.response, &step.call);
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

                let response = normalize_snapshot_response(&step.response, &step.call);
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

fn normalize_snapshot_response(
    response: &SnapshotResponse,
    call: &SnapshotCall,
) -> SnapshotResponse {
    match response {
        SnapshotResponse::ToolResult(result) => {
            let mut content = result
                .content
                .iter()
                .map(normalize_snapshot_content)
                .collect::<Vec<_>>();
            maybe_drop_settled_prompt_echo(&mut content, call);
            SnapshotResponse::ToolResult(SnapshotCallToolResult {
                is_error: result.is_error,
                content,
            })
        }
        SnapshotResponse::ServiceError(err) => SnapshotResponse::ServiceError(err.clone()),
    }
}

fn maybe_drop_settled_prompt_echo(content: &mut Vec<SnapshotContent>, call: &SnapshotCall) {
    if !is_repl_tool_name(&call.tool) {
        return;
    }

    let Some(Value::Object(args)) = &call.arguments else {
        return;
    };
    let Some(Value::String(input)) = args.get("input") else {
        return;
    };
    let mut input_lines = split_input_lines(input).into_iter();
    let Some(input_line) = input_lines.next() else {
        return;
    };
    if input_lines.next().is_some() {
        return;
    }

    let has_prompt_only = content.iter().any(|item| match item {
        SnapshotContent::Text { text } => prompt_only_snapshot_text(text),
        _ => false,
    });
    let has_stderr = content.iter().any(|item| match item {
        SnapshotContent::Text { text } => text.contains("stderr:"),
        _ => false,
    });
    if !(has_prompt_only && has_stderr) {
        return;
    }

    content.retain(|item| match item {
        SnapshotContent::Text { text } => !prompt_echo_matches_input(text, &input_line),
        _ => true,
    });
}

fn prompt_only_snapshot_text(text: &str) -> bool {
    text.lines().all(|line| {
        if line.is_empty() {
            return true;
        }
        strip_prompt_prefix(line)
            .map(|rest| rest.trim().is_empty())
            .unwrap_or(false)
    })
}

fn prompt_echo_matches_input(text: &str, input_line: &str) -> bool {
    let mut lines = text.lines();
    let Some(line) = lines.next() else {
        return false;
    };
    if lines.next().is_some() {
        return false;
    }
    strip_prompt_prefix(line)
        .map(|rest| rest == input_line)
        .unwrap_or(false)
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
    let text = normalize_busy_timeout_elapsed_ms(text);
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
    for line in stdout_lines.into_iter().chain(stderr_lines) {
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
    spawn_server_with_args_env(Vec::new(), Vec::new()).await
}

pub async fn spawn_server_with_files() -> TestResult<McpTestSession> {
    spawn_server_with_args(vec!["--oversized-output".to_string(), "files".to_string()]).await
}

pub async fn spawn_server_with_pager_page_chars(page_bytes: u64) -> TestResult<McpTestSession> {
    spawn_server_with_args_env_and_pager_page_chars(Vec::new(), Vec::new(), page_bytes).await
}

pub async fn spawn_server_with_env_vars(
    env_vars: Vec<(String, String)>,
) -> TestResult<McpTestSession> {
    spawn_server_with_args_env(Vec::new(), env_vars).await
}

pub async fn spawn_server_with_files_env_vars(
    env_vars: Vec<(String, String)>,
) -> TestResult<McpTestSession> {
    spawn_server_with_args_env(
        vec!["--oversized-output".to_string(), "files".to_string()],
        env_vars,
    )
    .await
}

pub async fn spawn_server_with_args(args: Vec<String>) -> TestResult<McpTestSession> {
    spawn_server_with_args_env(args, Vec::new()).await
}

pub async fn spawn_python_server_with_files() -> TestResult<McpTestSession> {
    spawn_python_server_with_files_env_vars(Vec::new()).await
}

pub async fn spawn_python_server_with_files_env_vars(
    env_vars: Vec<(String, String)>,
) -> TestResult<McpTestSession> {
    spawn_server_with_args_env(
        vec![
            "--interpreter".to_string(),
            "python".to_string(),
            "--oversized-output".to_string(),
            "files".to_string(),
            "--sandbox".to_string(),
            "danger-full-access".to_string(),
        ],
        env_vars,
    )
    .await
}

pub fn python_plain_help_env_vars() -> Vec<(String, String)> {
    vec![
        ("PAGER".to_string(), String::new()),
        ("MANPAGER".to_string(), String::new()),
        ("TERM".to_string(), "dumb".to_string()),
    ]
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
    spawn_server_with_args_env_and_cwd_and_pager_page_chars(args, env_vars, None, page_bytes).await
}

pub async fn spawn_server_with_args_env_and_cwd_and_pager_page_chars(
    args: Vec<String>,
    env_vars: Vec<(String, String)>,
    cwd: Option<PathBuf>,
    page_bytes: u64,
) -> TestResult<McpTestSession> {
    let mut args = args;
    args.push("--oversized-output".to_string());
    args.push("pager".to_string());
    let mut env_vars = env_vars;
    env_vars.push((
        "MCP_REPL_PAGER_PAGE_CHARS".to_string(),
        page_bytes.to_string(),
    ));
    spawn_server_with_args_env_and_cwd(args, env_vars, cwd).await
}

pub async fn spawn_server_with_args_env(
    args: Vec<String>,
    env_vars: Vec<(String, String)>,
) -> TestResult<McpTestSession> {
    spawn_server_with_args_env_and_cwd(args, env_vars, None).await
}

pub async fn spawn_server_with_args_env_and_cwd(
    args: Vec<String>,
    env_vars: Vec<(String, String)>,
    cwd: Option<PathBuf>,
) -> TestResult<McpTestSession> {
    let suite_lock = acquire_suite_server_lock()?;
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
    let current_dir = cwd;
    let transport = TokioChildProcess::new(Command::new(exe).configure(|cmd| {
        cmd.env_remove("R_PROFILE_USER");
        cmd.env_remove("R_PROFILE_SITE");
        cmd.env_remove("R_ENVIRON");
        cmd.env_remove("R_ENVIRON_USER");
        cmd.env_remove("MCP_REPL_UPDATE_PLOT_IMAGES");
        if let Some(cwd) = &current_dir {
            cmd.current_dir(cwd);
        }
        cmd.args(&args);
        for (key, value) in &env_vars {
            cmd.env(key, value);
        }
    }))?;

    let server_pid = transport.id();
    let service = TestClient.serve(transport).await?;
    drop(suite_lock);
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
        if arg == "--interpreter" {
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
    }
    TestBackend::R
}

fn resolve_server_path() -> TestResult<PathBuf> {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_mcp-repl") {
        return Ok(PathBuf::from(path));
    }

    let mut path = std::env::current_exe()?;
    path.pop();
    path.pop();
    {
        let candidate = "mcp-repl";
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
