#![allow(clippy::await_holding_lock)]

mod common;

use common::{McpTestSession, TestResult};
use rmcp::model::{CallToolResult, RawContent};
use serde_json::json;
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tempfile::tempdir;
use tokio::time::sleep;

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

fn result_text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|content| match &content.raw {
            RawContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn disclosed_path(text: &str, suffix: &str) -> Option<PathBuf> {
    let end = text.find(suffix)?.saturating_add(suffix.len());
    let start = text[..end]
        .rfind(|ch: char| ch.is_whitespace() || matches!(ch, '"' | '\'' | '[' | '('))
        .map_or(0, |idx| idx.saturating_add(1));
    Some(PathBuf::from(&text[start..end]))
}

fn bundle_transcript_path(text: &str) -> Option<PathBuf> {
    disclosed_path(text, "transcript.txt")
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
    text.contains("<<repl status: busy")
        || text.contains("worker is busy")
        || text.contains("request already running")
        || text.contains("input discarded while worker busy")
}

async fn spawn_server_retry() -> TestResult<common::McpTestSession> {
    spawn_server_retry_with_env_vars(Vec::new()).await
}

async fn spawn_server_retry_with_env_vars(
    env_vars: Vec<(String, String)>,
) -> TestResult<common::McpTestSession> {
    let mut last_error: Option<Box<dyn std::error::Error + Send + Sync>> = None;
    for _ in 0..3 {
        match common::spawn_server_with_env_vars(env_vars.clone()).await {
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

async fn spawn_server_with_files_retry_with_env_vars(
    env_vars: Vec<(String, String)>,
) -> TestResult<common::McpTestSession> {
    let mut last_error: Option<Box<dyn std::error::Error + Send + Sync>> = None;
    for _ in 0..3 {
        match common::spawn_server_with_files_env_vars(env_vars.clone()).await {
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
            "failed to spawn files-mode server after temp-dir retries".to_string(),
        )
    }))
}

enum SandboxUpdateKind {
    Request,
    Notification,
}

async fn wait_for_timeout_bundle_transcript(
    session: &mut McpTestSession,
    input: &str,
) -> TestResult<Option<PathBuf>> {
    let first = session.write_stdin_raw_with(input, Some(0.05)).await?;
    let first_text = result_text(&first);
    if backend_unavailable(&first_text) {
        return Ok(None);
    }

    sleep(Duration::from_millis(260)).await;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let spilled = session
            .write_stdin_raw_unterminated_with("", Some(0.1))
            .await?;
        let spilled_text = result_text(&spilled);
        if let Some(path) = bundle_transcript_path(&spilled_text) {
            return Ok(Some(path));
        }
        if !busy_response(&spilled_text) {
            return Err(format!(
                "expected timeout bundle disclosure in spill poll, got: {spilled_text:?}"
            )
            .into());
        }
        sleep(Duration::from_millis(100)).await;
    }

    Err("timed out waiting for timeout bundle transcript".into())
}

async fn poll_until_not_busy(session: &mut McpTestSession) -> TestResult<CallToolResult> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let result = session
            .write_stdin_raw_unterminated_with("", Some(1.0))
            .await?;
        let text = result_text(&result);
        if !busy_response(&text) {
            return Ok(result);
        }
        sleep(Duration::from_millis(100)).await;
    }
    Err("timed out waiting for non-busy empty poll".into())
}

