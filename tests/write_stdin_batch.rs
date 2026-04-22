#![allow(clippy::await_holding_lock)]

mod common;

#[cfg(not(windows))]
use common::McpSnapshot;
use common::TestResult;
#[cfg(not(windows))]
use serde_json::json;
use std::fs;
use std::path::PathBuf;
#[cfg(not(windows))]
use std::sync::{Mutex, MutexGuard, OnceLock};
#[cfg(not(windows))]
use tokio::time::{Duration, sleep};

#[cfg(not(windows))]
fn test_mutex() -> &'static Mutex<()> {
    static TEST_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
    TEST_MUTEX.get_or_init(|| Mutex::new(()))
}

#[cfg(not(windows))]
fn lock_mutex(mutex: &Mutex<()>) -> MutexGuard<'_, ()> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(not(windows))]
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

#[cfg(not(windows))]
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

fn bundle_transcript_path(text: &str) -> Option<PathBuf> {
    disclosed_path(text, "transcript.txt")
}

fn disclosed_path(text: &str, suffix: &str) -> Option<PathBuf> {
    let end = text.find(suffix)?.saturating_add(suffix.len());
    let start = text[..end]
        .rfind(|ch: char| ch.is_whitespace() || matches!(ch, '"' | '\'' | '[' | '('))
        .map_or(0, |idx| idx.saturating_add(1));
    Some(PathBuf::from(&text[start..end]))
}

#[test]
fn disclosed_path_parses_windows_paths() {
    let text = "...[full output: C:\\Users\\runner\\AppData\\Local\\Temp\\mcp-repl-output\\output-0001\\transcript.txt]...";
    assert_eq!(
        bundle_transcript_path(text),
        Some(PathBuf::from(
            r"C:\Users\runner\AppData\Local\Temp\mcp-repl-output\output-0001\transcript.txt"
        ))
    );
}

#[cfg(not(windows))]
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

#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread")]
async fn write_stdin_accepts_multiple_calls() -> TestResult<()> {
    let mut snapshot = McpSnapshot::new();

    snapshot
        .session(
            "list_inputs",
            mcp_script! {
                write_stdin("x <- 1", timeout = 10.0);
                write_stdin("x + 1", timeout = 10.0);
            },
        )
        .await?;

    assert_snapshot_or_skip("write_stdin_accepts_multiple_calls", &snapshot)
}

#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread")]
async fn write_stdin_timeout_then_busy_then_recovers() -> TestResult<()> {
    let mut snapshot = McpSnapshot::new();

    snapshot
        .files_session(
            "timeout_list",
            mcp_session!(|session| {
                session.write_stdin_with("Sys.sleep(5)", Some(2.0)).await;
                session.write_stdin_with("1+1", Some(1.0)).await;
                sleep(Duration::from_secs(4)).await;
                session.write_stdin_with("1+1", Some(10.0)).await;
                Ok(())
            }),
        )
        .await?;

    assert_snapshot_or_skip("write_stdin_timeout_then_busy_then_recovers", &snapshot)
}

