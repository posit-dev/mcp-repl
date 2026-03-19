mod common;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use common::{McpTestSession, TestResult};
use serde_json::json;
use std::io::Write;
use std::path::Path;
use std::process::Command;

const SESSION_ID_TOKEN_PREFIX: &str = "mcp_repl_session_id_b64_";

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

fn claude_scope_env_vars(state_home: &Path, scope_key: &str) -> Vec<(String, String)> {
    vec![
        (
            "XDG_STATE_HOME".to_string(),
            state_home.to_string_lossy().to_string(),
        ),
        (
            "MCP_REPL_CLAUDE_TEST_SCOPE_KEY".to_string(),
            scope_key.to_string(),
        ),
    ]
}

#[cfg(not(windows))]
fn source_session_id_from_env_file(env_file: &Path) -> TestResult<String> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(". \"$1\"; printf %s \"$MCP_REPL_CLAUDE_SESSION_ID\"")
        .arg("sh")
        .arg(env_file)
        .output()?;
    assert!(
        output.status.success(),
        "sourcing CLAUDE_ENV_FILE failed with status {}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let raw = String::from_utf8(output.stdout)?;
    let encoded = raw
        .strip_prefix(SESSION_ID_TOKEN_PREFIX)
        .ok_or_else(|| format!("expected session token prefix, got: {raw:?}"))?;
    let decoded = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|err| format!("failed to decode session token: {err}"))?;
    Ok(String::from_utf8(decoded)?)
}

