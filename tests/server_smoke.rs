mod common;

#[cfg(not(windows))]
use common::McpSnapshot;
use common::TestResult;
#[cfg(windows)]
use rmcp::model::RawContent;
#[cfg(windows)]
use tokio::time::{Duration, Instant, sleep};

#[cfg(windows)]
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

#[cfg(windows)]
fn is_busy_response(text: &str) -> bool {
    text.contains("<<repl status: busy")
        || text.contains("worker is busy")
        || text.contains("request already running")
        || text.contains("input discarded while worker busy")
}

#[cfg(not(windows))]
fn backend_unavailable(text: &str) -> bool {
    text.contains("Fatal error: cannot create 'R_TempDir'")
        || text.contains("failed to start R session")
        || text.contains("worker exited with status")
        || text.contains("worker exited with signal")
        || text.contains("worker io error: Broken pipe")
        || text.contains("unable to initialize the JIT")
        || text.contains("libR.so: cannot open shared object file")
        || text.contains("options(\"defaultPackages\") was not found")
        || text.contains(
            "worker protocol error: ipc disconnected while waiting for request completion",
        )
}

#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread")]
async fn sends_input_to_r_console() -> TestResult<()> {
    let mut snapshot = McpSnapshot::new();
    snapshot
        .session(
            "default",
            mcp_script! {
                write_stdin("1+1", timeout = 10.0);
            },
        )
        .await?;

    let rendered = snapshot.render();
    let transcript = snapshot.render_transcript();
    if backend_unavailable(&rendered) || backend_unavailable(&transcript) {
        eprintln!("server_smoke backend unavailable in this environment; skipping");
        return Ok(());
    }

    insta::assert_snapshot!("sends_input_to_r_console", rendered);
    insta::with_settings!({ snapshot_suffix => "transcript" }, {
        insta::assert_snapshot!("sends_input_to_r_console", transcript);
    });
    Ok(())
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn sends_input_to_r_console() -> TestResult<()> {
    let mut session = common::spawn_server().await?;
    let first = session.write_stdin_raw_with("1+1", Some(30.0)).await?;
    let mut text = result_text(&first);
    if text.contains("Fatal error: cannot create 'R_TempDir'")
        || text.contains(
            "worker protocol error: ipc disconnected while waiting for request completion",
        )
    {
        eprintln!("server_smoke backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if is_busy_response(&text) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if Instant::now() >= deadline {
                session.cancel().await?;
                panic!("expected 2 in output, got: {text:?}");
            }
            sleep(Duration::from_millis(100)).await;
            let result = session.write_stdin_raw_with("1+1", Some(5.0)).await?;
            text = result_text(&result);
            if is_busy_response(&text) {
                continue;
            }
            break;
        }
    }
    session.cancel().await?;
    assert!(text.contains("2"), "expected 2 in output, got: {text:?}");
    Ok(())
}
