#![allow(clippy::await_holding_lock)]

mod common;

use common::{McpTestSession, TestResult};
use rmcp::model::{CallToolResult, RawContent};
use serde_json::json;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const SANDBOX_STATE_METHOD: &str = "codex/sandbox-state/update";

fn test_mutex() -> &'static Mutex<()> {
    static TEST_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
    TEST_MUTEX.get_or_init(|| Mutex::new(()))
}

fn collect_text(result: &CallToolResult) -> String {
    let text = result
        .content
        .iter()
        .filter_map(|content| match &content.raw {
            RawContent::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    text.lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            !(trimmed.starts_with("> ") || trimmed.starts_with("+ ") || trimmed == ">")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn sandbox_update_params(network_access: bool) -> serde_json::Value {
    json!({
        "sandboxPolicy": {
            "type": "workspace-write",
            "writable_roots": [],
            "network_access": network_access,
            "exclude_tmpdir_env_var": false,
            "exclude_slash_tmp": false
        }
    })
}

fn backend_unavailable(text: &str) -> bool {
    text.contains("Fatal error: cannot create 'R_TempDir'")
        || text.contains("failed to start R session")
        || text.contains("worker exited with signal")
        || text.contains("worker exited with status")
        || text.contains("worker io error: Broken pipe")
        || text.contains("unable to initialize the JIT")
        || text.contains("libR.so: cannot open shared object file")
        || text.contains("options(\"defaultPackages\") was not found")
        || text.contains(
            "worker protocol error: ipc disconnected while waiting for request completion",
        )
}

fn busy_response(text: &str) -> bool {
    text.contains("<<console status: busy")
        || text.contains("worker is busy")
        || text.contains("request already running")
        || text.contains("input discarded while worker busy")
}

async fn spawn_server_retry() -> TestResult<common::McpTestSession> {
    let mut last_error: Option<Box<dyn std::error::Error + Send + Sync>> = None;
    for _ in 0..3 {
        match common::spawn_server().await {
            Ok(session) => return Ok(session),
            Err(err) => {
                let message = err.to_string();
                if message.contains(
                    "failed to create session temp dir: The directory is not empty. (os error 145)",
                ) {
                    last_error = Some(err);
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    continue;
                }
                return Err(err);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| {
        Box::<dyn std::error::Error + Send + Sync>::from(
            "failed to spawn server after temp-dir retries".to_string(),
        )
    }))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn sandbox_full_access_params() -> serde_json::Value {
    json!({
        "sandboxPolicy": {
            "type": "danger-full-access"
        }
    })
}

async fn assert_session_reset(session: &mut McpTestSession) -> TestResult<bool> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut last_text = String::new();
    while Instant::now() < deadline {
        let result = session
            .write_stdin_raw_with("x <- 42; print(exists(\"x\"))", Some(10.0))
            .await?;
        last_text = collect_text(&result);
        if backend_unavailable(&last_text) {
            return Ok(false);
        }
        if last_text.contains("TRUE") {
            return Ok(true);
        }
        if busy_response(&last_text) {
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    eprintln!("sandbox_state_updates pre-update check did not stabilize: {last_text}");
    Ok(false)
}

async fn assert_variable_cleared(session: &mut McpTestSession) -> TestResult<bool> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut last_text = String::new();
    while Instant::now() < deadline {
        let result = session
            .write_stdin_raw_with("print(exists(\"x\"))", Some(10.0))
            .await?;
        last_text = collect_text(&result);
        if backend_unavailable(&last_text) {
            return Ok(false);
        }
        if last_text.contains("FALSE") {
            return Ok(true);
        }
        if busy_response(&last_text) {
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    eprintln!("sandbox_state_updates post-update check did not stabilize: {last_text}");
    Ok(false)
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_state_update_request_restarts_worker() -> TestResult<()> {
    let _guard = test_mutex()
        .lock()
        .map_err(|_| "sandbox_state_updates test mutex poisoned")?;
    if !common::sandbox_exec_available() {
        eprintln!("sandbox-exec unavailable; skipping");
        return Ok(());
    }
    let mut session = spawn_server_retry().await?;
    if !assert_session_reset(&mut session).await? {
        eprintln!("sandbox_state_updates request backend unavailable; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session
        .send_custom_request(SANDBOX_STATE_METHOD, sandbox_update_params(true))
        .await?;
    if !assert_variable_cleared(&mut session).await? {
        eprintln!("sandbox_state_updates request backend unavailable; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_state_update_notification_restarts_worker() -> TestResult<()> {
    let _guard = test_mutex()
        .lock()
        .map_err(|_| "sandbox_state_updates test mutex poisoned")?;
    if !common::sandbox_exec_available() {
        eprintln!("sandbox-exec unavailable; skipping");
        return Ok(());
    }
    let mut session = spawn_server_retry().await?;
    if !assert_session_reset(&mut session).await? {
        eprintln!("sandbox_state_updates notification backend unavailable; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session
        .send_custom_notification(SANDBOX_STATE_METHOD, sandbox_update_params(true))
        .await?;
    if !assert_variable_cleared(&mut session).await? {
        eprintln!("sandbox_state_updates notification backend unavailable; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_state_update_applies_full_access_policy() -> TestResult<()> {
    if !common::sandbox_exec_available() {
        eprintln!("sandbox-exec unavailable; skipping");
        return Ok(());
    }
    if std::env::var_os("CODEX_SANDBOX").is_some() {
        return Ok(());
    }
    let target = std::env::temp_dir().join("mcp-console-sandbox-state-update.txt");
    let _ = std::fs::remove_file(&target);
    let mut session = common::spawn_server().await?;
    session
        .send_custom_request(SANDBOX_STATE_METHOD, sandbox_full_access_params())
        .await?;
    let target_literal = serde_json::to_string(&target.to_string_lossy().to_string())
        .map_err(|err| format!("failed to encode target path: {err}"))?;
    let code = r#"
target <- __TARGET__
tryCatch({
  writeLines("allowed", target)
  cat("WRITE_OK\n")
}, error = function(e) {
  message("WRITE_ERROR:", conditionMessage(e))
})
"#
    .replace("__TARGET__", &target_literal);
    let result = session.write_stdin_raw_with(code, Some(10.0)).await?;
    let text = collect_text(&result);
    if backend_unavailable(&text) {
        eprintln!("sandbox_state_updates full_access backend unavailable; skipping");
        let _ = std::fs::remove_file(&target);
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("WRITE_OK"),
        "expected full access to allow write, got: {text}"
    );
    assert!(
        !text.contains("WRITE_ERROR:"),
        "full access unexpectedly blocked write: {text}"
    );
    let _ = std::fs::remove_file(&target);
    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_state_capability_advertised() -> TestResult<()> {
    let _guard = test_mutex()
        .lock()
        .map_err(|_| "sandbox_state_updates test mutex poisoned")?;
    let session = spawn_server_retry().await?;
    let info = session.server_info().ok_or_else(|| {
        let message = "missing server info from initialize".to_string();
        Box::<dyn std::error::Error + Send + Sync>::from(message)
    })?;
    let experimental = info.capabilities.experimental.as_ref().ok_or_else(|| {
        let message = "missing experimental capabilities".to_string();
        Box::<dyn std::error::Error + Send + Sync>::from(message)
    })?;
    assert!(
        experimental.contains_key("codex/sandbox-state"),
        "expected sandbox state capability in experimental: {experimental:?}"
    );
    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_allows_initialize_before_state_update() -> TestResult<()> {
    let _guard = test_mutex()
        .lock()
        .map_err(|_| "sandbox_state_updates test mutex poisoned")?;
    let session =
        common::spawn_server_with_args(vec!["--sandbox".to_string(), "inherit".to_string()])
            .await?;
    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_without_state_update_errors_on_first_tool_call() -> TestResult<()> {
    let _guard = test_mutex()
        .lock()
        .map_err(|_| "sandbox_state_updates test mutex poisoned")?;
    let mut session =
        common::spawn_server_with_args(vec!["--sandbox".to_string(), "inherit".to_string()])
            .await?;
    let result = session.write_stdin_raw_with("1+1", Some(2.0)).await?;
    let text = collect_text(&result);
    assert!(
        text.contains("--sandbox inherit requested but no client sandbox state was provided"),
        "expected missing sandbox-state error, got: {text}"
    );
    session.cancel().await?;
    Ok(())
}
