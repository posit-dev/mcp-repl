mod common;

use common::TestResult;
use rmcp::model::RawContent;
use tokio::time::{Duration, Instant, sleep};

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

fn text_chunks(result: &rmcp::model::CallToolResult) -> Vec<&str> {
    result
        .content
        .iter()
        .filter_map(|item| match &item.raw {
            RawContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect()
}

fn backend_unavailable(text: &str) -> bool {
    text.contains("Fatal error: cannot create 'R_TempDir'")
        || text.contains("failed to start R session")
        || text.contains("worker exited with status")
        || text.contains("worker exited with signal")
        || text.contains("unable to initialize the JIT")
        || text.contains("options(\"defaultPackages\") was not found")
        || text.contains(
            "worker protocol error: ipc disconnected while waiting for request completion",
        )
}

fn busy_response(text: &str) -> bool {
    text.contains("<<repl status: busy")
        || text.contains("worker is busy")
        || text.contains("request already running")
        || text.contains("input discarded while worker busy")
}

#[tokio::test(flavor = "multi_thread")]
async fn respects_configured_small_page_size() -> TestResult<()> {
    let page_bytes = 80;
    let session = common::spawn_server_with_pager_page_chars(page_bytes).await?;

    let mut result = session
        .write_stdin_raw_with("for (i in 1:50) cat('abcd\\n')", Some(30.0))
        .await?;
    let mut full_text = result_text(&result);
    if backend_unavailable(&full_text) {
        eprintln!("pager_page_size backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    if busy_response(&full_text) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            sleep(Duration::from_millis(100)).await;
            let next = session
                .write_stdin_raw_unterminated_with("", Some(2.0))
                .await?;
            let text = result_text(&next);
            if backend_unavailable(&text) {
                eprintln!("pager_page_size backend unavailable in this environment; skipping");
                session.cancel().await?;
                return Ok(());
            }
            result = next;
            full_text = text;
            if !busy_response(&full_text) {
                break;
            }
        }
    }
    assert!(
        !busy_response(&full_text),
        "pager_page_size worker remained busy after waiting for completion: {full_text:?}"
    );
    session.cancel().await?;

    let chunks = text_chunks(&result);
    let first = chunks
        .iter()
        .find(|text| !text.contains("--More--") && !text.starts_with("(END"))
        .copied()
        .unwrap_or("");
    let expected_echo = "> for (i in 1:50) cat('abcd\\n')\n";
    let first_without_optional_echo = first.strip_prefix(expected_echo).unwrap_or(first);
    assert!(
        !first_without_optional_echo.is_empty(),
        "expected first page content, got: {first:?}"
    );
    assert!(
        first.len() <= page_bytes as usize,
        "first page exceeded page size: {} > {page_bytes}",
        first.len()
    );

    assert!(
        chunks.iter().any(|text| text.contains("--More--")),
        "expected pager footer in response"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn respects_configured_large_page_size() -> TestResult<()> {
    let page_bytes = 10_000;
    let session = common::spawn_server_with_pager_page_chars(page_bytes).await?;

    let mut result = session
        .write_stdin_raw_with("for (i in 1:10) cat('abcd\\n\')", Some(30.0))
        .await?;
    let mut full_text = result_text(&result);
    if backend_unavailable(&full_text) {
        eprintln!("pager_page_size backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    if busy_response(&full_text) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            sleep(Duration::from_millis(100)).await;
            let next = session
                .write_stdin_raw_unterminated_with("", Some(2.0))
                .await?;
            let text = result_text(&next);
            if backend_unavailable(&text) {
                eprintln!("pager_page_size backend unavailable in this environment; skipping");
                session.cancel().await?;
                return Ok(());
            }
            result = next;
            full_text = text;
            if !busy_response(&full_text) {
                break;
            }
        }
    }
    assert!(
        !busy_response(&full_text),
        "pager_page_size worker remained busy after waiting for completion: {full_text:?}"
    );
    session.cancel().await?;

    let chunks = text_chunks(&result);
    assert!(
        !chunks.iter().any(|text| text.contains("--More--")),
        "did not expect pager footer"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn large_page_size_keeps_pager_mode_instead_of_spilling_to_output_bundle() -> TestResult<()> {
    let page_bytes = 10_000;
    let session = common::spawn_server_with_pager_page_chars(page_bytes).await?;

    let initial = session
        .write_stdin_raw_with(
            "line <- paste(rep('x', 200), collapse = ''); for (i in 1:300) cat(sprintf('line%04d %s\\n', i, line))",
            Some(30.0),
        )
        .await?;
    let mut initial_text = result_text(&initial);
    if backend_unavailable(&initial_text) {
        eprintln!("pager_page_size backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    if busy_response(&initial_text) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            sleep(Duration::from_millis(100)).await;
            let next = session
                .write_stdin_raw_unterminated_with("", Some(2.0))
                .await?;
            let text = result_text(&next);
            if backend_unavailable(&text) {
                eprintln!("pager_page_size backend unavailable in this environment; skipping");
                session.cancel().await?;
                return Ok(());
            }
            initial_text = text;
            if !busy_response(&initial_text) {
                break;
            }
        }
    }
    assert!(
        !busy_response(&initial_text),
        "pager_page_size worker remained busy after waiting for completion: {initial_text:?}"
    );
    assert!(
        initial_text.contains("--More--"),
        "expected pager footer on the first page, got: {initial_text:?}"
    );
    assert!(
        !initial_text.contains("transcript.txt"),
        "did not expect output-bundle disclosure to replace pager output, got: {initial_text:?}"
    );

    let next = session.write_stdin_raw_with(":next", Some(30.0)).await?;
    let next_text = result_text(&next);
    session.cancel().await?;

    assert!(
        !next_text.contains("unexpected ':'"),
        "expected pager state to remain active for :next, got: {next_text:?}"
    );
    assert!(
        next_text.contains("--More--") || next_text.contains("(END"),
        "expected pager output for :next, got: {next_text:?}"
    );

    Ok(())
}
