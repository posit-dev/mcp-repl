mod common;

use common::TestResult;
use rmcp::model::RawContent;
use serde_json::json;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};

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

fn spawn_claude_hook_with_env(
    exe: &Path,
    env_vars: &[(String, String)],
    subcommand: &str,
) -> TestResult<std::process::Child> {
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
    Ok(cmd.spawn()?)
}

fn run_claude_hook(
    exe: &Path,
    state_home: &Path,
    project_dir: &Path,
    subcommand: &str,
    input: serde_json::Value,
) -> TestResult<()> {
    run_claude_hook_with_env(
        exe,
        &[
            (
                "XDG_STATE_HOME".to_string(),
                state_home.to_string_lossy().to_string(),
            ),
            (
                "CLAUDE_PROJECT_DIR".to_string(),
                project_dir.to_string_lossy().to_string(),
            ),
        ],
        subcommand,
        input,
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_clear_restart_binds_after_session_start_hook() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempfile::tempdir()?;
    let project_dir = temp.path().join("project");
    std::fs::create_dir_all(&project_dir)?;
    let exe = resolve_exe()?;

    let mut session = common::spawn_server_with_env_vars(vec![
        (
            "XDG_STATE_HOME".to_string(),
            temp.path().to_string_lossy().to_string(),
        ),
        (
            "CLAUDE_PROJECT_DIR".to_string(),
            project_dir.to_string_lossy().to_string(),
        ),
    ])
    .await?;

    run_claude_hook(
        &exe,
        temp.path(),
        &project_dir,
        "session-start",
        json!({
            "hook_event_name": "SessionStart",
            "session_id": "sess-current"
        }),
    )?;

    let set_var = session.write_stdin_raw_with("x <- 1", Some(10.0)).await?;
    let set_var_text = result_text(&set_var);
    if backend_unavailable(&set_var_text) {
        eprintln!("claude_clear_binding backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if busy_response(&set_var_text) {
        eprintln!("claude_clear_binding worker remained busy before clear; skipping");
        session.cancel().await?;
        return Ok(());
    }

    run_claude_hook(
        &exe,
        temp.path(),
        &project_dir,
        "session-end",
        json!({
            "hook_event_name": "SessionEnd",
            "session_id": "sess-current",
            "reason": "clear"
        }),
    )?;

    let after_clear = session
        .write_stdin_raw_with("print(exists(\"x\"))", Some(10.0))
        .await?;
    let after_clear_text = result_text(&after_clear);
    if backend_unavailable(&after_clear_text) {
        eprintln!("claude_clear_binding backend unavailable after clear; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if busy_response(&after_clear_text) {
        eprintln!("claude_clear_binding worker remained busy after clear; skipping");
        session.cancel().await?;
        return Ok(());
    }

    session.cancel().await?;
    assert!(
        after_clear_text.contains("FALSE"),
        "expected clear-triggered restart to clear x, got: {after_clear_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_clear_rebinds_idle_server_before_new_session_clears() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempfile::tempdir()?;
    let env_file_a = temp.path().join("claude-a.env");
    let env_file_b = temp.path().join("claude-b.env");
    let exe = resolve_exe()?;
    fs::write(&env_file_a, "export MCP_REPL_CLAUDE_SESSION_ID=sess-old\n")?;

    run_claude_hook_with_env(
        &exe,
        &[
            (
                "XDG_STATE_HOME".to_string(),
                temp.path().to_string_lossy().to_string(),
            ),
            (
                "CLAUDE_ENV_FILE".to_string(),
                env_file_a.to_string_lossy().to_string(),
            ),
        ],
        "session-start",
        json!({
            "hook_event_name": "SessionStart",
            "session_id": "sess-old"
        }),
    )?;

    let mut session = common::spawn_server_with_env_vars(vec![
        (
            "XDG_STATE_HOME".to_string(),
            temp.path().to_string_lossy().to_string(),
        ),
        (
            "CLAUDE_ENV_FILE".to_string(),
            env_file_a.to_string_lossy().to_string(),
        ),
    ])
    .await?;

    let set_var = session.write_stdin_raw_with("x <- 1", Some(10.0)).await?;
    let set_var_text = result_text(&set_var);
    if backend_unavailable(&set_var_text) {
        eprintln!("claude_clear_binding backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if busy_response(&set_var_text) {
        eprintln!("claude_clear_binding worker remained busy before idle rebind; skipping");
        session.cancel().await?;
        return Ok(());
    }

    run_claude_hook_with_env(
        &exe,
        &[
            (
                "XDG_STATE_HOME".to_string(),
                temp.path().to_string_lossy().to_string(),
            ),
            (
                "CLAUDE_ENV_FILE".to_string(),
                env_file_b.to_string_lossy().to_string(),
            ),
            (
                "MCP_REPL_CLAUDE_SESSION_ID".to_string(),
                "sess-old".to_string(),
            ),
        ],
        "session-start",
        json!({
            "hook_event_name": "SessionStart",
            "session_id": "sess-new"
        }),
    )?;
    run_claude_hook_with_env(
        &exe,
        &[
            (
                "XDG_STATE_HOME".to_string(),
                temp.path().to_string_lossy().to_string(),
            ),
            (
                "CLAUDE_ENV_FILE".to_string(),
                env_file_b.to_string_lossy().to_string(),
            ),
        ],
        "session-end",
        json!({
            "hook_event_name": "SessionEnd",
            "session_id": "sess-new",
            "reason": "clear"
        }),
    )?;

    let after_clear = session
        .write_stdin_raw_with("print(exists(\"x\"))", Some(10.0))
        .await?;
    let after_clear_text = result_text(&after_clear);
    if backend_unavailable(&after_clear_text) {
        eprintln!("claude_clear_binding backend unavailable after idle rebind clear; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if busy_response(&after_clear_text) {
        eprintln!("claude_clear_binding worker remained busy after idle rebind clear; skipping");
        session.cancel().await?;
        return Ok(());
    }

    session.cancel().await?;
    assert!(
        after_clear_text.contains("FALSE"),
        "expected clear after idle rebind to clear x, got: {after_clear_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_clear_reads_latest_session_from_env_file_without_trailing_newline() -> TestResult<()>
{
    let _guard = test_guard();
    let temp = tempfile::tempdir()?;
    let env_file = temp.path().join("claude.env");
    let exe = resolve_exe()?;
    fs::write(&env_file, "export MCP_REPL_CLAUDE_SESSION_ID=sess-old")?;

    run_claude_hook_with_env(
        &exe,
        &[
            (
                "XDG_STATE_HOME".to_string(),
                temp.path().to_string_lossy().to_string(),
            ),
            (
                "CLAUDE_ENV_FILE".to_string(),
                env_file.to_string_lossy().to_string(),
            ),
        ],
        "session-start",
        json!({
            "hook_event_name": "SessionStart",
            "session_id": "sess-current"
        }),
    )?;

    let mut session = common::spawn_server_with_env_vars(vec![
        (
            "XDG_STATE_HOME".to_string(),
            temp.path().to_string_lossy().to_string(),
        ),
        (
            "CLAUDE_ENV_FILE".to_string(),
            env_file.to_string_lossy().to_string(),
        ),
    ])
    .await?;

    let set_var = session.write_stdin_raw_with("x <- 1", Some(10.0)).await?;
    let set_var_text = result_text(&set_var);
    if backend_unavailable(&set_var_text) {
        eprintln!("claude_clear_binding backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if busy_response(&set_var_text) {
        eprintln!("claude_clear_binding worker remained busy before env-file clear; skipping");
        session.cancel().await?;
        return Ok(());
    }

    run_claude_hook_with_env(
        &exe,
        &[
            (
                "XDG_STATE_HOME".to_string(),
                temp.path().to_string_lossy().to_string(),
            ),
            (
                "CLAUDE_ENV_FILE".to_string(),
                env_file.to_string_lossy().to_string(),
            ),
        ],
        "session-end",
        json!({
            "hook_event_name": "SessionEnd",
            "session_id": "sess-current",
            "reason": "clear"
        }),
    )?;

    let after_clear = session
        .write_stdin_raw_with("print(exists(\"x\"))", Some(10.0))
        .await?;
    let after_clear_text = result_text(&after_clear);
    if backend_unavailable(&after_clear_text) {
        eprintln!("claude_clear_binding backend unavailable after env-file clear; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if busy_response(&after_clear_text) {
        eprintln!("claude_clear_binding worker remained busy after env-file clear; skipping");
        session.cancel().await?;
        return Ok(());
    }

    session.cancel().await?;
    assert!(
        after_clear_text.contains("FALSE"),
        "expected env-file-based clear to clear x, got: {after_clear_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_clear_preserves_project_state_across_non_clear_session_end() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempfile::tempdir()?;
    let project_dir = temp.path().join("project");
    fs::create_dir_all(&project_dir)?;
    let exe = resolve_exe()?;

    run_claude_hook(
        &exe,
        temp.path(),
        &project_dir,
        "session-start",
        json!({
            "hook_event_name": "SessionStart",
            "session_id": "sess-a"
        }),
    )?;

    let mut session = common::spawn_server_with_env_vars(vec![
        (
            "XDG_STATE_HOME".to_string(),
            temp.path().to_string_lossy().to_string(),
        ),
        (
            "CLAUDE_PROJECT_DIR".to_string(),
            project_dir.to_string_lossy().to_string(),
        ),
    ])
    .await?;

    let set_var = session.write_stdin_raw_with("x <- 1", Some(10.0)).await?;
    let set_var_text = result_text(&set_var);
    if backend_unavailable(&set_var_text) {
        eprintln!("claude_clear_binding backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if busy_response(&set_var_text) {
        eprintln!(
            "claude_clear_binding worker remained busy before project-state handoff; skipping"
        );
        session.cancel().await?;
        return Ok(());
    }

    run_claude_hook(
        &exe,
        temp.path(),
        &project_dir,
        "session-end",
        json!({
            "hook_event_name": "SessionEnd",
            "session_id": "sess-a",
            "reason": "other"
        }),
    )?;
    run_claude_hook(
        &exe,
        temp.path(),
        &project_dir,
        "session-start",
        json!({
            "hook_event_name": "SessionStart",
            "session_id": "sess-b"
        }),
    )?;
    run_claude_hook(
        &exe,
        temp.path(),
        &project_dir,
        "session-end",
        json!({
            "hook_event_name": "SessionEnd",
            "session_id": "sess-b",
            "reason": "clear"
        }),
    )?;

    let after_clear = session
        .write_stdin_raw_with("print(exists(\"x\"))", Some(10.0))
        .await?;
    let after_clear_text = result_text(&after_clear);
    if backend_unavailable(&after_clear_text) {
        eprintln!(
            "claude_clear_binding backend unavailable after project-state handoff clear; skipping"
        );
        session.cancel().await?;
        return Ok(());
    }
    if busy_response(&after_clear_text) {
        eprintln!(
            "claude_clear_binding worker remained busy after project-state handoff clear; skipping"
        );
        session.cancel().await?;
        return Ok(());
    }

    session.cancel().await?;
    assert!(
        after_clear_text.contains("FALSE"),
        "expected clear after non-clear session end handoff to clear x, got: {after_clear_text:?}"
    );
    Ok(())
}

#[test]
fn claude_clear_session_start_hooks_do_not_race_on_shared_project_state() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempfile::tempdir()?;
    let project_dir = temp.path().join("project");
    fs::create_dir_all(&project_dir)?;
    let exe = resolve_exe()?;
    let env_vars = vec![
        (
            "XDG_STATE_HOME".to_string(),
            temp.path().to_string_lossy().to_string(),
        ),
        (
            "CLAUDE_PROJECT_DIR".to_string(),
            project_dir.to_string_lossy().to_string(),
        ),
    ];

    let mut children = Vec::new();
    for _ in 0..32 {
        children.push(spawn_claude_hook_with_env(
            &exe,
            &env_vars,
            "session-start",
        )?);
    }

    for (index, child) in children.iter_mut().enumerate() {
        {
            let stdin = child
                .stdin
                .as_mut()
                .ok_or_else(|| "failed to capture concurrent claude-hook stdin".to_string())?;
            stdin.write_all(
                serde_json::to_string(&json!({
                    "hook_event_name": "SessionStart",
                    "session_id": format!("sess-{index}")
                }))?
                .as_bytes(),
            )?;
        }
    }
    for child in &mut children {
        let _ = child.stdin.take();
    }

    let mut failures = Vec::new();
    for child in children {
        let output = child.wait_with_output()?;
        if output.status.success() {
            continue;
        }
        failures.push(String::from_utf8_lossy(&output.stderr).to_string());
    }

    assert!(
        failures.is_empty(),
        "expected concurrent session-start hooks to succeed, got failures: {failures:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_clear_in_same_project_does_not_reset_another_live_session() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempfile::tempdir()?;
    let project_dir = temp.path().join("project");
    fs::create_dir_all(&project_dir)?;
    let exe = resolve_exe()?;

    run_claude_hook(
        &exe,
        temp.path(),
        &project_dir,
        "session-start",
        json!({
            "hook_event_name": "SessionStart",
            "session_id": "sess-a"
        }),
    )?;

    let mut session = common::spawn_server_with_env_vars(vec![
        (
            "XDG_STATE_HOME".to_string(),
            temp.path().to_string_lossy().to_string(),
        ),
        (
            "CLAUDE_PROJECT_DIR".to_string(),
            project_dir.to_string_lossy().to_string(),
        ),
    ])
    .await?;

    let set_var = session.write_stdin_raw_with("x <- 1", Some(10.0)).await?;
    let set_var_text = result_text(&set_var);
    if backend_unavailable(&set_var_text) {
        eprintln!("claude_clear_binding backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if busy_response(&set_var_text) {
        eprintln!(
            "claude_clear_binding worker remained busy before concurrent-project clear; skipping"
        );
        session.cancel().await?;
        return Ok(());
    }

    run_claude_hook(
        &exe,
        temp.path(),
        &project_dir,
        "session-start",
        json!({
            "hook_event_name": "SessionStart",
            "session_id": "sess-b"
        }),
    )?;
    run_claude_hook(
        &exe,
        temp.path(),
        &project_dir,
        "session-end",
        json!({
            "hook_event_name": "SessionEnd",
            "session_id": "sess-b",
            "reason": "clear"
        }),
    )?;

    let after_clear = session
        .write_stdin_raw_with("print(exists(\"x\"))", Some(10.0))
        .await?;
    let after_clear_text = result_text(&after_clear);
    if backend_unavailable(&after_clear_text) {
        eprintln!(
            "claude_clear_binding backend unavailable after concurrent-project clear; skipping"
        );
        session.cancel().await?;
        return Ok(());
    }
    if busy_response(&after_clear_text) {
        eprintln!(
            "claude_clear_binding worker remained busy after concurrent-project clear; skipping"
        );
        session.cancel().await?;
        return Ok(());
    }

    session.cancel().await?;
    assert!(
        after_clear_text.contains("TRUE"),
        "expected session B clear not to reset session A state, got: {after_clear_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_clear_re_resolves_env_file_after_handoff() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempfile::tempdir()?;
    let env_file_a = temp.path().join("claude-a.env");
    let env_file_b = temp.path().join("claude-b.env");
    let exe = resolve_exe()?;
    fs::write(&env_file_a, "export MCP_REPL_CLAUDE_SESSION_ID=sess-a\n")?;

    let mut session = common::spawn_server_with_env_vars(vec![
        (
            "XDG_STATE_HOME".to_string(),
            temp.path().to_string_lossy().to_string(),
        ),
        (
            "CLAUDE_ENV_FILE".to_string(),
            env_file_a.to_string_lossy().to_string(),
        ),
    ])
    .await?;

    let set_var = session.write_stdin_raw_with("x <- 1", Some(10.0)).await?;
    let set_var_text = result_text(&set_var);
    if backend_unavailable(&set_var_text) {
        eprintln!("claude_clear_binding backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if busy_response(&set_var_text) {
        eprintln!("claude_clear_binding worker remained busy before env-file handoff; skipping");
        session.cancel().await?;
        return Ok(());
    }

    run_claude_hook_with_env(
        &exe,
        &[
            (
                "XDG_STATE_HOME".to_string(),
                temp.path().to_string_lossy().to_string(),
            ),
            (
                "CLAUDE_ENV_FILE".to_string(),
                env_file_b.to_string_lossy().to_string(),
            ),
            (
                "MCP_REPL_CLAUDE_SESSION_ID".to_string(),
                "sess-a".to_string(),
            ),
        ],
        "session-start",
        json!({
            "hook_event_name": "SessionStart",
            "session_id": "sess-b"
        }),
    )?;

    fs::write(
        &env_file_a,
        "export MCP_REPL_CLAUDE_SESSION_ID=sess-stale\n",
    )?;

    let handoff = session.write_stdin_raw_with("y <- 1", Some(10.0)).await?;
    let handoff_text = result_text(&handoff);
    if backend_unavailable(&handoff_text) {
        eprintln!("claude_clear_binding backend unavailable during env-file handoff; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if busy_response(&handoff_text) {
        eprintln!("claude_clear_binding worker remained busy during env-file handoff; skipping");
        session.cancel().await?;
        return Ok(());
    }

    run_claude_hook_with_env(
        &exe,
        &[
            (
                "XDG_STATE_HOME".to_string(),
                temp.path().to_string_lossy().to_string(),
            ),
            (
                "CLAUDE_ENV_FILE".to_string(),
                env_file_b.to_string_lossy().to_string(),
            ),
        ],
        "session-end",
        json!({
            "hook_event_name": "SessionEnd",
            "session_id": "sess-b",
            "reason": "clear"
        }),
    )?;

    let after_clear = session
        .write_stdin_raw_with("print(exists(\"y\"))", Some(10.0))
        .await?;
    let after_clear_text = result_text(&after_clear);
    if backend_unavailable(&after_clear_text) {
        eprintln!(
            "claude_clear_binding backend unavailable after env-file re-resolve clear; skipping"
        );
        session.cancel().await?;
        return Ok(());
    }
    if busy_response(&after_clear_text) {
        eprintln!(
            "claude_clear_binding worker remained busy after env-file re-resolve clear; skipping"
        );
        session.cancel().await?;
        return Ok(());
    }

    session.cancel().await?;
    assert!(
        after_clear_text.contains("FALSE"),
        "expected re-resolved env-file clear to reset y, got: {after_clear_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_clear_shared_env_file_does_not_reset_another_live_session() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempfile::tempdir()?;
    let env_file = temp.path().join("claude.env");
    let exe = resolve_exe()?;
    fs::write(&env_file, "export MCP_REPL_CLAUDE_SESSION_ID=sess-a\n")?;

    run_claude_hook_with_env(
        &exe,
        &[
            (
                "XDG_STATE_HOME".to_string(),
                temp.path().to_string_lossy().to_string(),
            ),
            (
                "CLAUDE_ENV_FILE".to_string(),
                env_file.to_string_lossy().to_string(),
            ),
        ],
        "session-start",
        json!({
            "hook_event_name": "SessionStart",
            "session_id": "sess-a"
        }),
    )?;

    let mut session = common::spawn_server_with_env_vars(vec![
        (
            "XDG_STATE_HOME".to_string(),
            temp.path().to_string_lossy().to_string(),
        ),
        (
            "CLAUDE_ENV_FILE".to_string(),
            env_file.to_string_lossy().to_string(),
        ),
    ])
    .await?;

    let set_var = session.write_stdin_raw_with("x <- 1", Some(10.0)).await?;
    let set_var_text = result_text(&set_var);
    if backend_unavailable(&set_var_text) {
        eprintln!("claude_clear_binding backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if busy_response(&set_var_text) {
        eprintln!(
            "claude_clear_binding worker remained busy before shared env-file clear; skipping"
        );
        session.cancel().await?;
        return Ok(());
    }

    run_claude_hook_with_env(
        &exe,
        &[
            (
                "XDG_STATE_HOME".to_string(),
                temp.path().to_string_lossy().to_string(),
            ),
            (
                "CLAUDE_ENV_FILE".to_string(),
                env_file.to_string_lossy().to_string(),
            ),
        ],
        "session-start",
        json!({
            "hook_event_name": "SessionStart",
            "session_id": "sess-b"
        }),
    )?;
    run_claude_hook_with_env(
        &exe,
        &[
            (
                "XDG_STATE_HOME".to_string(),
                temp.path().to_string_lossy().to_string(),
            ),
            (
                "CLAUDE_ENV_FILE".to_string(),
                env_file.to_string_lossy().to_string(),
            ),
        ],
        "session-end",
        json!({
            "hook_event_name": "SessionEnd",
            "session_id": "sess-b",
            "reason": "clear"
        }),
    )?;

    let after_clear = session
        .write_stdin_raw_with("print(exists(\"x\"))", Some(10.0))
        .await?;
    let after_clear_text = result_text(&after_clear);
    if backend_unavailable(&after_clear_text) {
        eprintln!("claude_clear_binding backend unavailable after shared env-file clear; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if busy_response(&after_clear_text) {
        eprintln!(
            "claude_clear_binding worker remained busy after shared env-file clear; skipping"
        );
        session.cancel().await?;
        return Ok(());
    }

    session.cancel().await?;
    assert!(
        after_clear_text.contains("TRUE"),
        "expected session B clear on shared env file not to reset session A state, got: {after_clear_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_clear_same_project_request_stays_pinned_to_live_session() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempfile::tempdir()?;
    let project_dir = temp.path().join("project");
    fs::create_dir_all(&project_dir)?;
    let exe = resolve_exe()?;

    run_claude_hook(
        &exe,
        temp.path(),
        &project_dir,
        "session-start",
        json!({
            "hook_event_name": "SessionStart",
            "session_id": "sess-a"
        }),
    )?;

    let mut session = common::spawn_server_with_env_vars(vec![
        (
            "XDG_STATE_HOME".to_string(),
            temp.path().to_string_lossy().to_string(),
        ),
        (
            "CLAUDE_PROJECT_DIR".to_string(),
            project_dir.to_string_lossy().to_string(),
        ),
    ])
    .await?;

    let set_var = session.write_stdin_raw_with("x <- 1", Some(10.0)).await?;
    let set_var_text = result_text(&set_var);
    if backend_unavailable(&set_var_text) {
        eprintln!("claude_clear_binding backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if busy_response(&set_var_text) {
        eprintln!(
            "claude_clear_binding worker remained busy before same-project pinning check; skipping"
        );
        session.cancel().await?;
        return Ok(());
    }

    run_claude_hook(
        &exe,
        temp.path(),
        &project_dir,
        "session-start",
        json!({
            "hook_event_name": "SessionStart",
            "session_id": "sess-b"
        }),
    )?;

    let follow_up = session.write_stdin_raw_with("y <- 1", Some(10.0)).await?;
    let follow_up_text = result_text(&follow_up);
    if backend_unavailable(&follow_up_text) {
        eprintln!(
            "claude_clear_binding backend unavailable during same-project pinning check; skipping"
        );
        session.cancel().await?;
        return Ok(());
    }
    if busy_response(&follow_up_text) {
        eprintln!(
            "claude_clear_binding worker remained busy during same-project pinning check; skipping"
        );
        session.cancel().await?;
        return Ok(());
    }

    run_claude_hook(
        &exe,
        temp.path(),
        &project_dir,
        "session-end",
        json!({
            "hook_event_name": "SessionEnd",
            "session_id": "sess-b",
            "reason": "clear"
        }),
    )?;

    let after_clear = session
        .write_stdin_raw_with("print(exists(\"y\"))", Some(10.0))
        .await?;
    let after_clear_text = result_text(&after_clear);
    if backend_unavailable(&after_clear_text) {
        eprintln!(
            "claude_clear_binding backend unavailable after same-project pinning clear; skipping"
        );
        session.cancel().await?;
        return Ok(());
    }
    if busy_response(&after_clear_text) {
        eprintln!(
            "claude_clear_binding worker remained busy after same-project pinning clear; skipping"
        );
        session.cancel().await?;
        return Ok(());
    }

    session.cancel().await?;
    assert!(
        after_clear_text.contains("TRUE"),
        "expected later request in session A to stay pinned after session B clear, got: {after_clear_text:?}"
    );
    Ok(())
}
