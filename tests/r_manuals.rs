#![allow(clippy::await_holding_lock)]

use common::TestResult;
use rmcp::model::RawContent;
use std::sync::{Mutex, OnceLock};
use tokio::time::{Duration, Instant, sleep};

mod common;

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

async fn wait_until_not_busy(
    session: &mut common::McpTestSession,
    initial: rmcp::model::CallToolResult,
) -> TestResult<rmcp::model::CallToolResult> {
    let mut result = initial;
    let mut text = result_text(&result);
    if !text.contains("<<console status: busy") {
        return Ok(result);
    }

    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        sleep(Duration::from_millis(250)).await;
        let next = session
            .write_stdin_raw_unterminated_with("", Some(2.0))
            .await?;
        text = result_text(&next);
        result = next;
        if !text.contains("<<console status: busy") {
            return Ok(result);
        }
    }

    Err(format!("worker remained busy after polling: {text:?}").into())
}

#[tokio::test(flavor = "multi_thread")]
async fn r_show_doc_prints_manual_html_in_console() -> TestResult<()> {
    let _guard = test_mutex()
        .lock()
        .map_err(|_| "r_manuals test mutex poisoned")?;
    let mut session = common::spawn_server().await?;

    let result = session
        .write_stdin_raw_with(
            "RShowDoc(\"R-exts\", type = \"html\"); invisible(NULL)",
            Some(60.0),
        )
        .await?;
    let result = wait_until_not_busy(&mut session, result).await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("r_manuals backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;
    assert!(
        text.contains("[repl] browseURL file:") && text.contains("R-exts.html"),
        "expected RShowDoc() to print HTML file contents, got: {text:?}"
    );
    assert!(
        text.contains("Writing R Extensions"),
        "expected manual title in output, got: {text:?}"
    );
    assert!(
        !text.contains("starting httpd help server"),
        "did not expect dynamic help server output, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn r_show_doc_accepts_text_type_alias() -> TestResult<()> {
    let _guard = test_mutex()
        .lock()
        .map_err(|_| "r_manuals test mutex poisoned")?;
    let mut session = common::spawn_server().await?;

    let result = session
        .write_stdin_raw_with(
            "RShowDoc(\"R-exts\", type = \"text\"); invisible(NULL)",
            Some(60.0),
        )
        .await?;
    let result = wait_until_not_busy(&mut session, result).await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("r_manuals backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;
    assert!(
        text.contains("[repl] browseURL file:") && text.contains("R-exts.html"),
        "expected RShowDoc() to fall back to HTML output, got: {text:?}"
    );
    assert!(
        text.contains("Writing R Extensions"),
        "expected manual content in output, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn browseurl_supports_html_fragments_for_r_manuals() -> TestResult<()> {
    let _guard = test_mutex()
        .lock()
        .map_err(|_| "r_manuals test mutex poisoned")?;
    let mut session = common::spawn_server().await?;

    let result = session
        .write_stdin_raw_with(
            "p <- file.path(R.home(\"doc\"), \"manual\", \"R-exts.html\"); browseURL(paste0(p, \"#\", \"Error-signaling-from-Fortran\")); invisible(NULL)",
            Some(60.0),
        )
        .await?;
    let result = wait_until_not_busy(&mut session, result).await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("r_manuals backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;
    assert!(
        text.contains("Error signaling from Fortran"),
        "expected fragment section heading in output, got: {text:?}"
    );
    assert!(
        text.contains("subroutine rexit") && text.contains("subroutine rwarn"),
        "expected Fortran entry points in output, got: {text:?}"
    );
    assert!(
        !text.contains("Table of Contents"),
        "did not expect full manual output for fragment browse, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn r_show_doc_does_not_open_pdfs() -> TestResult<()> {
    let _guard = test_mutex()
        .lock()
        .map_err(|_| "r_manuals test mutex poisoned")?;
    let mut session = common::spawn_server().await?;

    let result = session
        .write_stdin_raw_with("RShowDoc(\"R-exts\"); invisible(NULL)", Some(60.0))
        .await?;
    let result = wait_until_not_busy(&mut session, result).await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("r_manuals backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;
    assert!(
        text.contains("[repl] browseURL file:") && text.contains("R-exts.html"),
        "expected RShowDoc() to render HTML in console, got: {text:?}"
    );
    assert!(
        text.contains("Writing R Extensions"),
        "expected manual content in output, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn r_show_doc_search_returns_compact_card() -> TestResult<()> {
    let _guard = test_mutex()
        .lock()
        .map_err(|_| "r_manuals test mutex poisoned")?;
    let mut session = common::spawn_server_with_pager_page_chars(3_500).await?;

    let setup = session
        .write_stdin_raw_with("RShowDoc(\"R-exts\"); invisible(NULL)", Some(60.0))
        .await?;
    let setup = wait_until_not_busy(&mut session, setup).await?;
    let setup_text = result_text(&setup);
    if backend_unavailable(&setup_text) {
        eprintln!("r_manuals backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let result = session
        .write_stdin_raw_with(":/shebang", Some(60.0))
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains("[pager] search for `shebang` @"),
        "expected compact search header, got: {text:?}"
    );
    assert!(
        !text.contains("[¶]("),
        "expected cleaned breadcrumb text, got: {text:?}"
    );
    assert!(
        !text.contains("LD_LIBRARY_PATH") || text.len() < 2_000,
        "expected compact card instead of full manual page, got: {text:?}"
    );
    Ok(())
}
