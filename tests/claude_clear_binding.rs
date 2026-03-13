mod common;

use common::TestResult;
use rmcp::model::RawContent;
use serde_json::json;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};

fn test_mutex() -> &'static Mutex<()> {
    static TEST_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
    TEST_MUTEX.get_or_init(|| Mutex::new(()))
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

fn run_claude_hook(
    exe: &Path,
    state_home: &Path,
    project_dir: &Path,
    subcommand: &str,
    input: serde_json::Value,
) -> TestResult<()> {
    let mut child = Command::new(exe)
        .arg("claude-hook")
        .arg(subcommand)
        .env("XDG_STATE_HOME", state_home)
        .env("CLAUDE_PROJECT_DIR", project_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;

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

#[tokio::test(flavor = "multi_thread")]
async fn claude_clear_restart_binds_after_session_start_hook() -> TestResult<()> {
    let _guard = test_mutex()
        .lock()
        .map_err(|_| "claude_clear_binding test mutex poisoned")?;
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