fn run_session_start(
    exe: &Path,
    state_home: &Path,
    env_file: &Path,
    session_id: &str,
) -> TestResult<()> {
    let env_vars = claude_env_vars(state_home, env_file);
    common::run_claude_hook(
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
    common::run_claude_hook(
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

fn run_session_start_with_env(
    exe: &Path,
    env_vars: &[(String, String)],
    session_id: &str,
) -> TestResult<()> {
    common::run_claude_hook(
        exe,
        env_vars,
        "session-start",
        json!({
            "hook_event_name": "SessionStart",
            "session_id": session_id,
        }),
    )
}

fn run_session_end_clear_with_env(
    exe: &Path,
    env_vars: &[(String, String)],
    session_id: &str,
) -> TestResult<()> {
    common::run_claude_hook(
        exe,
        env_vars,
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
    common::write_stdin_until_ready(session, input, 10.0, context).await
}

#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread")]
async fn claude_session_start_writes_shell_safe_session_token() -> TestResult<()> {
    let _guard = common::lock_test_mutex()?;
    let temp = tempfile::tempdir()?;
    let env_file = temp.path().join("claude.env");
    let exe = common::resolve_test_binary()?;
    let session_id = "sess $HOME; 'quoted'\n# mcp-repl-session-id-b64=literal";

    run_session_start(&exe, temp.path(), &env_file, session_id)?;

    let sourced_session_id = source_session_id_from_env_file(&env_file)?;
    assert_eq!(
        sourced_session_id, session_id,
        "expected sourced env file to preserve the exact session id"
    );

    let mut session =
        common::spawn_server_with_env_vars(claude_env_vars(temp.path(), &env_file)).await?;

    let first_request = match repl_text(
        &mut session,
        "quoted_bound <- 1; print(exists(\"quoted_bound\"))",
        "during quoted-session binding",
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
        "expected first request after quoted SessionStart binding to create state, got: {first_request:?}"
    );

    run_session_end_clear(&exe, temp.path(), &env_file, session_id)?;

    let after_clear = match repl_text(
        &mut session,
        "print(exists(\"quoted_bound\"))",
        "after quoted-session clear",
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
        "expected clear for the quoted session to reset bound state, got: {after_clear:?}"
    );
    Ok(())
}

#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread")]
async fn claude_clear_matches_multiline_session_ids_with_plaintext_continuations() -> TestResult<()>
{
    let _guard = common::lock_test_mutex()?;
    let temp = tempfile::tempdir()?;
    let env_file = temp.path().join("claude.env");
    let exe = common::resolve_test_binary()?;
    let session_id = "sess-first-line\ncontinued session text";

    run_session_start(&exe, temp.path(), &env_file, session_id)?;

    let sourced_session_id = source_session_id_from_env_file(&env_file)?;
    assert_eq!(
        sourced_session_id, session_id,
        "expected sourced env file to preserve the multiline session id"
    );

    let mut session =
        common::spawn_server_with_env_vars(claude_env_vars(temp.path(), &env_file)).await?;

    let first_request = match repl_text(
        &mut session,
        "multiline_bound <- 1; print(exists(\"multiline_bound\"))",
        "during multiline continuation binding",
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
        "expected first request after multiline binding to create state, got: {first_request:?}"
    );

    run_session_end_clear(&exe, temp.path(), &env_file, session_id)?;

    let after_clear = match repl_text(
        &mut session,
        "print(exists(\"multiline_bound\"))",
        "after multiline continuation clear",
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
        "expected clear for the multiline session to reset bound state, got: {after_clear:?}"
    );
    Ok(())
}

#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread")]
async fn claude_clear_matches_multiline_session_ids_with_export_like_continuations()
-> TestResult<()> {
    let _guard = common::lock_test_mutex()?;
    let temp = tempfile::tempdir()?;
    let env_file = temp.path().join("claude.env");
    let exe = common::resolve_test_binary()?;
    let session_id = "sess-first-line\nexport FOO=bar\ncontinued tail";

    run_session_start(&exe, temp.path(), &env_file, session_id)?;

    let sourced_session_id = source_session_id_from_env_file(&env_file)?;
    assert_eq!(
        sourced_session_id, session_id,
        "expected sourced env file to preserve the export-like multiline session id"
    );

    let mut session =
        common::spawn_server_with_env_vars(claude_env_vars(temp.path(), &env_file)).await?;

    let first_request = match repl_text(
        &mut session,
        "export_like_bound <- 1; print(exists(\"export_like_bound\"))",
        "during export-like continuation binding",
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
        "expected first request after export-like continuation binding to create state, got: {first_request:?}"
    );

    run_session_end_clear(&exe, temp.path(), &env_file, session_id)?;

    let after_clear = match repl_text(
        &mut session,
        "print(exists(\"export_like_bound\"))",
        "after export-like continuation clear",
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
        "expected clear for the export-like continuation session to reset bound state, got: {after_clear:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_clear_restart_binds_after_session_start_hook() -> TestResult<()> {
    let _guard = common::lock_test_mutex()?;
    let temp = tempfile::tempdir()?;
    let env_file = temp.path().join("claude.env");
    let exe = common::resolve_test_binary()?;

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
async fn claude_clear_ignores_marker_text_in_non_export_lines() -> TestResult<()> {
    let _guard = common::lock_test_mutex()?;
    let temp = tempfile::tempdir()?;
    let env_file = temp.path().join("claude.env");
    let exe = common::resolve_test_binary()?;

    run_session_start(&exe, temp.path(), &env_file, "sess-exported")?;
    let stray_marker = "# stray marker # mcp-repl-session-id-b64=c2Vzcy1zdHJheQ==\n";
    std::fs::OpenOptions::new()
        .append(true)
        .open(&env_file)?
        .write_all(stray_marker.as_bytes())?;

    let mut session =
        common::spawn_server_with_env_vars(claude_env_vars(temp.path(), &env_file)).await?;

    let bound = match repl_text(
        &mut session,
        "export_bound <- 1; print(exists(\"export_bound\"))",
        "before clear with stray marker text",
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
        "expected exported session to bind before clear, got: {bound:?}"
    );

    run_session_end_clear(&exe, temp.path(), &env_file, "sess-exported")?;

    let after_clear = match repl_text(
        &mut session,
        "print(exists(\"export_bound\"))",
        "after clear with stray marker text",
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
        "expected stray non-export marker text not to change clear targeting, got: {after_clear:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_clear_late_session_binding_restarts_prebound_worker_state() -> TestResult<()> {
    let _guard = common::lock_test_mutex()?;
    let temp = tempfile::tempdir()?;
    let env_file = temp.path().join("claude.env");
    let exe = common::resolve_test_binary()?;

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
async fn claude_clear_retargets_stale_startup_binding_before_next_clear() -> TestResult<()> {
    let _guard = common::lock_test_mutex()?;
    let temp = tempfile::tempdir()?;
    let env_file = temp.path().join("claude.env");
    let exe = common::resolve_test_binary()?;

    let encoded = URL_SAFE_NO_PAD.encode("sess-stale");
    std::fs::write(
        &env_file,
        format!("export MCP_REPL_CLAUDE_SESSION_ID={SESSION_ID_TOKEN_PREFIX}{encoded}\n"),
    )?;

    let mut session =
        common::spawn_server_with_env_vars(claude_env_vars(temp.path(), &env_file)).await?;

    run_session_start(&exe, temp.path(), &env_file, "sess-current")?;

    let before_clear = match repl_text(
        &mut session,
        "x <- 1; print(exists(\"x\"))",
        "after startup session start and before clear",
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
        before_clear.contains("TRUE"),
        "expected current session to create state before clear, got: {before_clear:?}"
    );

    run_session_end_clear(&exe, temp.path(), &env_file, "sess-current")?;
    run_session_start(&exe, temp.path(), &env_file, "sess-next")?;

    let after_clear = match repl_text(
        &mut session,
        "print(exists(\"x\"))",
        "after clearing a stale startup binding",
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
        "expected clear to reset state after startup rebinding, got: {after_clear:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_clear_matches_exact_env_file_and_session() -> TestResult<()> {
    let _guard = common::lock_test_mutex()?;
    let temp = tempfile::tempdir()?;
    let env_file_a = temp.path().join("claude-a.env");
    let env_file_b = temp.path().join("claude-b.env");
    let exe = common::resolve_test_binary()?;
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
async fn claude_clear_rebinds_after_later_session_start() -> TestResult<()> {
    let _guard = common::lock_test_mutex()?;
    let temp = tempfile::tempdir()?;
    let env_file = temp.path().join("claude.env");
    let exe = common::resolve_test_binary()?;

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
        after_session_b_clear.contains("FALSE"),
        "expected a later SessionStart to retarget the binding so sess-b clear resets the session, got: {after_session_b_clear:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_clear_concurrent_same_directory_sessions_do_not_reset_each_other() -> TestResult<()>
{
    let _guard = common::lock_test_mutex()?;
    let temp = tempfile::tempdir()?;
    let env_file_a = temp.path().join("claude-a.env");
    let env_file_b = temp.path().join("claude-b.env");
    let exe = common::resolve_test_binary()?;

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

#[tokio::test(flavor = "multi_thread")]
async fn claude_clear_scope_binding_works_without_claude_env_file() -> TestResult<()> {
    let _guard = common::lock_test_mutex()?;
    let temp = tempfile::tempdir()?;
    let exe = common::resolve_test_binary()?;
    let env_vars = claude_scope_env_vars(temp.path(), "scope-without-env-file");

    run_session_start_with_env(&exe, &env_vars, "sess-scope")?;

    let mut session = common::spawn_server_with_env_vars(env_vars.clone()).await?;

    let first_request = match repl_text(
        &mut session,
        "scope_bound <- 1; print(exists(\"scope_bound\"))",
        "during scope binding without CLAUDE_ENV_FILE",
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
        "expected scope-bound request to create state, got: {first_request:?}"
    );

    run_session_end_clear_with_env(&exe, &env_vars, "sess-scope")?;

    let after_clear = match repl_text(
        &mut session,
        "print(exists(\"scope_bound\"))",
        "after scope-based clear without CLAUDE_ENV_FILE",
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
        "expected scope-based clear to reset state without CLAUDE_ENV_FILE, got: {after_clear:?}"
    );
    Ok(())
}
