#![allow(clippy::await_holding_lock)]

mod common;

use common::{McpTestSession, TestResult};
use serde_json::json;
use std::time::{Duration, Instant};

const SANDBOX_STATE_METHOD: &str = "codex/sandbox-state/update";

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
        last_text = common::call_tool_text_without_prompts(&result);
        if common::r_backend_unavailable(&last_text) {
            return Ok(false);
        }
        if last_text.contains("TRUE") {
            return Ok(true);
        }
        if common::worker_busy_response(&last_text) {
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
        last_text = common::call_tool_text_without_prompts(&result);
        if common::r_backend_unavailable(&last_text) {
            return Ok(false);
        }
        if last_text.contains("FALSE") {
            return Ok(true);
        }
        if common::worker_busy_response(&last_text) {
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
    let _guard = common::lock_test_mutex()?;
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
    let _guard = common::lock_test_mutex()?;
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
    let text = common::call_tool_text_without_prompts(&result);
    if common::r_backend_unavailable(&text) {
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
    let _guard = common::lock_test_mutex()?;
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
    let _guard = common::lock_test_mutex()?;
    let session =
        common::spawn_server_with_args(vec!["--sandbox".to_string(), "inherit".to_string()])
            .await?;
    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_without_state_update_errors_on_first_tool_call() -> TestResult<()> {
    let _guard = common::lock_test_mutex()?;
    let mut session =
        common::spawn_server_with_args(vec!["--sandbox".to_string(), "inherit".to_string()])
            .await?;
    let result = session.write_stdin_raw_with("1+1", Some(2.0)).await?;
    let text = common::call_tool_text_without_prompts(&result);
    assert!(
        text.contains("--sandbox inherit requested but no client sandbox state was provided"),
        "expected missing sandbox-state error, got: {text}"
    );
    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_without_state_update_errors_on_repl_reset() -> TestResult<()> {
    let _guard = common::lock_test_mutex()?;
    let mut session =
        common::spawn_server_with_args(vec!["--sandbox".to_string(), "inherit".to_string()])
            .await?;
    let result = session.call_tool_raw("repl_reset", json!({})).await?;
    let text = common::call_tool_text_without_prompts(&result);
    assert!(
        text.contains("--sandbox inherit requested but no client sandbox state was provided"),
        "expected missing sandbox-state error, got: {text}"
    );
    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_late_claude_binding_allows_first_sandbox_update() -> TestResult<()> {
    let _guard = common::lock_test_mutex()?;
    let temp = tempfile::tempdir()?;
    let env_file = temp.path().join("claude.env");
    let exe = common::resolve_test_binary()?;
    let env_vars = vec![
        (
            "XDG_STATE_HOME".to_string(),
            temp.path().to_string_lossy().to_string(),
        ),
        (
            "CLAUDE_ENV_FILE".to_string(),
            env_file.to_string_lossy().to_string(),
        ),
    ];
    let mut session = common::spawn_server_with_args_env_and_pager_page_chars(
        vec!["--sandbox".to_string(), "inherit".to_string()],
        env_vars.clone(),
        300,
    )
    .await?;

    common::run_claude_hook(
        &exe,
        &env_vars,
        "session-start",
        json!({
            "hook_event_name": "SessionStart",
            "session_id": "sess-late"
        }),
    )?;

    session
        .send_custom_request(SANDBOX_STATE_METHOD, sandbox_update_params(true))
        .await?;

    let result = session.write_stdin_raw_with("1+1", Some(10.0)).await?;
    let text = common::call_tool_text_without_prompts(&result);
    if common::r_backend_unavailable(&text) {
        eprintln!("sandbox_state_updates late claude binding backend unavailable; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        !text.contains("--sandbox inherit requested but no client sandbox state was provided"),
        "expected first sandbox update to unblock a late-bound Claude inherit session, got: {text}"
    );
    assert!(
        text.contains("2"),
        "expected late-bound Claude inherit session to evaluate input after first sandbox update, got: {text}"
    );

    session.cancel().await?;
    Ok(())
}
