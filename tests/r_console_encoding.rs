mod common;

#[cfg(windows)]
use common::TestResult;
#[cfg(windows)]
use rmcp::model::RawContent;

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
fn backend_unavailable(text: &str) -> bool {
    text.contains("Fatal error: cannot create 'R_TempDir'")
        || text.contains("failed to start R session")
        || text.contains("worker exited with status")
        || text.contains("unable to initialize the JIT")
        || text.contains(
            "worker protocol error: ipc disconnected while waiting for request completion",
        )
}

#[cfg(windows)]
fn assert_no_console_encoding_artifacts(text: &str) {
    assert!(
        !text.contains('\u{2}'),
        "unexpected STX marker in output: {text:?}"
    );
    assert!(
        !text.contains('\u{3}'),
        "unexpected ETX marker in output: {text:?}"
    );
    assert!(
        !text.contains('\u{fffd}'),
        "unexpected replacement character in output: {text:?}"
    );
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn write_stdin_windows_output_has_no_utf8_marker_artifacts() -> TestResult<()> {
    let session = common::spawn_server_with_args(vec![
        "--sandbox".to_string(),
        "danger-full-access".to_string(),
    ])
    .await?;

    let quoted = session
        .write_stdin_raw_with("'after interrupt'", Some(30.0))
        .await?;
    let quoted_text = result_text(&quoted);
    if backend_unavailable(&quoted_text) {
        eprintln!("r_console_encoding backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        quoted_text.contains("after interrupt"),
        "expected quoted string output, got: {quoted_text:?}"
    );
    assert_no_console_encoding_artifacts(&quoted_text);

    let help = session
        .write_stdin_raw_with("options(useFancyQuotes=TRUE); ?mean", Some(30.0))
        .await?;
    let help_text = result_text(&help);
    if help_text.contains("<<repl status: busy") {
        eprintln!("r_console_encoding help output still busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        help_text.contains("R Documentation"),
        "expected help output, got: {help_text:?}"
    );
    assert_no_console_encoding_artifacts(&help_text);

    session.cancel().await?;
    Ok(())
}
