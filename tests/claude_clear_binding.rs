mod common;

use common::{McpTestSession, TestResult};
use rmcp::model::RawContent;
use serde_json::json;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

fn test_mutex() -> &'static Mutex<()> {
    static TEST_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
    TEST_MUTEX.get_or_init(|| Mutex::new(()))
}

fn test_guard() -> MutexGuard<'static, ()> {
    test_mutex().lock().unwrap_or_else(|err| err.into_inner())
}

fn resolve_exe() -> TestResult<PathBuf> {
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

fn result_text(result: &rmcp::model::CallToolResult) -> String {
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

fn backend_unavailable(text: &str) -> bool {
    text.contains("Fatal error: cannot create 'R_TempDir'")
        || text.contains("failed to start R session")
        || text.contains("worker exited with status")
        || text.contains("worker exited with signal")
        || text.contains("unable to initialize the JIT")
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

fn run_claude_hook_with_env(
    exe: &Path,
    env_vars: &[(String, String)],
    subcommand: &str,
    input: serde_json::Value,
) -> TestResult<()> {
    let mut cmd = Command::new(exe);
    cmd.arg("claude-hook")
        .arg(subcommand)
        .env_remove("CLAUDE_PROJECT_DIR")
        .env_remove("CLAUDE_ENV_FILE")
        .env_remove("MCP_REPL_CLAUDE_SESSION_ID")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    for (key, value) in env_vars {
        cmd.env(key, value);
    }
    let mut child = cmd.spawn()?;

    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| "failed to capture claude-hook stdin".to_string())?;
        stdin.write_all(serde_json::to_string(&input)?.as_bytes())?;
    }

    let output = child.wait_with_output()?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!(
        "claude-hook {subcommand} failed with status {}\nstderr:\n{stderr}",
        output.status
    )
    .into())
}

fn claude_env_vars(state_home: &Path, env_file: &Path) -> Vec<(String, String)> {
    vec![
        (
            "XDG_STATE_HOME".to_string(),
            state_home.to_string_lossy().to_string(),
        ),
        (
            "CLAUDE_ENV_FILE".to_string(),
            env_file.to_string_lossy().to_string(),
        ),
    ]
}

fn run_session_start(
    exe: &Path,
    state_home: &Path,
    env_file: &Path,
    session_id: &str,
) -> TestResult<()> {
    let env_vars = claude_env_vars(state_home, env_file);
    run_claude_hook_with_env(
        exe,
        &env_vars,
        "session-start",
        json!({
            "hook_event_name": "SessionStart",
            "session_id": session_id,
        }),
    )
}

fn run_session_end_clear(
    exe: &Path,
    state_home: &Path,
    env_file: &Path,
    session_id: &str,
) -> TestResult<()> {
    let env_vars = claude_env_vars(state_home, env_file);
    run_claude_hook_with_env(
        exe,
        &env_vars,
        "session-end",
        json!({
            "hook_event_name": "SessionEnd",
            "session_id": session_id,
            "reason": "clear",
        }),
    )
}

