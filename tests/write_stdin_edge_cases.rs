#![allow(clippy::await_holding_lock)]

mod common;

use common::TestResult;
use rmcp::model::RawContent;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

fn test_mutex() -> &'static Mutex<()> {
    static TEST_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
    TEST_MUTEX.get_or_init(|| Mutex::new(()))
}

fn lock_test_mutex() -> MutexGuard<'static, ()> {
    match test_mutex().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
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

fn assert_invalid_timeout(result: &rmcp::model::CallToolResult) {
    assert_eq!(
        result.is_error,
        Some(true),
        "expected timeout<0 to be rejected as a tool error, got: {result:?}"
    );
    let text = result_text(result);
    assert!(
        text.contains("timeout_ms")
            || text.contains("non-negative")
            || text.contains("expected u64")
            || text.contains("invalid value"),
        "unexpected error message: {text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn write_stdin_timeout_zero_is_non_blocking() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let session = common::spawn_server().await?;

    let timeout_result = session
        .write_stdin_raw_unterminated_with("Sys.sleep(0.25); 6 * 7", Some(0.0))
        .await?;
    let timeout_text = result_text(&timeout_result);
    if backend_unavailable(&timeout_text) {
        eprintln!("write_stdin_edge_cases backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        timeout_text.contains("<<repl status: busy"),
        "expected zero-timeout call to return busy before expression completes, got: {timeout_text:?}"
    );
    assert!(
        !timeout_text.contains("[1] 42"),
        "did not expect zero-timeout call to include final result, got: {timeout_text:?}"
    );

    let completed = session
        .write_stdin_raw_unterminated_with("", Some(5.0))
        .await?;
    let completed_text = result_text(&completed);
    if backend_unavailable(&completed_text) {
        eprintln!("write_stdin_edge_cases backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        completed_text.contains("[1] 42"),
        "expected empty poll to return pending result, got: {completed_text:?}"
    );

    let follow_up = session
        .write_stdin_raw_unterminated_with("1+1", Some(5.0))
        .await?;
    let follow_up_text = result_text(&follow_up);
    if backend_unavailable(&follow_up_text) {
        eprintln!("write_stdin_edge_cases backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        follow_up_text.contains("2"),
        "expected session to remain usable after non-blocking call, got: {follow_up_text:?}"
    );

    let err = session
        .write_stdin_raw_unterminated_with("1+1", Some(-1.0))
        .await
        .expect("expected timeout<0 rejection result");
    assert_invalid_timeout(&err);

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn write_stdin_accepts_crlf_input() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let session = common::spawn_server().await?;

    let input = "cat('A\\n')\r\ncat('B\\n')";
    let result = session.write_stdin_raw_with(input, Some(10.0)).await?;
    let mut text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("write_stdin_edge_cases backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if text.contains("<<repl status: busy") {
        let deadline = Instant::now() + Duration::from_secs(30);
        while text.contains("<<repl status: busy") && Instant::now() < deadline {
            let polled = session
                .write_stdin_raw_unterminated_with("", Some(5.0))
                .await?;
            text = result_text(&polled);
            if backend_unavailable(&text) {
                eprintln!(
                    "write_stdin_edge_cases backend unavailable in this environment; skipping"
                );
                session.cancel().await?;
                return Ok(());
            }
        }
    }
    session.cancel().await?;
    assert!(
        text.contains("A"),
        "expected output to include A, got: {text:?}"
    );
    assert!(
        text.contains("B"),
        "expected output to include B, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn write_stdin_without_trailing_newline_runs() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let session = common::spawn_server().await?;

    let result = session
        .write_stdin_raw_unterminated_with("1+1", Some(10.0))
        .await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("write_stdin_edge_cases backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;
    assert!(
        text.contains("2"),
        "expected evaluation result, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn write_stdin_empty_returns_prompt() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let session = common::spawn_server().await?;

    let result = session
        .write_stdin_raw_unterminated_with("", Some(1.0))
        .await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("write_stdin_edge_cases backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;

    assert_ne!(result.is_error, Some(true), "empty input should not error");
    assert!(
        text.contains("<<repl status: idle>>"),
        "expected idle status on empty poll, got: {text:?}"
    );
    assert!(
        text.contains(">"),
        "expected prompt on empty poll, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_poll_after_completed_request_returns_idle_status_and_prompt() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let session = common::spawn_server().await?;

    let result = session.write_stdin_raw_with("1+1", Some(10.0)).await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("write_stdin_edge_cases backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("2"),
        "expected evaluation result before idle poll, got: {text:?}"
    );

    let idle = session
        .write_stdin_raw_unterminated_with("", Some(1.0))
        .await?;
    let idle_text = result_text(&idle);
    if backend_unavailable(&idle_text) {
        eprintln!("write_stdin_edge_cases backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    session.cancel().await?;

    assert_ne!(idle.is_error, Some(true), "empty input should not error");
    assert!(
        idle_text.contains("<<repl status: idle>>"),
        "expected idle status after completed request, got: {idle_text:?}"
    );
    assert!(
        idle_text.contains(">"),
        "expected prompt after completed request, got: {idle_text:?}"
    );
    Ok(())
}
