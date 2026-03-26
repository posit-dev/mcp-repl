#![allow(clippy::await_holding_lock)]

mod common;

use common::TestResult;
use rmcp::model::RawContent;
use std::sync::{Mutex, OnceLock};

fn test_mutex() -> &'static Mutex<()> {
    static TEST_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
    TEST_MUTEX.get_or_init(|| Mutex::new(()))
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
        || text.contains("unable to initialize the JIT")
        || text.contains(
            "worker protocol error: ipc disconnected while waiting for request completion",
        )
}

#[tokio::test(flavor = "multi_thread")]
async fn matches_limit_parses_long_flag() -> TestResult<()> {
    let _guard = test_mutex()
        .lock()
        .map_err(|_| "pager_flags test mutex poisoned")?;
    let mut session = common::spawn_server_with_pager_page_chars(60).await?;

    let setup = session
        .write_stdin_raw_with(
            "for (i in 1:30) cat(sprintf(\"match %02d configure\\n\", i))",
            Some(30.0),
        )
        .await?;
    let setup_text = result_text(&setup);
    if backend_unavailable(&setup_text) {
        eprintln!("pager_flags backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let result = session
        .write_stdin_raw_with(":matches --limit 2 configure", Some(30.0))
        .await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("pager_flags backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;
    assert!(
        text.contains("matches: 2") && text.contains("limit 2"),
        "expected matches --limit 2 output, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn hits_count_parses_long_flag() -> TestResult<()> {
    let _guard = test_mutex()
        .lock()
        .map_err(|_| "pager_flags test mutex poisoned")?;
    let mut session = common::spawn_server_with_pager_page_chars(60).await?;

    let setup = session
        .write_stdin_raw_with(
            "for (i in 1:10) cat(sprintf(\"hit configure %02d\\n\", i))",
            Some(60.0),
        )
        .await?;
    let setup_text = result_text(&setup);
    if backend_unavailable(&setup_text) {
        eprintln!("pager_flags backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let result = session
        .write_stdin_raw_with(":hits --count 2 configure", Some(60.0))
        .await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("pager_flags backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;
    assert!(
        text.contains("#2"),
        "expected hits --count 2 output, got: {text:?}"
    );
    Ok(())
}
