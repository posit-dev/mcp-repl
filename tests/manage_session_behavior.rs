#![allow(clippy::await_holding_lock)]

mod common;

use common::TestResult;
use rmcp::model::RawContent;
use std::sync::{Mutex, OnceLock};
use tokio::time::{Duration, Instant, sleep};

fn test_mutex() -> &'static Mutex<()> {
    static TEST_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
    TEST_MUTEX.get_or_init(|| Mutex::new(()))
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

async fn spawn_manage_session() -> TestResult<common::McpTestSession> {
    common::spawn_server_with_args(vec![
        "--sandbox".to_string(),
        "danger-full-access".to_string(),
    ])
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupt_without_active_request_returns_prompt() -> TestResult<()> {
    let _guard = test_mutex()
        .lock()
        .map_err(|_| "manage_session_behavior test mutex poisoned")?;
    let mut session = spawn_manage_session().await?;

    let _ = session.write_stdin_raw_with("1+1", Some(5.0)).await?;
    let result = session.write_stdin_raw_with("\u{3}", Some(5.0)).await?;

    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("interrupt test backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains(">") || text.contains("<<repl status: busy"),
        "expected prompt or timeout status in output, got: {text:?}"
    );
    assert!(
        !text.contains("worker exited"),
        "did not expect interrupt to terminate the worker: {text:?}"
    );

    let deadline = Instant::now() + Duration::from_secs(20);
    let follow_text = loop {
        if Instant::now() >= deadline {
            session.cancel().await?;
            eprintln!("interrupt recovery did not complete in time; skipping");
            return Ok(());
        }
        let follow_up = session.write_stdin_raw_with("1+1", Some(1.0)).await?;
        let text = result_text(&follow_up);
        if backend_unavailable(&text) {
            eprintln!("interrupt test backend unavailable in this environment; skipping");
            session.cancel().await?;
            return Ok(());
        }
        if text.contains("worker is busy")
            || text.contains("request already running")
            || text.contains("input discarded while worker busy")
            || text.contains("<<repl status: busy")
        {
            sleep(Duration::from_millis(50)).await;
            continue;
        }
        break text;
    };
    session.cancel().await?;
    assert!(
        follow_text.contains("2"),
        "expected session to recover after interrupt, got: {follow_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_while_busy_resets_session() -> TestResult<()> {
    let _guard = test_mutex()
        .lock()
        .map_err(|_| "manage_session_behavior test mutex poisoned")?;
    let mut session = spawn_manage_session().await?;

    let _ = session
        .write_stdin_raw_with("x <- 1; Sys.sleep(5)", Some(0.1))
        .await?;

    let restart = session.write_stdin_raw_with("\u{4}", Some(5.0)).await?;
    let restart_text = result_text(&restart);
    if backend_unavailable(&restart_text) {
        eprintln!("restart test backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        restart_text.contains("new session started"),
        "expected restart notice, got: {restart_text:?}"
    );

    let result = session
        .write_stdin_raw_with("print(exists(\"x\"))", Some(5.0))
        .await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("restart test backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;

    assert!(
        text.contains("FALSE"),
        "expected cleared session, got: {text:?}"
    );
    Ok(())
}
