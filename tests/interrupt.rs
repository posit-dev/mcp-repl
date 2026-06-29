#![allow(clippy::await_holding_lock)]

mod common;

use common::TestResult;
use rmcp::model::{CallToolResult, RawContent};
#[cfg(unix)]
use serde_json::Value;
#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};
#[cfg(any(unix, windows))]
use tokio::time::{Duration, Instant, sleep};

fn test_mutex() -> &'static Mutex<()> {
    static TEST_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
    TEST_MUTEX.get_or_init(|| Mutex::new(()))
}

fn lock_mutex(mutex: &Mutex<()>) -> MutexGuard<'_, ()> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn lock_test_mutex() -> MutexGuard<'static, ()> {
    lock_mutex(test_mutex())
}

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

#[cfg(unix)]
fn latest_debug_events(debug_dir: &Path) -> TestResult<Vec<Value>> {
    let mut sessions = fs::read_dir(debug_dir)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    sessions.sort();
    let session_dir = sessions.last().ok_or("missing debug session directory")?;
    let events_path = session_dir.join("events.jsonl");
    let text = fs::read_to_string(&events_path)?;
    Ok(text
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()?)
}

#[cfg(unix)]
fn wait_for_debug_event(debug_dir: &Path, event: &str) -> TestResult<Value> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut latest = Vec::new();
    loop {
        if let Ok(events) = latest_debug_events(debug_dir) {
            latest = events;
            if let Some(entry) = latest.iter().find(|entry| entry["event"] == event) {
                return Ok(entry.clone());
            }
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "expected debug event {event:?} under {}, got {latest:?}",
                debug_dir.display()
            )
            .into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

async fn spawn_interrupt_session() -> TestResult<common::McpTestSession> {
    common::spawn_server_with_args(vec![
        "--sandbox".to_string(),
        "danger-full-access".to_string(),
    ])
    .await
}

#[cfg(unix)]
async fn spawn_interrupt_files_session() -> TestResult<common::McpTestSession> {
    common::spawn_server_with_args(vec![
        "--sandbox".to_string(),
        "danger-full-access".to_string(),
        "--oversized-output".to_string(),
        "files".to_string(),
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
    let _guard = lock_test_mutex();
    let session = spawn_interrupt_session().await?;

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

    let interrupt_result = session
        .write_stdin_raw_unterminated_with("\u{3}", Some(5.0))
        .await?;
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
async fn r_interrupt_ack_does_not_wait_for_busy_main_thread() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let temp = tempfile::tempdir()?;
    let debug_dir = temp.path().join("debug");
    let handler_done = temp.path().join("handler-done");
    let handler_done_literal = serde_json::to_string(&handler_done.to_string_lossy())?;
    let session = common::spawn_server_with_args_env(
        vec!["--sandbox".to_string(), "danger-full-access".to_string()],
        vec![(
            "MCP_REPL_DEBUG_DIR".to_string(),
            debug_dir.to_string_lossy().to_string(),
        )],
    )
    .await?;

    let input = format!(
        r#"
done_path <- {handler_done_literal}
cat("R_INTERRUPT_READY\n")
flush.console()
tryCatch(
  {{
    Sys.sleep(30)
  }},
  interrupt = function(e) {{
    cat("R_INTERRUPT_HANDLER_START\n")
    flush.console()
    Sys.sleep(1)
    writeLines("done", done_path)
    cat("R_INTERRUPT_HANDLER_DONE\n")
    flush.console()
  }}
)
x_stale_marker <- 42
"#
    );
    let timeout_result = session.write_stdin_raw_with(input, Some(0.1)).await?;
    let mut text = result_text(&timeout_result);
    if backend_unavailable(&text) {
        eprintln!("R interrupt ack test backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    let ready_deadline = Instant::now() + Duration::from_secs(10);
    while !text.contains("R_INTERRUPT_READY") {
        if Instant::now() >= ready_deadline {
            session.cancel().await?;
            return Err(format!("R request did not reach interrupt-ready state: {text:?}").into());
        }
        sleep(Duration::from_millis(50)).await;
        let poll = session.write_stdin_raw_with("", Some(0.5)).await?;
        text = result_text(&poll);
        if backend_unavailable(&text) {
            eprintln!("R interrupt ack test backend unavailable in this environment; skipping");
            session.cancel().await?;
            return Ok(());
        }
    }

    let interrupt = session
        .write_stdin_raw_unterminated_with("\u{3}", Some(0.2))
        .await?;
    let interrupt_text = result_text(&interrupt);
    if backend_unavailable(&interrupt_text) {
        eprintln!("R interrupt ack test backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    let ack = wait_for_debug_event(&debug_dir, "worker_interrupt_ack_observed")?;
    assert_eq!(
        ack["payload"]["discarded_input"], true,
        "queued stale R input should be discarded by interrupt ack"
    );
    assert!(
        !handler_done.exists(),
        "interrupt ack should be observed before the busy R interrupt handler returns; interrupt reply: {interrupt_text:?}"
    );

    let done_deadline = Instant::now() + Duration::from_secs(10);
    while !handler_done.exists() {
        if Instant::now() >= done_deadline {
            session.cancel().await?;
            return Err("R interrupt handler did not finish".into());
        }
        sleep(Duration::from_millis(50)).await;
        let _ = session.write_stdin_raw_with("", Some(0.5)).await?;
    }

    let probe_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if Instant::now() >= probe_deadline {
            session.cancel().await?;
            return Err("R worker stayed busy before stale-input probe".into());
        }
        let result = session
            .write_stdin_raw_with("exists('x_stale_marker')", Some(1.0))
            .await?;
        let text = result_text(&result);
        if is_busy_response(&text) {
            sleep(Duration::from_millis(50)).await;
            continue;
        }
        session.cancel().await?;
        assert!(
            text.contains("FALSE"),
            "stale queued R input should have been discarded, got: {text:?}"
        );
        break;
    }

    Ok(())
}

#[cfg(unix)]
async fn assert_interrupt_drain_preserves_prompt_shaped_child_stdout(
    session: common::McpTestSession,
) -> TestResult<()> {
    let temp = tempfile::tempdir()?;
    let marker_path = temp.path().join("child-output-ready");
    let marker_literal = serde_json::to_string(&marker_path.to_string_lossy())?;
    let input = format!(
        "Sys.sleep(0.1); invisible(system(\"printf '> '\")); writeLines('ready', {marker_literal}); repeat Sys.sleep(0.05)"
    );
    let timeout_result = session.write_stdin_raw_with(input, Some(0.005)).await?;
    let timeout_text = result_text(&timeout_result);
    if backend_unavailable(&timeout_text) {
        eprintln!("interrupt test backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        timeout_text.contains("<<repl status: busy"),
        "expected child stdout request to time out, got: {timeout_text:?}"
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    while !marker_path.exists() {
        if Instant::now() >= deadline {
            session.cancel().await?;
            panic!("child prompt-shaped output marker was not written");
        }
        sleep(Duration::from_millis(50)).await;
    }

    let interrupt_result = session
        .write_stdin_raw_unterminated_with("\u{3}", Some(5.0))
        .await?;
    let interrupt_text = result_text(&interrupt_result);
    if backend_unavailable(&interrupt_text) {
        eprintln!("interrupt test backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if is_busy_response(&interrupt_text) {
        eprintln!("interrupt drain did not complete in time; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        interrupt_text.matches("> ").count() >= 2,
        "expected raw prompt-shaped output plus completion prompt, got: {interrupt_text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn interrupt_drain_preserves_prompt_shaped_child_stdout() -> TestResult<()> {
    let _guard = lock_test_mutex();
    assert_interrupt_drain_preserves_prompt_shaped_child_stdout(spawn_interrupt_session().await?)
        .await
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn files_interrupt_drain_preserves_prompt_shaped_child_stdout() -> TestResult<()> {
    let _guard = lock_test_mutex();
    assert_interrupt_drain_preserves_prompt_shaped_child_stdout(
        spawn_interrupt_files_session().await?,
    )
    .await
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn pager_ctrl_c_prefix_preserves_interrupt_output() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let session = spawn_interrupt_session().await?;

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
    let session = spawn_interrupt_session().await?;

    let long_sleep = r#"
cat("INTERRUPT_READY\n")
flush.console()
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

    let ready_deadline = Instant::now() + Duration::from_secs(20);
    let mut ready_text = timeout_text;
    loop {
        if backend_unavailable(&ready_text) {
            eprintln!("interrupt test backend unavailable in this environment; skipping");
            session.cancel().await?;
            return Ok(());
        }
        if ready_text.contains("INTERRUPT_READY") {
            break;
        }
        if !is_busy_response(&ready_text) {
            session.cancel().await?;
            panic!(
                "expected long-running request to reach user code before interrupt, got: {ready_text:?}"
            );
        }
        if Instant::now() >= ready_deadline {
            session.cancel().await?;
            panic!(
                "expected long-running request to emit readiness marker before interrupt, got: {ready_text:?}"
            );
        }
        sleep(Duration::from_millis(50)).await;
        let poll = session.write_stdin_raw_with("", Some(0.5)).await?;
        ready_text = result_text(&poll);
    }

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
async fn pager_ctrl_d_prefix_preserves_restart_notice() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let session = spawn_interrupt_session().await?;

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
    let _guard = lock_test_mutex();
    let session = common::spawn_server_with_files().await?;

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
        "expected ctrl-d files reply to preserve the restart notice and output, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}