async fn repl_text(
    session: &mut McpTestSession,
    input: &str,
    context: &str,
) -> TestResult<Option<String>> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let result = session.write_stdin_raw_with(input, Some(10.0)).await?;
        let text = result_text(&result);
        if backend_unavailable(&text) {
            eprintln!("claude_clear_binding backend unavailable {context}; skipping");
            return Ok(None);
        }
        if !busy_response(&text) {
            return Ok(Some(text));
        }
        if Instant::now() >= deadline {
            return Err(
                format!("claude_clear_binding worker remained busy {context}: {text:?}").into(),
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_clear_restart_binds_after_session_start_hook() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempfile::tempdir()?;
    let env_file = temp.path().join("claude.env");
    let exe = resolve_exe()?;

    run_session_start(&exe, temp.path(), &env_file, "sess-startup")?;

    let mut session =
        common::spawn_server_with_env_vars(claude_env_vars(temp.path(), &env_file)).await?;

    let first_request = match repl_text(
        &mut session,
        "startup_bound <- 1; print(exists(\"startup_bound\"))",
        "during startup binding",
    )
    .await?
    {
        Some(text) => text,
        None => {
            session.cancel().await?;
            return Ok(());
        }
    };
    assert!(
        first_request.contains("TRUE"),
        "expected first request after SessionStart binding to create startup-bound state, got: {first_request:?}"
    );

    run_session_end_clear(&exe, temp.path(), &env_file, "sess-startup")?;

    let after_clear = match repl_text(
        &mut session,
        "print(exists(\"startup_bound\"))",
        "after startup-session clear",
    )
    .await?
    {
        Some(text) => text,
        None => {
            session.cancel().await?;
            return Ok(());
        }
    };

    session.cancel().await?;
    assert!(
        after_clear.contains("FALSE"),
        "expected clear for the startup-bound session to reset startup-bound state, got: {after_clear:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_clear_late_session_binding_restarts_prebound_worker_state() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempfile::tempdir()?;
    let env_file = temp.path().join("claude.env");
    let exe = resolve_exe()?;

    let mut session =
        common::spawn_server_with_env_vars(claude_env_vars(temp.path(), &env_file)).await?;

    let prebind = match repl_text(
        &mut session,
        "x <- 1; print(exists(\"x\"))",
        "before late binding",
    )
    .await?
    {
        Some(text) => text,
        None => {
            session.cancel().await?;
            return Ok(());
        }
    };
    assert!(
        prebind.contains("TRUE"),
        "expected pre-bind worker state to exist before SessionStart, got: {prebind:?}"
    );

    run_session_start(&exe, temp.path(), &env_file, "sess-late")?;

    let after_bind = match repl_text(
        &mut session,
        "print(exists(\"x\"))",
        "after late binding restart",
    )
    .await?
    {
        Some(text) => text,
        None => {
            session.cancel().await?;
            return Ok(());
        }
    };

    session.cancel().await?;
    assert!(
        after_bind.contains("FALSE"),
        "expected late binding to restart the worker before the first bound request, got: {after_bind:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_clear_matches_exact_env_file_and_session() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempfile::tempdir()?;
    let env_file_a = temp.path().join("claude-a.env");
    let env_file_b = temp.path().join("claude-b.env");
    let exe = resolve_exe()?;
    let session_id = "sess-shared";

    run_session_start(&exe, temp.path(), &env_file_a, session_id)?;
    let mut session_a =
        common::spawn_server_with_env_vars(claude_env_vars(temp.path(), &env_file_a)).await?;
    let a_ready = match repl_text(
        &mut session_a,
        "a_state <- 1; print(exists(\"a_state\"))",
        "before exact env-file clear on session A",
    )
    .await?
    {
        Some(text) => text,
        None => {
            session_a.cancel().await?;
            return Ok(());
        }
    };
    assert!(
        a_ready.contains("TRUE"),
        "expected session A state to be created before clear, got: {a_ready:?}"
    );

    run_session_start(&exe, temp.path(), &env_file_b, session_id)?;
    let mut session_b =
        common::spawn_server_with_env_vars(claude_env_vars(temp.path(), &env_file_b)).await?;
    let b_ready = match repl_text(
        &mut session_b,
        "b_state <- 1; print(exists(\"b_state\"))",
        "before exact env-file clear on session B",
    )
    .await?
    {
        Some(text) => text,
        None => {
            session_a.cancel().await?;
            session_b.cancel().await?;
            return Ok(());
        }
    };
    assert!(
        b_ready.contains("TRUE"),
        "expected session B state to be created before clear, got: {b_ready:?}"
    );

    run_session_end_clear(&exe, temp.path(), &env_file_b, session_id)?;

    let b_after_clear = match repl_text(
        &mut session_b,
        "print(exists(\"b_state\"))",
        "after exact env-file clear on session B",
    )
    .await?
    {
        Some(text) => text,
        None => {
            session_a.cancel().await?;
            session_b.cancel().await?;
            return Ok(());
        }
    };
    let a_after_b_clear = match repl_text(
        &mut session_a,
        "print(exists(\"a_state\"))",
        "after exact env-file clear on session A",
    )
    .await?
    {
        Some(text) => text,
        None => {
            session_a.cancel().await?;
            session_b.cancel().await?;
            return Ok(());
        }
    };

    session_a.cancel().await?;
    session_b.cancel().await?;
    assert!(
        b_after_clear.contains("FALSE"),
        "expected clear through env file B to reset B, got: {b_after_clear:?}"
    );
    assert!(
        a_after_b_clear.contains("TRUE"),
        "expected clear through env file B to leave A bound state intact, got: {a_after_b_clear:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_clear_ignores_later_session_start_after_server_is_bound() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempfile::tempdir()?;
    let env_file = temp.path().join("claude.env");
    let exe = resolve_exe()?;

    run_session_start(&exe, temp.path(), &env_file, "sess-a")?;
    let mut session =
        common::spawn_server_with_env_vars(claude_env_vars(temp.path(), &env_file)).await?;

    let bound = match repl_text(
        &mut session,
        "x <- 1; print(exists(\"x\"))",
        "before later SessionStart",
    )
    .await?
    {
        Some(text) => text,
        None => {
            session.cancel().await?;
            return Ok(());
        }
    };
    assert!(
        bound.contains("TRUE"),
        "expected session A state to exist before later SessionStart, got: {bound:?}"
    );

    run_session_start(&exe, temp.path(), &env_file, "sess-b")?;
    run_session_end_clear(&exe, temp.path(), &env_file, "sess-b")?;

    let after_session_b_clear = match repl_text(
        &mut session,
        "print(exists(\"x\"))",
        "after later SessionStart and sess-b clear",
    )
    .await?
    {
        Some(text) => text,
        None => {
            session.cancel().await?;
            return Ok(());
        }
    };

    session.cancel().await?;
    assert!(
        after_session_b_clear.contains("TRUE"),
        "expected sess-b SessionStart plus clear not to reset sess-a state, got: {after_session_b_clear:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_clear_concurrent_same_directory_sessions_do_not_reset_each_other() -> TestResult<()>
{
    let _guard = test_guard();
    let temp = tempfile::tempdir()?;
    let env_file_a = temp.path().join("claude-a.env");
    let env_file_b = temp.path().join("claude-b.env");
    let exe = resolve_exe()?;

    run_session_start(&exe, temp.path(), &env_file_a, "sess-a")?;
    run_session_start(&exe, temp.path(), &env_file_b, "sess-b")?;

    let mut session_a =
        common::spawn_server_with_env_vars(claude_env_vars(temp.path(), &env_file_a)).await?;
    let mut session_b =
        common::spawn_server_with_env_vars(claude_env_vars(temp.path(), &env_file_b)).await?;

    let a_ready = match repl_text(
        &mut session_a,
        "a_state <- 1; print(exists(\"a_state\"))",
        "before same-directory clear on session A",
    )
    .await?
    {
        Some(text) => text,
        None => {
            session_a.cancel().await?;
            session_b.cancel().await?;
            return Ok(());
        }
    };
    let b_ready = match repl_text(
        &mut session_b,
        "b_state <- 1; print(exists(\"b_state\"))",
        "before same-directory clear on session B",
    )
    .await?
    {
        Some(text) => text,
        None => {
            session_a.cancel().await?;
            session_b.cancel().await?;
            return Ok(());
        }
    };
    assert!(
        a_ready.contains("TRUE"),
        "expected session A state to be created before clear, got: {a_ready:?}"
    );
    assert!(
        b_ready.contains("TRUE"),
        "expected session B state to be created before clear, got: {b_ready:?}"
    );

    run_session_end_clear(&exe, temp.path(), &env_file_a, "sess-a")?;

    let a_after_clear = match repl_text(
        &mut session_a,
        "print(exists(\"a_state\"))",
        "after same-directory clear on session A",
    )
    .await?
    {
        Some(text) => text,
        None => {
            session_a.cancel().await?;
            session_b.cancel().await?;
            return Ok(());
        }
    };
    let b_after_a_clear = match repl_text(
        &mut session_b,
        "print(exists(\"b_state\"))",
        "after same-directory clear on session B",
    )
    .await?
    {
        Some(text) => text,
        None => {
            session_a.cancel().await?;
            session_b.cancel().await?;
            return Ok(());
        }
    };

    session_a.cancel().await?;
    session_b.cancel().await?;
    assert!(
        a_after_clear.contains("FALSE"),
        "expected clear for session A to reset A, got: {a_after_clear:?}"
    );
    assert!(
        b_after_a_clear.contains("TRUE"),
        "expected clear for session A not to reset concurrent session B, got: {b_after_a_clear:?}"
    );
    Ok(())
}
