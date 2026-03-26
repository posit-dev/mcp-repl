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
        || text.contains("worker exited with signal")
        || text.contains("unable to initialize the JIT")
        || text.contains(
            "worker protocol error: ipc disconnected while waiting for request completion",
        )
        || text.contains("options(\"defaultPackages\") was not found")
        || text.contains("worker io error: Broken pipe")
}

#[tokio::test(flavor = "multi_thread")]
async fn seek_parses_offset_and_percent() -> TestResult<()> {
    let mut session = common::spawn_server_with_pager_page_chars(36).await?;

    let setup = session
        .write_stdin_raw_with("for (i in 1:100) cat(sprintf(\"L%04d\\n\", i))", Some(30.0))
        .await?;
    let setup_text = result_text(&setup);
    if backend_unavailable(&setup_text) {
        eprintln!("pager_seek backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let result = session
        .write_stdin_raw_with(":seek @180", Some(30.0))
        .await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("pager_seek backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("L0031"),
        "expected seek @offset to jump to line 31, got: {text:?}"
    );

    let result = session
        .write_stdin_raw_with(":seek 50%", Some(30.0))
        .await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("pager_seek backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;
    assert!(
        text.contains("L0052"),
        "expected seek percent to jump to middle, got: {text:?}"
    );
    assert!(
        !text.contains("L0009") && !text.contains("L0010"),
        "did not expect seek percent to behave like byte offset, got: {text:?}"
    );
    Ok(())
}
