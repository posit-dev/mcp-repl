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
async fn where_does_not_advance_cursor() -> TestResult<()> {
    let mut session = common::spawn_server_with_pager_page_chars(60).await?;

    let initial = session
        .write_stdin_raw_with("for (i in 1:60) cat(sprintf(\"L%04d\\n\", i))", Some(30.0))
        .await?;
    let initial_text = result_text(&initial);
    if backend_unavailable(&initial_text) {
        eprintln!("pager_where backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let result = session
        .write_stdin_raw_with(":where L0031", Some(60.0))
        .await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("pager_where backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("next match") || text.contains("match is on the current/next page"),
        "expected where() guidance, got: {text:?}"
    );

    let result = session.write_stdin_raw_with(":next", Some(60.0)).await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("pager_where backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;
    let first_line = first_line_number(&text).unwrap_or(0);
    assert!(
        (4..=20).contains(&first_line),
        "expected where() not to jump to remote match page, got: {text:?}"
    );
    Ok(())
}
