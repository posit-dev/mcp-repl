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

fn result_text_items(result: &rmcp::model::CallToolResult) -> Vec<String> {
    result
        .content
        .iter()
        .filter_map(|item| match &item.raw {
            RawContent::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect()
}

fn result_image_count(result: &rmcp::model::CallToolResult) -> usize {
    result
        .content
        .iter()
        .filter(|item| matches!(item.raw, RawContent::Image(_)))
        .count()
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

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_invalid_output_base64_is_protocol_error() -> TestResult<()> {
    let session = spawn_zod_server().await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "bad-output-base64",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);
    assert!(
        text.contains("invalid output_text base64"),
        "expected invalid base64 protocol error, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_readline_input_mismatch_is_protocol_error() -> TestResult<()> {
    let session = spawn_zod_server().await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "misreport-input different",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);
    assert!(
        text.contains("readline_input text does not match active stdin"),
        "expected readline_input accounting protocol error, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_output_after_session_end_is_protocol_error() -> TestResult<()> {
    let session = spawn_zod_server().await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "bad-output-after-session-end",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);
    assert!(
        text.contains("worker sideband message after session_end"),
        "expected output-after-session-end protocol error, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_preserves_mixed_output_order() -> TestResult<()> {
    let session = spawn_zod_server().await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "mixed-output",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let items = result_text_items(&result);
    let joined = items.join("");
    let before = joined.find("stdout-before\n");
    let middle = joined.find("stderr-middle\n");
    let after = joined.find("stdout-after\n");
    assert!(
        matches!((before, middle, after), (Some(before), Some(middle), Some(after)) if before < middle && middle < after),
        "expected mixed output in sideband order, got: {items:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_emits_image_output() -> TestResult<()> {
    let session = spawn_zod_server().await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "image",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    assert_eq!(
        result_image_count(&result),
        1,
        "expected one Zod image, got content: {:?}",
        result.content
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_interrupt_tail_runs_after_recovery() -> TestResult<()> {
    let session = spawn_zod_server().await?;

    let first = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "interruptible 1000",
                "timeout_ms": 10
            }),
        )
        .await?;
    let first_text = result_text(&first);
    assert!(
        first_text.contains("<<repl status: busy"),
        "expected timeout busy status, got: {first_text:?}"
    );

    let interrupted = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "\u{3}tail after interrupt",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&interrupted);
    assert!(
        text.contains("tail after interrupt\n"),
        "expected interrupt tail to run after recovery, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}
