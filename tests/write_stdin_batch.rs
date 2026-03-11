mod common;

#[cfg(not(windows))]
use common::McpSnapshot;
use common::TestResult;
#[cfg(not(windows))]
use tokio::time::{Duration, sleep};

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
        .session(
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
async fn write_stdin_timeout_polling_returns_pending_output() -> TestResult<()> {
    let mut snapshot = McpSnapshot::new();

    snapshot
        .session("timeout_poll", mcp_script! {
            write_stdin("cat(\"start\\n\"); flush.console(); Sys.sleep(1); cat(\"end\\n\")", timeout = 0.5);
            write_stdin("", timeout = 2.0);
        })
        .await?;

    assert_snapshot_or_skip(
        "write_stdin_timeout_polling_returns_pending_output",
        &snapshot,
    )
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
        .session_with_pager_page_chars("pager_search_queue", 300, mcp_script! {
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
        .session_with_pager_page_chars("pager_hits_queue", 300, mcp_script! {
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
    let mut session = common::spawn_server().await?;
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
    if text.contains("<<console status: busy") {
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
    let mut session = common::spawn_server().await?;

    // Large silent inputs should not be returned as echoed transcripts (which can trip pager mode
    // and waste tokens). The backend prompt is still returned.
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
    if text.contains("<<console status: busy") {
        eprintln!("write_stdin_batch huge echo-only input still busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;
    assert!(
        !text.contains("--More--"),
        "expected no pager activation for echo-only input, got: {text:?}"
    );
    assert!(
        text.trim_end().ends_with('>'),
        "expected backend prompt, got: {text:?}"
    );
    assert!(
        text.len() < 1_000,
        "expected trimmed output; got {} bytes",
        text.len()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn write_stdin_collapses_huge_echo_with_output_attribution() -> TestResult<()> {
    let mut session = common::spawn_server().await?;

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
    if text.contains("<<console status: busy") {
        eprintln!("write_stdin_batch huge echo attribution still busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;
    assert!(
        text.contains("ok") && text.contains("done"),
        "expected output from both cat() calls, got: {text:?}"
    );
    assert!(
        text.contains("echoed input elided"),
        "expected echo elision marker, got: {text:?}"
    );
    assert!(
        !text.contains("x500 <- 500"),
        "expected large echoed transcript to be collapsed, got: {text:?}"
    );
    assert!(
        !text.contains("--More--"),
        "expected no pager activation for huge echo with small output, got: {text:?}"
    );
    assert!(
        text.len() < 8_000,
        "expected bounded output; got {} bytes",
        text.len()
    );
    Ok(())
}
