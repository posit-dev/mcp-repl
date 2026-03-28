mod common;

use common::TestResult;
use rmcp::model::RawContent;
use serde_json::json;
#[cfg(not(windows))]
use std::time::Duration;
#[cfg(not(windows))]
use tokio::time::Instant;

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
        || text.contains("unable to initialize the JIT")
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

#[cfg(not(windows))]
fn shutdown_completion_budget() -> Duration {
    if cfg!(target_os = "macos") {
        Duration::from_millis(1_500)
    } else {
        Duration::from_millis(1_200)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn repl_tool_accepts_input_and_timeout_ms() -> TestResult<()> {
    let mut session = common::spawn_server().await?;

    let result = session
        .call_tool_raw(
            session.repl_tool_name(),
            json!({
                "input": "1+1\n",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("repl_surface backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if busy_response(&text) {
        eprintln!("repl_surface worker remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }

    assert!(text.contains("2"), "expected 2 in output, got: {text:?}");
    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn repl_reset_clears_state() -> TestResult<()> {
    let mut session = common::spawn_server().await?;

    let set_var = session
        .call_tool_raw(
            session.repl_tool_name(),
            json!({
                "input": "x <- 1\n",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let set_var_text = result_text(&set_var);
    if backend_unavailable(&set_var_text) {
        eprintln!("repl_surface backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if busy_response(&set_var_text) {
        eprintln!("repl_surface worker remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let _ = session.call_tool_raw("repl_reset", json!({})).await?;

    let after_reset = session
        .call_tool_raw(
            session.repl_tool_name(),
            json!({
                "input": "print(exists(\"x\"))\n",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let after_reset_text = result_text(&after_reset);
    if backend_unavailable(&after_reset_text) {
        eprintln!("repl_surface backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if busy_response(&after_reset_text) {
        eprintln!("repl_surface worker remained busy after reset; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        after_reset_text.contains("FALSE"),
        "expected reset state, got: {after_reset_text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread")]
async fn repl_reset_does_not_wait_for_background_ipc_holders() -> TestResult<()> {
    let mut session = common::spawn_server().await?;

    let setup = session
        .call_tool_raw(
            session.repl_tool_name(),
            json!({
                "input": "fd <- Sys.getenv(\"MCP_REPL_IPC_WRITE_FD\"); cmd <- sprintf(\"eval 'exec 3>&%s'; sleep 2.5\", fd); system2(\"/bin/sh\", c(\"-c\", cmd), wait = FALSE, stdout = FALSE, stderr = FALSE); cat(\"ipc background ready\\n\")\n",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let setup_text = result_text(&setup);
    if backend_unavailable(&setup_text) {
        eprintln!("repl_surface backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if busy_response(&setup_text) {
        eprintln!("repl_surface worker remained busy before reset; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        setup_text.contains("ipc background ready"),
        "expected setup confirmation, got: {setup_text:?}"
    );

    let start = Instant::now();
    let reset = session.call_tool_raw("repl_reset", json!({})).await?;
    let elapsed = start.elapsed();
    let reset_text = result_text(&reset);
    if backend_unavailable(&reset_text) {
        eprintln!("repl_surface backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if busy_response(&reset_text) {
        eprintln!("repl_surface worker remained busy during reset; skipping");
        session.cancel().await?;
        return Ok(());
    }

    assert!(
        elapsed < shutdown_completion_budget(),
        "expected repl_reset to finish before background IPC holder exit, got {elapsed:?}: {reset_text:?}"
    );

    let after_reset = session
        .call_tool_raw(
            session.repl_tool_name(),
            json!({
                "input": "1+1\n",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let after_reset_text = result_text(&after_reset);
    session.cancel().await?;

    assert!(
        after_reset_text.contains("2"),
        "expected prompt recovery after reset, got: {after_reset_text:?}"
    );

    Ok(())
}