async fn assert_sandbox_update_clears_stale_timeout_bundle(
    kind: SandboxUpdateKind,
) -> TestResult<()> {
    let _guard = test_mutex()
        .lock()
        .map_err(|_| "sandbox_state_updates test mutex poisoned")?;
    if !common::sandbox_exec_available() {
        eprintln!("sandbox-exec unavailable; skipping");
        return Ok(());
    }

    let temp = tempdir()?;
    // Transcript-path assertions are part of the files-mode surface. The
    // default pager mode can satisfy the same timeout semantics without
    // deterministically disclosing a transcript path before the restart.
    let mut session = spawn_server_with_files_retry_with_env_vars(vec![(
        "TMPDIR".to_string(),
        temp.path().display().to_string(),
    )])
    .await?;
    let input = "big <- paste(rep('q', 120), collapse = ''); cat('start\\n'); flush.console(); Sys.sleep(0.2); for (i in 1:80) cat(sprintf('mid%03d %s\\n', i, big)); flush.console(); Sys.sleep(30); cat('tail\\n')";
    let Some(transcript_path) = wait_for_timeout_bundle_transcript(&mut session, input).await?
    else {
        eprintln!("sandbox_state_updates backend unavailable; skipping");
        session.cancel().await?;
        return Ok(());
    };
    let transcript_before = fs::read_to_string(&transcript_path)?;

    match kind {
        SandboxUpdateKind::Request => {
            session
                .send_custom_request(SANDBOX_STATE_METHOD, sandbox_update_params(true))
                .await?;
        }
        SandboxUpdateKind::Notification => {
            session
                .send_custom_notification(SANDBOX_STATE_METHOD, sandbox_update_params(true))
                .await?;
            sleep(Duration::from_millis(200)).await;
        }
    }

    let poll = poll_until_not_busy(&mut session).await?;
    let poll_text = result_text(&poll);
    let transcript_after = fs::read_to_string(&transcript_path)?;

    session.cancel().await?;

    assert!(
        bundle_transcript_path(&poll_text).is_none(),
        "did not expect empty poll after sandbox restart to reuse prior timeout bundle: {poll_text:?}"
    );
    assert_eq!(
        transcript_after, transcript_before,
        "did not expect sandbox-triggered restart output to append to prior timeout bundle"
    );
    Ok(())
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
async fn sandbox_state_update_request_clears_hidden_timeout_bundle() -> TestResult<()> {
    assert_sandbox_update_clears_stale_timeout_bundle(SandboxUpdateKind::Request).await
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

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_state_update_notification_clears_hidden_timeout_bundle() -> TestResult<()> {
    assert_sandbox_update_clears_stale_timeout_bundle(SandboxUpdateKind::Notification).await
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
    let target = std::env::temp_dir().join("mcp-repl-sandbox-state-update.txt");
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
async fn sandbox_inherit_without_state_update_falls_back_on_first_tool_call() -> TestResult<()> {
    let _guard = test_mutex()
        .lock()
        .map_err(|_| "sandbox_state_updates test mutex poisoned")?;
    let mut session =
        common::spawn_server_with_args(vec!["--sandbox".to_string(), "inherit".to_string()])
            .await?;
    let result = session.write_stdin_raw_with("1+1", Some(2.0)).await?;
    let text = collect_text(&result);
    if backend_unavailable(&text) {
        eprintln!("sandbox_state_updates backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        !text.contains("--sandbox inherit requested but no client sandbox state was provided"),
        "did not expect missing sandbox-state error, got: {text}"
    );
    assert!(
        text.contains("2"),
        "expected successful fallback evaluation, got: {text}"
    );
    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_without_state_update_falls_back_on_repl_reset() -> TestResult<()> {
    let _guard = test_mutex()
        .lock()
        .map_err(|_| "sandbox_state_updates test mutex poisoned")?;
    let mut session =
        common::spawn_server_with_args(vec!["--sandbox".to_string(), "inherit".to_string()])
            .await?;
    let first = session.write_stdin_raw_with("x <- 42", Some(10.0)).await?;
    let first_text = collect_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("sandbox_state_updates backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    let result = session.call_tool_raw("repl_reset", json!({})).await?;
    let text = collect_text(&result);
    assert!(
        !text.contains("--sandbox inherit requested but no client sandbox state was provided"),
        "did not expect missing sandbox-state error, got: {text}"
    );
    assert!(
        text.contains("new session started"),
        "expected repl_reset to succeed under fallback policy, got: {text}"
    );
    session.cancel().await?;
    Ok(())
}
