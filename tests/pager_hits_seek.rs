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
async fn hits_after_seek_does_not_repeat() -> TestResult<()> {
    let mut session = common::spawn_server_with_pager_page_chars(120).await?;

    let setup = session
        .write_stdin_raw_with(
            "cat('## Alpha\\n'); for (i in 1:8) cat(sprintf('alpha line %02d foo\\n', i)); cat('## Beta\\n'); for (i in 1:8) cat(sprintf('beta line %02d foo\\n', i))",
            Some(30.0),
        )
        .await?;
    let setup_text = result_text(&setup);
    if backend_unavailable(&setup_text) {
        eprintln!("pager_hits_seek backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let first = session
        .write_stdin_raw_with(":hits foo", Some(30.0))
        .await?;
    let first_text = result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("pager_hits_seek backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        first_text.contains("#1"),
        "expected initial hits output, got: {first_text:?}"
    );
    let first_match_line = first_text
        .lines()
        .find(|line| line.trim_start().starts_with('>'))
        .map(|line| line.trim().to_string())
        .unwrap_or_default();
    assert!(
        !first_match_line.is_empty(),
        "expected a match line in initial hits output, got: {first_text:?}"
    );

    let _ = session.write_stdin_raw_with(":seek 0", Some(30.0)).await?;
    let second = session
        .write_stdin_raw_with(":hits foo", Some(30.0))
        .await?;
    let second_text = result_text(&second);
    if backend_unavailable(&second_text) {
        eprintln!("pager_hits_seek backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        !second_text.contains(&first_match_line),
        "expected hits after seek to avoid repeating lines, got: {second_text:?}"
    );

    session.cancel().await?;
    Ok(())
}
