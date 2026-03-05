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
async fn vignette_prints_contents_in_console() -> TestResult<()> {
    let _guard = test_mutex()
        .lock()
        .map_err(|_| "r_vignettes test mutex poisoned")?;
    let mut session = common::spawn_server_with_pager_page_chars(4_000).await?;

    let result = session
        .write_stdin_raw_with(
            "x <- vignette(\"grid\", package = \"grid\"); print(x); invisible(NULL)",
            Some(60.0),
        )
        .await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("r_vignettes backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;
    assert!(
        text.contains("[repl] vignette: grid (package: grid)"),
        "expected vignette info in console, got: {text:?}"
    );
    assert!(
        text.contains("Source:") && text.contains("grid.Rnw"),
        "expected source path in output, got: {text:?}"
    );
    assert!(
        text.contains("% File src/library/grid/vignettes/grid.Rnw"),
        "expected vignette contents in output, got: {text:?}"
    );
    assert!(
        !text.contains("starting httpd help server"),
        "did not expect dynamic help server output, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn browse_vignettes_prints_text_listing() -> TestResult<()> {
    let _guard = test_mutex()
        .lock()
        .map_err(|_| "r_vignettes test mutex poisoned")?;
    let mut session = common::spawn_server_with_pager_page_chars(20_000).await?;

    let result = session
        .write_stdin_raw_with(
            "x <- browseVignettes(package = \"grid\"); print(x); invisible(NULL)",
            Some(60.0),
        )
        .await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("r_vignettes backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;
    assert!(
        text.contains("[repl] browseVignettes:"),
        "expected browseVignettes text output, got: {text:?}"
    );
    assert!(
        text.contains("Package: grid"),
        "expected package heading in output, got: {text:?}"
    );
    assert!(
        !text.contains("starting httpd help server"),
        "did not expect dynamic help server output, got: {text:?}"
    );
    Ok(())
}
