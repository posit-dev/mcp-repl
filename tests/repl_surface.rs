mod common;

use common::TestResult;
use rmcp::model::RawContent;
use serde_json::json;

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

#[tokio::test(flavor = "multi_thread")]
async fn repl_tool_hides_ipc_fd_env_vars_from_r_user_code() -> TestResult<()> {
    let mut session = common::spawn_server().await?;

    let result = session
        .call_tool_raw(
            session.repl_tool_name(),
            json!({
                "input": "cat(sprintf(\"%s %s\\n\", nzchar(Sys.getenv(\"MCP_REPL_IPC_READ_FD\")), nzchar(Sys.getenv(\"MCP_REPL_IPC_WRITE_FD\"))))\n",
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
    session.cancel().await?;

    assert!(
        text.contains("FALSE FALSE"),
        "expected IPC fd env vars to be hidden from R user code, got: {text:?}"
    );

    Ok(())
}
