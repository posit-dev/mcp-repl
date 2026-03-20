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
async fn file_show_uses_mcp_repl_pager() -> TestResult<()> {
    let mut session = common::spawn_server_with_pager_page_chars(4_000).await?;

    let result = session
        .write_stdin_raw_with(
            "line <- paste(rep(\"x\", 200), collapse = \"\"); tf <- tempfile(\"mcp-repl-file-show-\"); writeLines(sprintf(\"file_show_line%04d %s\", 1:200, line), tf); file.show(tf, delete.file = TRUE); invisible(NULL)",
            Some(30.0),
        )
        .await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("r_file_show backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("file_show_line0001"),
        "expected file.show() content in output, got: {text:?}"
    );
    assert!(
        text.contains("--More--"),
        "expected mcp-repl pager footer, got: {text:?}"
    );

    let result = session.write_stdin_raw_with(":next", Some(30.0)).await?;
    session.cancel().await?;

    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("r_file_show backend unavailable in this environment; skipping");
        return Ok(());
    }
    assert!(
        text.contains("file_show_line") && !text.contains("file_show_line0001"),
        "expected a later page of file.show() output, got: {text:?}"
    );
    assert!(
        text.contains("--More--") || text.contains("(END"),
        "expected pager footer on subsequent page, got: {text:?}"
    );
    Ok(())
}
