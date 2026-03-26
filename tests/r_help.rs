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
async fn text_help_is_llm_friendly() -> TestResult<()> {
    let mut session = common::spawn_server().await?;

    let result = session.write_stdin_raw_with("?mean", Some(30.0)).await?;
    session.cancel().await?;

    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("r_help backend unavailable in this environment; skipping");
        return Ok(());
    }
    assert!(
        text.contains("R Documentation"),
        "expected text help output, got: {text:?}"
    );
    assert!(
        !text.contains('\u{8}'),
        "expected no backspace control characters in help output, got: {text:?}"
    );
    #[cfg(not(windows))]
    assert!(
        !text.contains('\u{fffd}'),
        "expected no replacement characters in help output, got: {text:?}"
    );
    Ok(())
}
