mod common;

use common::TestResult;
use rmcp::model::RawContent;
use serde_json::json;
use std::path::PathBuf;

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

fn zod_worker_path() -> TestResult<PathBuf> {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_zod-worker") {
        return Ok(PathBuf::from(path));
    }

    let mut path = std::env::current_exe()?;
    path.pop();
    path.pop();
    path.push(if cfg!(windows) {
        "zod-worker.exe"
    } else {
        "zod-worker"
    });
    if path.exists() {
        return Ok(path);
    }

    Err("unable to locate zod-worker test binary".into())
}

async fn spawn_zod_server() -> TestResult<common::McpTestSession> {
    let tempdir = tempfile::tempdir()?;
    let spec_path = tempdir.path().join("zod-worker.json");
    let spec = json!({
        "executable": zod_worker_path()?,
        "args": [],
        "working_dir": "inherit",
        "env": {},
        "stdin": "pipe",
        "sandbox": "server"
    });
    std::fs::write(&spec_path, serde_json::to_vec_pretty(&spec)?)?;
    common::spawn_server_with_args(vec![
        "--worker-spec".to_string(),
        spec_path.display().to_string(),
        "--sandbox".to_string(),
        "danger-full-access".to_string(),
        "--oversized-output".to_string(),
        "files".to_string(),
    ])
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_echoes_input_and_returns_worker_prompt() -> TestResult<()> {
    let session = spawn_zod_server().await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "hello zod",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);

    assert!(
        text.contains("hello zod\n"),
        "expected Zod to receive server-normalized stdin, got: {text:?}"
    );
    assert!(
        text.contains("zod> "),
        "expected worker-supplied prompt in response, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_preserves_prompt_shaped_stdout() -> TestResult<()> {
    let session = spawn_zod_server().await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": ">>> ",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);

    assert!(
        text.contains(">>> \n"),
        "expected prompt-shaped stdout to be preserved, got: {text:?}"
    );
    assert!(
        text.contains("zod> "),
        "expected worker-supplied prompt in response, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_empty_prompt_uses_generic_wait_status() -> TestResult<()> {
    let session = spawn_zod_server().await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "wait ",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);

    assert!(
        text.contains("<<repl status: waiting for stdin>>"),
        "expected generic wait status for empty worker prompt, got: {text:?}"
    );
    assert!(
        !text.contains("zod> "),
        "did not expect fabricated Zod prompt for empty worker prompt, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_timeout_poll_waits_for_unsatisfied_prompt() -> TestResult<()> {
    let session = spawn_zod_server().await?;

    let first = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "sleep 150",
                "timeout_ms": 10
            }),
        )
        .await?;
    let first_text = result_text(&first);
    assert!(
        first_text.contains("<<repl status: busy"),
        "expected timeout busy status, got: {first_text:?}"
    );

    let poll = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let poll_text = result_text(&poll);
    assert!(
        poll_text.contains("zod> "),
        "expected later poll to observe worker prompt, got: {poll_text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_busy_follow_up_does_not_reach_stdin() -> TestResult<()> {
    let session = spawn_zod_server().await?;

    let first = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "sleep 150",
                "timeout_ms": 10
            }),
        )
        .await?;
    let first_text = result_text(&first);
    assert!(
        first_text.contains("<<repl status: busy"),
        "expected timeout busy status, got: {first_text:?}"
    );

    let busy = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "second input",
                "timeout_ms": 10
            }),
        )
        .await?;
    let busy_text = result_text(&busy);
    assert!(
        busy_text.contains("busy") || busy_text.contains("discarded"),
        "expected busy follow-up response, got: {busy_text:?}"
    );

    let poll = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let poll_text = result_text(&poll);
    assert!(
        !poll_text.contains("second input"),
        "busy follow-up should not have reached Zod stdin, got: {poll_text:?}"
    );

    session.cancel().await?;
    Ok(())
}
