#![allow(clippy::await_holding_lock)]

mod common;

use common::McpSnapshot;
use common::TestResult;
use serde_json::json;
use std::sync::{Mutex, MutexGuard, OnceLock};

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

fn collect_text(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|item| match &item.raw {
            rmcp::model::RawContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn count_images(result: &rmcp::model::CallToolResult) -> usize {
    result
        .content
        .iter()
        .filter(|item| matches!(item.raw, rmcp::model::RawContent::Image(_)))
        .count()
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

fn assert_snapshot_or_skip(name: &str, snapshot: &McpSnapshot) -> TestResult<()> {
    let rendered = snapshot.render();
    let transcript = snapshot.render_transcript();
    if backend_unavailable(&rendered) || backend_unavailable(&transcript) {
        eprintln!("write_stdin_batch backend unavailable in this environment; skipping");
        return Ok(());
    }

    insta::assert_snapshot!(name, rendered);
    insta::with_settings!({ snapshot_suffix => "transcript" }, {
        insta::assert_snapshot!(name, transcript);
    });
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn write_stdin_files_multidrain_plot_then_later_stdout_snapshot() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let session = common::spawn_server_with_files().await?;

    let first = session
        .call_tool_raw(
            session.repl_tool_name(),
            json!({
                "input": "plot(1:10)\nSys.sleep(0.5)\ncat('done\\n')\n",
                "timeout_ms": 100
            }),
        )
        .await?;
    let first_text = collect_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("write_stdin_batch backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let second = session
        .call_tool_raw(
            session.repl_tool_name(),
            json!({
                "input": "",
                "timeout_ms": 10000
            }),
        )
        .await?;
    let second_text = collect_text(&second);
    session.cancel().await?;

    let combined_text = format!("{first_text}\n{second_text}");
    let total_images = count_images(&first) + count_images(&second);

    assert!(
        first_text.contains("<<repl status: busy"),
        "expected initial call to time out while the request was still running, got: {first_text:?}"
    );
    assert!(
        !second_text.contains("<<repl status: busy"),
        "expected follow-up poll to finish the timed-out request, got: {second_text:?}"
    );
    assert!(
        combined_text.contains("done"),
        "expected combined responses to include trailing stdout, got: {combined_text:?}"
    );
    assert_eq!(
        total_images, 1,
        "expected exactly one plot image across timeout and poll responses"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn write_stdin_drives_browser() -> TestResult<()> {
    let mut snapshot = McpSnapshot::new();

    snapshot
        .session(
            "browser_queue",
            mcp_script! {
                write_stdin("f <- function() { browser(); x <- 1; x <- x + 1; x }", timeout = 10.0);
                write_stdin("f()", timeout = 10.0);
                write_stdin("n", timeout = 10.0);
                write_stdin("n", timeout = 10.0);
                write_stdin("c", timeout = 10.0);
            },
        )
        .await?;

    assert_snapshot_or_skip("write_stdin_drives_browser", &snapshot)
}

#[tokio::test(flavor = "multi_thread")]
async fn write_stdin_pager_search() -> TestResult<()> {
    let mut snapshot = McpSnapshot::new();

    snapshot
        .pager_session("pager_search_queue", 300, mcp_script! {
            write_stdin("line <- paste(rep(\"x\", 200), collapse = \"\"); for (i in 1:200) cat(sprintf(\"line%04d %s\\n\", i, line))", timeout = 30.0);
            write_stdin(":/line0050", timeout = 30.0);
            write_stdin(":n", timeout = 30.0);
            write_stdin(":q", timeout = 30.0);
        })
        .await?;

    assert_snapshot_or_skip("write_stdin_pager_search", &snapshot)
}

#[tokio::test(flavor = "multi_thread")]
async fn write_stdin_pager_hits() -> TestResult<()> {
    let mut snapshot = McpSnapshot::new();

    snapshot
        .pager_session("pager_hits_queue", 300, mcp_script! {
            write_stdin("line <- paste(rep(\"x\", 200), collapse = \"\"); for (i in 1:200) cat(sprintf(\"line%04d %s\\n\", i, line))", timeout = 30.0);
            write_stdin(":hits line0150", timeout = 30.0);
            write_stdin(":n", timeout = 30.0);
            write_stdin(":q", timeout = 30.0);
        })
        .await?;

    assert_snapshot_or_skip("write_stdin_pager_hits", &snapshot)
}
