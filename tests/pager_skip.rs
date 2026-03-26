mod common;

use common::TestResult;
use rmcp::model::RawContent;

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

fn first_line_number(text: &str) -> Option<u32> {
    let bytes = text.as_bytes();
    if bytes.len() < 5 {
        return None;
    }
    for idx in 0..=bytes.len().saturating_sub(5) {
        if bytes[idx] != b'L' {
            continue;
        }
        let digits = &bytes[idx + 1..idx + 5];
        if digits.iter().all(|ch| ch.is_ascii_digit())
            && let Some(value) = std::str::from_utf8(digits)
                .ok()
                .and_then(|s| s.parse::<u32>().ok())
        {
            return Some(value);
        }
    }
    None
}

#[tokio::test(flavor = "multi_thread")]
async fn skip_advances_without_printing_intermediate_pages() -> TestResult<()> {
    let mut session = common::spawn_server_with_pager_page_chars(60).await?;

    let result = session
        .write_stdin_raw_with("for (i in 1:60) cat(sprintf(\"L%04d\\n\", i))", Some(120.0))
        .await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("pager_skip backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("L0001") && text.contains("--More--"),
        "expected first page, got: {text:?}"
    );
    assert!(
        !text.contains("L0011"),
        "did not expect second page in first reply, got: {text:?}"
    );

    let result = session.write_stdin_raw_with(":skip 1", Some(60.0)).await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("pager_skip backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;
    let first_line = first_line_number(&text).unwrap_or(0);
    assert!(
        (14..=25).contains(&first_line),
        "expected skip to advance at least one page (around L0014..L0025), got: {text:?}"
    );
    assert!(
        !text.contains("L0001") && !text.contains("L0010"),
        "did not expect skipped page content in output, got: {text:?}"
    );
    Ok(())
}
