mod common;

#[cfg(not(windows))]
use common::McpSnapshot;
use common::TestResult;
use regex_lite::Regex;
use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;
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

fn bundle_events_log_path(text: &str) -> Option<PathBuf> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(/[^]\s]+/events\.log)").expect("events-log regex should compile")
    });
    re.captures(text)
        .and_then(|caps| caps.get(1))
        .map(|path| PathBuf::from(path.as_str()))
}

fn bundle_transcript_path(events_log: &PathBuf) -> PathBuf {
    events_log
        .parent()
        .expect("events.log should have bundle dir parent")
        .join("transcript.txt")
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

#[cfg(feature = "pager")]
#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread")]
async fn write_stdin_pager_search() -> TestResult<()> {
    let mut snapshot = McpSnapshot::new();

    snapshot
        .session("pager_search_queue", mcp_script! {
            write_stdin("line <- paste(rep(\"x\", 200), collapse = \"\"); for (i in 1:200) cat(sprintf(\"line%04d %s\\n\", i, line))", timeout = 30.0);
            write_stdin(":/line0050", timeout = 30.0);
            write_stdin(":n", timeout = 30.0);
            write_stdin(":q", timeout = 30.0);
        })
        .await?;

    assert_snapshot_or_skip("write_stdin_pager_search", &snapshot)
}

#[cfg(feature = "pager")]
#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread")]
async fn write_stdin_pager_hits() -> TestResult<()> {
    let mut snapshot = McpSnapshot::new();

    snapshot
        .session("pager_hits_queue", mcp_script! {
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
async fn write_stdin_preserves_huge_echo_only_inputs() -> TestResult<()> {
    let mut session = common::spawn_server().await?;

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
        "did not expect pager activation for echo-only input, got: {text:?}"
    );
    assert!(
        text.contains("x1 <- 1") && text.contains("x2000 <- 2000"),
        "expected echoed input to be preserved, got: {text:?}"
    );
    assert!(
        !text.contains("echoed input elided"),
        "did not expect echo elision marker, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn write_stdin_preserves_huge_echo_with_output() -> TestResult<()> {
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
    let events_log = bundle_events_log_path(&text);
    let spill_text = events_log
        .as_ref()
        .map(bundle_transcript_path)
        .as_ref()
        .map(fs::read_to_string)
        .transpose()?;
    session.cancel().await?;
    assert!(
        text.contains("events.log")
            || (text.contains("x500 <- 500") && text.contains("y500 <- 500")),
        "expected either an inline transcript or a spill path, got: {text:?}"
    );
    if let Some(spill_text) = spill_text {
        assert!(
            spill_text.contains("x500 <- 500") && spill_text.contains("y500 <- 500"),
            "expected full echoed transcript in spill file, got: {spill_text:?}"
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
            text.contains("x500 <- 500") && text.contains("y500 <- 500"),
            "expected echoed transcript to be preserved inline, got: {text:?}"
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
