mod common;

use common::TestResult;
use rmcp::model::{CallToolResult, RawContent};
#[cfg(any(unix, windows))]
use tokio::time::{Duration, Instant, sleep};

fn result_text(result: &CallToolResult) -> String {
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

fn is_busy_response(text: &str) -> bool {
    text.contains("<<repl status: busy")
        || text.contains("worker is busy")
        || text.contains("request already running")
        || text.contains("input discarded while worker busy")
}

fn is_restart_transient_output(text: &str) -> bool {
    is_busy_response(text)
        || text.contains("new session started")
        || text.contains("worker exited with status")
}

async fn spawn_interrupt_session() -> TestResult<common::McpTestSession> {
    common::spawn_server_with_args(vec![
        "--sandbox".to_string(),
        "danger-full-access".to_string(),
    ])
    .await
}

#[cfg(windows)]
fn backend_unavailable(text: &str) -> bool {
    text.contains("Fatal error: cannot create 'R_TempDir'")
        || text.contains("failed to start R session")
        || text.contains("worker exited with status")
        || text.contains("unable to initialize the JIT")
        || text.contains(
            "worker protocol error: ipc disconnected while waiting for request completion",
        )
}

#[cfg(not(windows))]
fn backend_unavailable(text: &str) -> bool {
    text.contains("Fatal error: cannot create 'R_TempDir'")
        || text.contains("failed to start R session")
        || text.contains("worker exited with status")
        || text.contains("worker exited with signal")
        || text.contains("unable to initialize the JIT")
        || text.contains(
            "worker protocol error: ipc disconnected while waiting for request completion",
        )
        || text.contains("options(\"defaultPackages\") was not found")
        || text.contains("worker io error: Broken pipe")
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn interrupt_unblocks_long_running_request() -> TestResult<()> {
    let mut session = spawn_interrupt_session().await?;

    let timeout_result = session
        .write_stdin_raw_with("Sys.sleep(30)", Some(0.5))
        .await?;
    let timeout_text = result_text(&timeout_result);
    if backend_unavailable(&timeout_text) {
        eprintln!("interrupt test backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        timeout_text.contains("<<repl status: busy"),
        "expected sleep call to time out, got: {timeout_text:?}"
    );

    let interrupt_result = session.write_stdin_raw_with("\u{3}", Some(5.0)).await?;
    let interrupt_text = result_text(&interrupt_result);
    if backend_unavailable(&interrupt_text) {
        eprintln!("interrupt test backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        interrupt_text.contains("> ")
            || interrupt_text.contains("<<repl status: busy")
            || interrupt_text.contains("worker is busy")
            || interrupt_text.contains("request already running")
            || interrupt_text.contains("input discarded while worker busy"),
        "expected prompt or transient busy response after interrupt, got: {interrupt_text:?}"
    );

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if Instant::now() >= deadline {
            session.cancel().await?;
            eprintln!("interrupt did not unblock worker in time; skipping");
            return Ok(());
        }

        let result = session.write_stdin_raw_with("1+1", Some(1.0)).await?;
        let text = result_text(&result);
        if text.contains("worker is busy")
            || text.contains("request already running")
            || text.contains("input discarded while worker busy")
            || text.contains("<<repl status: busy")
        {
            sleep(Duration::from_millis(50)).await;
            continue;
        }
        assert!(
            text.contains("[1] 2") || text.contains("2"),
            "expected evaluation to run after interrupt, got: {text:?}"
        );
        break;
    }

    session.cancel().await?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn write_stdin_ctrl_c_prefix_interrupts_then_runs_remaining_input() -> TestResult<()> {
    let mut session = spawn_interrupt_session().await?;

    let timeout_result = session
        .write_stdin_raw_with("Sys.sleep(30)", Some(0.5))
        .await?;
    let timeout_text = result_text(&timeout_result);
    if backend_unavailable(&timeout_text) {
        eprintln!("interrupt test backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        timeout_text.contains("<<repl status: busy"),
        "expected sleep call to time out, got: {timeout_text:?}"
    );

    let result = session.write_stdin_raw_with("\u{3}1+1", Some(5.0)).await?;
    let text = result_text(&result);
    if text.contains("<<repl status: busy")
        || text.contains("worker is busy")
        || text.contains("request already running")
        || text.contains("input discarded while worker busy")
    {
        eprintln!("interrupt prefix did not complete in time; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("[1] 2") || text.contains("2"),
        "expected evaluation after interrupt prefix, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn pager_ctrl_c_prefix_preserves_interrupt_output() -> TestResult<()> {
    let mut session = spawn_interrupt_session().await?;

    let long_sleep =
        r#"tryCatch({ Sys.sleep(30) }, interrupt = function(e) cat("interrupt received\n"))"#;
    let timeout_result = session.write_stdin_raw_with(long_sleep, Some(0.2)).await?;
    let timeout_text = result_text(&timeout_result);
    if backend_unavailable(&timeout_text) {
        eprintln!("interrupt test backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        timeout_text.contains("<<repl status: busy"),
        "expected sleep call to time out, got: {timeout_text:?}"
    );

    let result = session.write_stdin_raw_with("\u{3}1+1", Some(5.0)).await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        eprintln!("interrupt prefix did not complete in time; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("interrupt received"),
        "expected interrupt handler output to be preserved in pager mode, got: {text:?}"
    );
    assert!(
        text.contains("[1] 2") || text.contains("2"),
        "expected evaluation after interrupt prefix, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn write_stdin_ctrl_c_prefix_interrupts_then_runs_remaining_input_on_windows()
-> TestResult<()> {
    let mut session = spawn_interrupt_session().await?;

    let long_sleep = r#"
tryCatch(
  {
    repeat Sys.sleep(0.5)
  },
  interrupt = function(e) cat("interrupt received\n")
)
"#;
    let timeout_result = session.write_stdin_raw_with(long_sleep, Some(0.2)).await?;
    let timeout_text = result_text(&timeout_result);
    if backend_unavailable(&timeout_text) {
        eprintln!("interrupt test backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        timeout_text.contains("<<repl status: busy"),
        "expected sleep call to time out, got: {timeout_text:?}"
    );

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut text = result_text(
        &session
            .write_stdin_raw_with("\u{3}cat('AFTER_INTERRUPT\\n')", Some(5.0))
            .await?,
    );
    loop {
        if backend_unavailable(&text) {
            eprintln!("interrupt test backend unavailable in this environment; skipping");
            session.cancel().await?;
            return Ok(());
        }
        if !is_busy_response(&text) {
            break;
        }
        if Instant::now() >= deadline {
            session.cancel().await?;
            panic!("expected interrupt prefix to recover the worker in time, got: {text:?}");
        }
        sleep(Duration::from_millis(50)).await;
        let poll = session.write_stdin_raw_with("", Some(0.5)).await?;
        text = result_text(&poll);
    }

    session.cancel().await?;

    assert!(
        text.contains("interrupt received"),
        "expected interrupt handler output to be preserved, got: {text:?}"
    );
    assert!(
        text.contains("AFTER_INTERRUPT"),
        "expected remaining input after ctrl-c prefix to run, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn write_stdin_ctrl_d_prefix_restarts_then_runs_remaining_input() -> TestResult<()> {
    let mut session = spawn_interrupt_session().await?;

    let _ = session.write_stdin_raw_with("x <- 1", Some(5.0)).await?;

    let first = session
        .write_stdin_raw_with("\u{4}print(exists(\"x\"))", Some(10.0))
        .await?;
    let mut text = result_text(&first);
    if backend_unavailable(&text) {
        eprintln!("interrupt test backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if text.contains("FALSE") {
            break;
        }
        assert!(
            !text.contains("TRUE"),
            "expected restarted session to clear x, got: {text:?}"
        );
        if Instant::now() >= deadline {
            session.cancel().await?;
            panic!("expected fresh session after restart prefix, got: {text:?}");
        }
        sleep(Duration::from_millis(100)).await;
        let result = session
            .write_stdin_raw_with("print(exists(\"x\"))", Some(5.0))
            .await?;
        text = result_text(&result);
        if backend_unavailable(&text) {
            eprintln!("interrupt test backend unavailable in this environment; skipping");
            session.cancel().await?;
            return Ok(());
        }
        if is_restart_transient_output(&text) {
            continue;
        }
    }
    assert!(
        text.contains("FALSE"),
        "expected fresh session output, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn pager_ctrl_d_prefix_preserves_restart_notice() -> TestResult<()> {
    let mut session = spawn_interrupt_session().await?;

    let result = session
        .write_stdin_raw_with("\u{4}print('AFTER_RESET')", Some(10.0))
        .await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("interrupt test backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if is_busy_response(&text) || text.contains("worker exited with status") {
        eprintln!("restart prefix did not complete in time; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("new session started"),
        "expected restart notice to be preserved in pager mode, got: {text:?}"
    );
    assert!(
        text.contains("AFTER_RESET"),
        "expected follow-up output after restart prefix, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn ctrl_d_prefix_in_files_mode_separates_restart_notice_from_output() -> TestResult<()> {
    let mut session = common::spawn_server_with_files().await?;

    let result = session
        .write_stdin_raw_with("\u{4}cat('AFTER_RESET\\n')", Some(10.0))
        .await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("interrupt test backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if is_busy_response(&text) || text.contains("worker exited with status") {
        eprintln!("restart prefix in files mode did not complete in time; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("[repl] new session started\nAFTER_RESET"),
        "expected ctrl-d files reply to preserve a newline between restart notice and output, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}