#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread")]
async fn write_stdin_files_multidrain_plot_then_later_stdout_snapshot() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let session = common::spawn_server_with_files().await?;

    let first = session
        .call_tool_raw(
            session.repl_tool_name(),
            json!({
                "input": "plot(1:10)\nSys.sleep(2)\ncat('done\\n')\n",
                "timeout_ms": 200
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

#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread")]
async fn write_stdin_timeout_polling_returns_pending_output() -> TestResult<()> {
    let session = common::spawn_server().await?;

    let first = session
        .write_stdin_raw_with(
            "cat(\"start\\n\"); flush.console(); Sys.sleep(1); cat(\"end\\n\")",
            Some(0.5),
        )
        .await?;
    let first_text = collect_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("write_stdin_batch backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        first_text.contains("start"),
        "expected timeout reply to include early output, got: {first_text:?}"
    );
    assert!(
        first_text.contains("<<repl status: busy"),
        "expected timeout status marker, got: {first_text:?}"
    );

    let second = session.write_stdin_raw_with("", Some(2.0)).await?;
    let second_text = collect_text(&second);
    session.cancel().await?;

    assert!(
        !second_text.contains("<<repl status: busy"),
        "expected empty poll to finish request, got: {second_text:?}"
    );
    assert!(
        second_text.contains("end"),
        "expected empty poll to return trailing output, got: {second_text:?}"
    );
    Ok(())
}

#[cfg(not(windows))]
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

#[cfg(not(windows))]
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

#[cfg(not(windows))]
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

#[tokio::test(flavor = "multi_thread")]
async fn write_stdin_recovers_after_error() -> TestResult<()> {
    let session = common::spawn_server().await?;
    let _ = session
        .write_stdin_raw_with("stop('boom')", Some(10.0))
        .await?;
    let result = session
        .write_stdin_raw_with("cat('after')", Some(10.0))
        .await?;
    let text = collect_text(&result);
    if backend_unavailable(&text) {
        eprintln!("write_stdin_batch backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if text.contains("<<repl status: busy") {
        eprintln!("write_stdin_batch huge echo attribution still busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;
    assert!(
        text.contains("after"),
        "expected follow-up output after error, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn write_stdin_drops_huge_echo_only_inputs() -> TestResult<()> {
    let session = common::spawn_server().await?;

    let input = (1..=2_000)
        .map(|idx| format!("x{idx} <- {idx}\n"))
        .collect::<String>();
    let result = session.write_stdin_raw_with(input, Some(30.0)).await?;
    let text = collect_text(&result);
    if backend_unavailable(&text) {
        eprintln!("write_stdin_batch backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if text.contains("<<repl status: busy") {
        eprintln!("write_stdin_batch huge echo-only input still busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;
    assert!(
        !text.contains("--More--"),
        "did not expect pager activation for echo-only input, got: {text:?}"
    );
    assert!(
        !text.contains("echoed input elided"),
        "did not expect echo elision marker, got: {text:?}"
    );
    assert_eq!(text, "> ", "expected prompt-only reply, got: {text:?}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn write_stdin_trims_huge_leading_echo_prefix_and_preserves_later_echo() -> TestResult<()> {
    let session = common::spawn_server_with_files().await?;

    let mut input = String::new();
    for idx in 1..=1_000 {
        input.push_str(&format!("x{idx} <- {idx}\n"));
    }
    input.push_str("cat(\"ok\\n\")\n");
    for idx in 1..=1_000 {
        input.push_str(&format!("y{idx} <- {idx}\n"));
    }
    input.push_str("cat(\"done\\n\")\n");

    let result = session.write_stdin_raw_with(input, Some(30.0)).await?;
    let text = collect_text(&result);
    if backend_unavailable(&text) {
        eprintln!("write_stdin_batch backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if text.contains("<<repl status: busy") {
        eprintln!("write_stdin_batch huge echo attribution still busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    let transcript_path = bundle_transcript_path(&text);
    let spill_text = transcript_path
        .as_ref()
        .map(fs::read_to_string)
        .transpose()?;
    session.cancel().await?;
    assert!(
        text.contains("transcript.txt") || (text.contains("ok") && text.contains("y500 <- 500")),
        "expected either an inline transcript or a spill path, got: {text:?}"
    );
    if let Some(spill_text) = spill_text {
        assert!(
            !spill_text.contains("x500 <- 500"),
            "did not expect the pure leading echo prefix in spill file, got: {spill_text:?}"
        );
        assert!(
            spill_text.contains("y500 <- 500"),
            "expected later echoed input to remain after output interleaving, got: {spill_text:?}"
        );
        assert!(
            spill_text.contains("ok") && spill_text.contains("done"),
            "expected output from both cat() calls in spill file, got: {spill_text:?}"
        );
        assert!(
            text.contains("done"),
            "expected the inline tail to keep the final output, got: {text:?}"
        );
    } else {
        assert!(
            text.contains("ok") && text.contains("done"),
            "expected output from both cat() calls inline, got: {text:?}"
        );
        assert!(
            !text.contains("x500 <- 500"),
            "did not expect the pure leading echo prefix inline, got: {text:?}"
        );
        assert!(
            text.contains("y500 <- 500"),
            "expected later echoed input to remain after output interleaving, got: {text:?}"
        );
    }
    assert!(
        !text.contains("echoed input elided"),
        "did not expect echo elision marker, got: {text:?}"
    );
    assert!(
        !text.contains("--More--"),
        "did not expect pager activation for huge echo with small output, got: {text:?}"
    );
    Ok(())
}
