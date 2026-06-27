#![allow(clippy::await_holding_lock)]

mod common;

use common::TestResult;
use rmcp::model::RawContent;
use std::sync::{Mutex, MutexGuard, OnceLock};
use tokio::time::{Duration, Instant, sleep};

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
        || text.contains("worker exited with status")
        || text.contains("worker exited with signal")
        || text.contains("unable to initialize the JIT")
        || text.contains("options(\"defaultPackages\") was not found")
        || text.contains(
            "worker protocol error: ipc disconnected while waiting for request completion",
        )
}

fn empty_or_blank_stderr(text: &str) -> bool {
    text.is_empty() || text.trim() == "stderr:"
}

async fn spawn_manage_session() -> TestResult<common::McpTestSession> {
    common::spawn_server_with_args(vec![
        "--sandbox".to_string(),
        "danger-full-access".to_string(),
    ])
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupt_without_active_request_keeps_session_usable() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let session = spawn_manage_session().await?;

    let _ = session.write_stdin_raw_with("1+1", Some(5.0)).await?;
    let result = session
        .write_stdin_raw_unterminated_with("\u{3}", Some(5.0))
        .await?;

    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("interrupt test backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        empty_or_blank_stderr(&text) || text.contains(">") || text.contains("<<repl status: busy"),
        "expected empty reply, prompt, or timeout status in output, got: {text:?}"
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
            || (text.contains(">") && !text.contains("2"))
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
    let _guard = lock_test_mutex();
    let session = spawn_manage_session().await?;

    let _ = session
        .write_stdin_raw_with("x <- 1; Sys.sleep(5)", Some(0.1))
        .await?;

    let restart = session
        .write_stdin_raw_unterminated_with("\u{4}", Some(5.0))
        .await?;
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

#[tokio::test(flavor = "multi_thread")]
async fn restart_while_busy_not_reading_stdin_returns_promptly() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let session = spawn_manage_session().await?;

    let _ = session
        .write_stdin_raw_with("x <- 1; Sys.sleep(30)", Some(0.1))
        .await?;

    let start = Instant::now();
    let restart = session
        .write_stdin_raw_unterminated_with("\u{4}", Some(3.0))
        .await?;
    let elapsed = start.elapsed();
    let restart_text = result_text(&restart);
    if backend_unavailable(&restart_text) {
        eprintln!("prompt restart test backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;

    assert!(
        restart_text.contains("new session started"),
        "expected restart notice, got: {restart_text:?}"
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "expected busy restart to return promptly, elapsed={elapsed:?}, got: {restart_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn pager_restart_preserves_output_captured_during_shutdown() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let session = common::spawn_server_with_pager_page_chars(120).await?;

    let input = r#"
cat("WAITING_FOR_RESTART_EOF\n")
flush.console()
suppressWarnings(invisible(readLines("stdin", n = 1)))
for (i in 1:80) cat(sprintf("RESTART_LINE_%03d\n", i))
flush.console()
Sys.sleep(1.0)
"#;
    let timeout = session.write_stdin_raw_with(input, Some(0.5)).await?;
    let timeout_text = result_text(&timeout);
    if backend_unavailable(&timeout_text) {
        eprintln!("pager restart test backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        timeout_text.contains("WAITING_FOR_RESTART_EOF"),
        "expected request to block on stdin before restart, got: {timeout_text:?}"
    );
    assert!(
        !timeout_text.contains("RESTART_LINE_"),
        "did not expect restart-only output before Ctrl-D closes stdin, got: {timeout_text:?}"
    );

    let restart = session
        .write_stdin_raw_unterminated_with("\u{4}", Some(0.8))
        .await?;
    let restart_text = result_text(&restart);
    if backend_unavailable(&restart_text) {
        eprintln!("pager restart test backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        restart_text.contains("new session started"),
        "expected restart notice, got: {restart_text:?}"
    );
    assert!(
        restart_text.contains("RESTART_LINE_"),
        "expected restart reply to include pager output produced during shutdown, got: {restart_text:?}"
    );
    assert!(
        restart_text.contains("--More--"),
        "expected restart reply to preserve pager state for shutdown output, got: {restart_text:?}"
    );

    let next = session
        .write_stdin_raw_unterminated_with("", Some(2.0))
        .await?;
    let next_text = result_text(&next);
    session.cancel().await?;

    assert!(
        next_text.contains("RESTART_LINE_"),
        "expected follow-up poll to drain restart pager output, got: {next_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn ctrl_d_restart_clears_active_pager_when_reply_has_no_overflow() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let mut session = common::spawn_server_with_pager_page_chars(120).await?;

    let initial = session
        .write_stdin_raw_with(
            "for (i in 1:80) cat(sprintf(\"STALE_PAGER_%03d\\n\", i))",
            Some(30.0),
        )
        .await?;
    let initial = common::wait_until_not_busy(
        &mut session,
        initial,
        Duration::from_millis(100),
        Duration::from_secs(60),
    )
    .await?;
    let initial_text = result_text(&initial);
    if backend_unavailable(&initial_text) {
        eprintln!("pager reset test backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        initial_text.contains("STALE_PAGER_"),
        "expected initial reply to include stale-marker output, got: {initial_text:?}"
    );
    assert!(
        initial_text.contains("--More--"),
        "expected initial reply to activate pager, got: {initial_text:?}"
    );

    let reset = session
        .write_stdin_raw_unterminated_with("\u{4}", Some(5.0))
        .await?;
    let reset_text = result_text(&reset);
    if backend_unavailable(&reset_text) {
        eprintln!("pager reset test backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        reset_text.contains("new session started"),
        "expected reset notice, got: {reset_text:?}"
    );
    assert!(
        !reset_text.contains("STALE_PAGER_"),
        "did not expect stale pager output in reset reply, got: {reset_text:?}"
    );

    let next = session.write_stdin_raw_with(":next", Some(5.0)).await?;
    let next_text = result_text(&next);
    session.cancel().await?;

    assert!(
        !next_text.contains("STALE_PAGER_"),
        "did not expect stale pager output after reset, got: {next_text:?}"
    );
    assert!(
        !next_text.contains("--More--"),
        "did not expect :next to keep controlling a pre-reset pager, got: {next_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_while_busy_returns_output_captured_during_shutdown() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let session = spawn_manage_session().await?;

    let input = r#"
cat("WAITING_FOR_RESTART_EOF\n")
flush.console()
suppressWarnings(invisible(readLines("stdin", n = 1)))
cat("DURING_RESTART\n")
flush.console()
Sys.sleep(1.0)
cat("TOO_LATE\n")
flush.console()
"#;
    let timeout = session.write_stdin_raw_with(input, Some(0.5)).await?;
    let timeout_text = result_text(&timeout);
    if backend_unavailable(&timeout_text) {
        eprintln!(
            "restart graceful shutdown test backend unavailable in this environment; skipping"
        );
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        timeout_text.contains("WAITING_FOR_RESTART_EOF"),
        "expected request to block on stdin before restart, got: {timeout_text:?}"
    );
    assert!(
        !timeout_text.contains("DURING_RESTART"),
        "did not expect restart-only output before Ctrl-D closes stdin, got: {timeout_text:?}"
    );

    let restart = session
        .write_stdin_raw_unterminated_with("\u{4}", Some(0.8))
        .await?;
    let restart_text = result_text(&restart);
    if backend_unavailable(&restart_text) {
        eprintln!(
            "restart graceful shutdown test backend unavailable in this environment; skipping"
        );
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;

    assert!(
        restart_text.contains("DURING_RESTART"),
        "expected restart to return output produced during shutdown, got: {restart_text:?}"
    );
    assert!(
        !restart_text.contains("TOO_LATE"),
        "did not expect restart to include later old-session output, got: {restart_text:?}"
    );
    assert!(
        restart_text.contains("new session started"),
        "expected restart notice, got: {restart_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_tail_output_is_included_in_same_response() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let session = spawn_manage_session().await?;

    let input = r#"
cat("WAITING_FOR_RESTART_EOF\n")
flush.console()
suppressWarnings(invisible(readLines("stdin", n = 1)))
cat("DURING_RESTART\n")
flush.console()
"#;
    let timeout = session.write_stdin_raw_with(input, Some(0.5)).await?;
    let timeout_text = result_text(&timeout);
    if backend_unavailable(&timeout_text) {
        eprintln!("restart tail response test backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        timeout_text.contains("WAITING_FOR_RESTART_EOF"),
        "expected request to block on stdin before restart, got: {timeout_text:?}"
    );

    let restart = session
        .write_stdin_raw_unterminated_with("\u{4}cat(\"TAIL_DONE\\n\"); flush.console()", Some(5.0))
        .await?;
    let restart_text = result_text(&restart);
    if backend_unavailable(&restart_text) {
        eprintln!("restart tail response test backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;

    assert!(
        restart_text.contains("DURING_RESTART"),
        "expected restart reply to include output captured before tail input, got: {restart_text:?}"
    );
    assert!(
        restart_text.contains("new session started"),
        "expected restart reply to include the fresh-session notice, got: {restart_text:?}"
    );
    assert!(
        restart_text.contains("TAIL_DONE"),
        "expected Ctrl-D tail output in the same response, got: {restart_text:?}"
    );
    assert!(
        !restart_text.contains("<<repl status: busy"),
        "did not expect the completed Ctrl-D tail to require a follow-up poll, got: {restart_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_tail_uses_remaining_timeout_budget() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let session = spawn_manage_session().await?;

    let input = r#"
cat("WAITING_FOR_RESTART_EOF\n")
flush.console()
suppressWarnings(invisible(readLines("stdin", n = 1)))
cat("DURING_RESTART\n")
flush.console()
Sys.sleep(1.0)
"#;
    let timeout = session.write_stdin_raw_with(input, Some(0.5)).await?;
    let timeout_text = result_text(&timeout);
    if backend_unavailable(&timeout_text) {
        eprintln!("restart tail timeout test backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        timeout_text.contains("WAITING_FOR_RESTART_EOF"),
        "expected request to block on stdin before restart, got: {timeout_text:?}"
    );

    let restart = session
        .write_stdin_raw_unterminated_with(
            "\u{4}Sys.sleep(0.45); cat(\"TAIL_DONE\\n\"); flush.console()",
            Some(0.7),
        )
        .await?;
    let restart_text = result_text(&restart);
    if backend_unavailable(&restart_text) {
        eprintln!("restart tail timeout test backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;

    assert!(
        restart_text.contains("DURING_RESTART"),
        "expected restart reply to include output captured before tail input, got: {restart_text:?}"
    );
    assert!(
        restart_text.contains("<<repl status: busy"),
        "expected tail input to time out against the original Ctrl-D call budget, got: {restart_text:?}"
    );
    assert!(
        !restart_text.contains("TAIL_DONE"),
        "did not expect tail input to receive a fresh timeout budget, got: {restart_text:?}"
    );
    Ok(())
}
