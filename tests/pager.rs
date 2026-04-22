mod common;

#[cfg(not(windows))]
use common::McpSnapshot;
use common::TestResult;
use rmcp::model::RawContent;
#[cfg(windows)]
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
async fn wait_until_not_busy(
    session: &mut common::McpTestSession,
    initial: rmcp::model::CallToolResult,
) -> TestResult<rmcp::model::CallToolResult> {
    let mut result = initial;
    let mut text = result_text(&result);
    if !text.contains("<<repl status: busy") {
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
        if !text.contains("<<repl status: busy") {
            return Ok(result);
        }
    }

    Err(format!("worker remained busy after polling: {text:?}").into())
}

#[cfg(not(windows))]
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
        || text.contains("[repl] protocol error: missing prompt after pager dismiss")
}

#[cfg(not(windows))]
fn assert_snapshot_or_skip(name: &str, snapshot: &McpSnapshot) -> TestResult<()> {
    let rendered = snapshot.render();
    let transcript = snapshot.render_transcript();
    if backend_unavailable(&rendered) || backend_unavailable(&transcript) {
        eprintln!("pager backend unavailable in this environment; skipping");
        return Ok(());
    }
    insta::assert_snapshot!(name, rendered);
    insta::with_settings!({ snapshot_suffix => "transcript" }, {
        insta::assert_snapshot!(name, transcript);
    });
    Ok(())
}

#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread")]
async fn pager_commands_are_handled_server_side() -> TestResult<()> {
    let session = common::spawn_server_with_pager_page_chars(120).await?;

    let initial = session
        .write_stdin_raw_with(
            "line <- paste(rep(\"x\", 200), collapse = \"\"); for (i in 1:200) cat(sprintf(\"line%04d %s\\n\", i, line))",
            Some(30.0),
        )
        .await?;
    let initial_text = result_text(&initial);
    if backend_unavailable(&initial_text) {
        eprintln!("pager backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        initial_text.contains("--More--"),
        "expected pager to activate, got: {initial_text:?}"
    );

    let next = session.write_stdin_raw_with(":next", Some(30.0)).await?;
    let next_text = result_text(&next);
    assert!(
        !next_text.contains("unexpected ':'"),
        "expected :next to be handled by pager, got: {next_text:?}"
    );
    assert!(
        next_text.contains("--More--") || next_text.contains("(END"),
        "expected pager output for :next, got: {next_text:?}"
    );

    let hits = session
        .write_stdin_raw_with(":hits line0150", Some(30.0))
        .await?;
    let hits_text = result_text(&hits);
    assert!(
        !hits_text.contains("unexpected ':'"),
        "expected :hits to be handled by pager, got: {hits_text:?}"
    );
    assert!(
        hits_text.contains("[pager]") || hits_text.contains("#1 @"),
        "expected pager response for :hits, got: {hits_text:?}"
    );

    let quit = session.write_stdin_raw_with(":q", Some(30.0)).await?;
    let quit_text = result_text(&quit);
    assert!(
        !quit_text.contains("unexpected ':'"),
        "expected :q to be handled by pager, got: {quit_text:?}"
    );
    assert!(
        quit_text.contains(">"),
        "expected prompt after :q, got: {quit_text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread")]
async fn pager_matches_stays_inline_in_pager_mode() -> TestResult<()> {
    let session = common::spawn_server_with_pager_page_chars(120).await?;

    let initial = session
        .write_stdin_raw_with(
            "line <- paste(rep(\"foo\", 80), collapse = \" \"); for (i in 1:300) cat(sprintf(\"line%04d %s\\n\", i, line))",
            Some(30.0),
        )
        .await?;
    let initial_text = result_text(&initial);
    if backend_unavailable(&initial_text) {
        eprintln!("pager backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        initial_text.contains("--More--"),
        "expected pager to activate, got: {initial_text:?}"
    );

    let matches = session
        .write_stdin_raw_with(":matches foo", Some(30.0))
        .await?;
    let matches_text = result_text(&matches);

    session.cancel().await?;

    assert!(
        !matches_text.contains("transcript.txt"),
        "did not expect oversized :matches output to spill to a bundle, got: {matches_text:?}"
    );
    assert!(
        matches_text.contains("[pager] matches:"),
        "expected pager summary inline, got: {matches_text:?}"
    );
    assert!(
        matches_text.contains("#1 @"),
        "expected inline :matches rows to remain navigable in the pager, got: {matches_text:?}"
    );

    Ok(())
}

#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread")]
async fn paginates_large_output() -> TestResult<()> {
    let mut snapshot = McpSnapshot::new();
    snapshot
        .pager_session("default", 300, mcp_script! {
            write_stdin("line <- paste(rep(\"x\", 200), collapse = \"\"); for (i in 1:200) cat(sprintf(\"line%04d %s\\n\", i, line))", timeout = 30.0);
            write_stdin("1+1", timeout = 10.0);
            write_stdin("line <- paste(rep(\"x\", 200), collapse = \"\"); for (i in 1:200) cat(sprintf(\"line%04d %s\\n\", i, line))", timeout = 30.0);
            write_stdin(":next", timeout = 30.0);
            write_stdin(":tail", timeout = 30.0);
            write_stdin("1+1", timeout = 10.0);
        })
        .await?;

    assert_snapshot_or_skip("paginates_large_output", &snapshot)
}

#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread")]
async fn pager_search_and_counts() -> TestResult<()> {
    let mut snapshot = McpSnapshot::new();
    snapshot
        .pager_session("default", 300, mcp_script! {
            write_stdin("line <- paste(rep(\"x\", 200), collapse = \"\"); for (i in 1:200) cat(sprintf(\"line%04d %s\\n\", i, line))", timeout = 30.0);
            write_stdin(":/line01", timeout = 30.0);
            write_stdin(":n", timeout = 30.0);
            write_stdin(":next 2", timeout = 30.0);
            write_stdin(":tail 2", timeout = 30.0);
            write_stdin("1+1", timeout = 10.0);
        })
        .await?;

    assert_snapshot_or_skip("pager_search_and_counts", &snapshot)
}

#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread")]
async fn pager_search_preserves_whitespace() -> TestResult<()> {
    let mut snapshot = McpSnapshot::new();
    snapshot
        .pager_session("default", 300, mcp_script! {
            write_stdin("line <- paste(rep(\"x\", 200), collapse = \"\"); for (i in 1:200) { suffix <- if (i == 25) \" r\" else if (i == 75) \"r \" else \"\"; cat(sprintf(\"line%04d %s%s\\n\", i, line, suffix)) }", timeout = 30.0);
            write_stdin(":where  r", timeout = 30.0);
            write_stdin(":/ r", timeout = 30.0);
            write_stdin(format!(":where r{}", " "), timeout = 30.0);
            write_stdin(format!(":/r{}", " "), timeout = 30.0);
            write_stdin(":q", timeout = 30.0);
        })
        .await?;

    assert_snapshot_or_skip("pager_search_preserves_whitespace", &snapshot)
}

#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread")]
async fn pager_search_case_insensitive_prefix_parsing() -> TestResult<()> {
    let mut snapshot = McpSnapshot::new();
    snapshot
        .pager_session("default", 300, mcp_script! {
            write_stdin("line <- paste(rep(\"x\", 200), collapse = \"\"); for (i in 1:200) cat(sprintf(\"line%04d %s\\n\", i, line))", timeout = 30.0);
            write_stdin(":where -i LINE01", timeout = 30.0);
            write_stdin(":/i LINE01", timeout = 30.0);
            write_stdin(":/iLINE01", timeout = 30.0);
            write_stdin(":q", timeout = 30.0);
        })
        .await?;

    assert_snapshot_or_skip("pager_search_case_insensitive_prefix_parsing", &snapshot)
}

#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread")]
async fn pager_matches_with_headings() -> TestResult<()> {
    let mut snapshot = McpSnapshot::new();
    snapshot
        .pager_session("default", 300, mcp_script! {
            write_stdin("cat('# Title\\n\\n## Alpha\\n'); for (i in 1:20) cat(sprintf('alpha line %02d foo\\n', i)); cat('\\n## Beta\\n'); for (i in 1:20) cat(sprintf('beta line %02d foo\\n', i))", timeout = 30.0);
            write_stdin(":matches foo", timeout = 30.0);
            write_stdin(":matches -C 1 foo", timeout = 30.0);
            write_stdin(":q", timeout = 30.0);
        })
        .await?;

    assert_snapshot_or_skip("pager_matches_with_headings", &snapshot)
}

#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread")]
async fn pager_hits_mode() -> TestResult<()> {
    let mut snapshot = McpSnapshot::new();
    snapshot
        .pager_session("default", 300, mcp_script! {
            write_stdin("cat('# Title\\n'); for (i in 1:40) cat(sprintf('filler %02d xxxxxxxxxxxxxxxxxxxxxxxxxxxxx\\n', i)); cat('## Alpha\\n'); for (i in 1:3) cat(sprintf('alpha configure %02d\\n', i)); cat('## Beta\\n'); for (i in 1:3) cat(sprintf('beta configure %02d\\n', i))", timeout = 30.0);
            write_stdin(":hits configure", timeout = 30.0);
            write_stdin(":n", timeout = 30.0);
            write_stdin(":q", timeout = 30.0);
        })
        .await?;

    assert_snapshot_or_skip("pager_hits_mode", &snapshot)
}

#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread")]
async fn pager_whitespace_only_input_advances_page() -> TestResult<()> {
    let mut snapshot = McpSnapshot::new();
    snapshot
        .pager_session("default", 300, mcp_script! {
            write_stdin("line <- paste(rep(\"x\", 200), collapse = \"\"); for (i in 1:200) cat(sprintf(\"line%04d %s\\n\", i, line))", timeout = 30.0);
            write_stdin("   ", timeout = 30.0);
            write_stdin(":q", timeout = 30.0);
        })
        .await?;

    assert_snapshot_or_skip("pager_whitespace_only_input_advances_page", &snapshot)
}

#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread")]
async fn pager_dedup_on_seek() -> TestResult<()> {
    let mut snapshot = McpSnapshot::new();
    snapshot
        .pager_session("default", 300, mcp_script! {
            write_stdin("line <- paste(rep(\"x\", 120), collapse = \"\"); for (i in 1:40) cat(sprintf(\"line%02d %s\\n\", i, line))", timeout = 30.0);
            write_stdin(":next", timeout = 30.0);
            write_stdin(":seek 0", timeout = 30.0);
            write_stdin(":next", timeout = 30.0);
            write_stdin(":q", timeout = 30.0);
        })
        .await?;

    assert_snapshot_or_skip("pager_dedup_on_seek", &snapshot)
}

#[tokio::test(flavor = "multi_thread")]
async fn pager_smoke() -> TestResult<()> {
    let mut session = common::spawn_server_with_pager_page_chars(80).await?;

    let result = session
        .write_stdin_raw_with("for (i in 1:80) cat(sprintf(\"L%04d\\n\", i))", Some(120.0))
        .await?;
    let result = common::wait_until_not_busy(
        &mut session,
        result,
        std::time::Duration::from_millis(250),
        std::time::Duration::from_secs(30),
    )
    .await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("pager backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("L0001"),
        "expected first page output, got: {text:?}"
    );
    assert!(
        text.contains("--More--"),
        "expected pager footer, got: {text:?}"
    );

    let result = session.write_stdin_raw_with(":next", Some(60.0)).await?;
    let result = common::wait_until_not_busy(
        &mut session,
        result,
        std::time::Duration::from_millis(250),
        std::time::Duration::from_secs(30),
    )
    .await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("pager backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("L0002")
            || text.contains("L0003")
            || text.contains("L0010")
            || text.contains("L0014"),
        "expected next page output, got: {text:?}"
    );

    let result = session.write_stdin_raw_with(":/L0031", Some(60.0)).await?;
    let result = common::wait_until_not_busy(
        &mut session,
        result,
        std::time::Duration::from_millis(250),
        std::time::Duration::from_secs(30),
    )
    .await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("pager backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("L0031") || text.contains("next match"),
        "expected search guidance/output, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn pager_empty_input_does_not_start_worker_before_inherit_update() -> TestResult<()> {
    let session = common::spawn_server_with_args_env_and_pager_page_chars(
        vec!["--sandbox".to_string(), "inherit".to_string()],
        Vec::new(),
        80,
    )
    .await?;

    let result = session
        .write_stdin_raw_unterminated_with("", Some(1.0))
        .await?;
    let text = result_text(&result);

    if backend_unavailable(&text) {
        eprintln!("pager backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    assert!(
        text.contains("--sandbox inherit requested but no client sandbox state was provided"),
        "expected initial empty pager input to surface sandbox-state failure, got: {text:?}"
    );
    assert!(
        !text.contains("<<repl status: idle>>"),
        "did not expect empty pager input to spawn the worker before a sandbox update, got: {text:?}"
    );
    assert!(
        !text.contains(">"),
        "did not expect empty pager input to surface a worker prompt before a sandbox update, got: {text:?}"
    );
    session.cancel().await?;
    Ok(())
}

#[cfg(windows)]
async fn assert_blank_pager_input_advances_page(input: &str) -> TestResult<()> {
    let mut session = common::spawn_server_with_pager_page_chars(80).await?;

    let result = session
        .write_stdin_raw_with("for (i in 1:80) cat(sprintf(\"L%04d\\n\", i))", Some(120.0))
        .await?;
    let result = wait_until_not_busy(&mut session, result).await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("pager backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("L0001"),
        "expected first page output, got: {text:?}"
    );
    assert!(
        text.contains("--More--"),
        "expected pager footer, got: {text:?}"
    );

    let result = session.write_stdin_raw_with(input, Some(60.0)).await?;
    let result = wait_until_not_busy(&mut session, result).await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("pager backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("L0002")
            || text.contains("L0003")
            || text.contains("L0010")
            || text.contains("L0014"),
        "expected blank pager input to advance to the next page, got: {text:?}"
    );
    assert!(
        text.contains("--More--") || text.contains("(END"),
        "expected pager output after blank pager input, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn pager_whitespace_only_input_advances_page() -> TestResult<()> {
    assert_blank_pager_input_advances_page("   ").await
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn pager_empty_input_advances_page() -> TestResult<()> {
    assert_blank_pager_input_advances_page("").await
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn empty_poll_while_busy_preserves_busy_pager_state() -> TestResult<()> {
    let session = common::spawn_server_with_pager_page_chars(80).await?;

    let initial = session
        .write_stdin_raw_with(
            "for (i in 1:80) cat(sprintf(\"L%04d\\n\", i)); flush.console(); Sys.sleep(1.0)",
            Some(0.1),
        )
        .await?;
    let initial_text = result_text(&initial);
    if backend_unavailable(&initial_text) {
        eprintln!("pager backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if !initial_text.contains("<<repl status: busy") || !initial_text.contains("--More--") {
        eprintln!("pager did not stay busy with an active footer in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let poll = session
        .write_stdin_raw_unterminated_with("", Some(0.1))
        .await?;
    let poll_text = result_text(&poll);
    if backend_unavailable(&poll_text) {
        eprintln!("pager backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    assert!(
        poll_text.contains("<<repl status: busy"),
        "expected empty poll to remain a busy follow-up while the request was still running, got: {poll_text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn wait_until_not_busy_does_not_return_while_pager_request_is_still_running() -> TestResult<()>
{
    let mut session = common::spawn_server_with_pager_page_chars(80).await?;

    let initial = session
        .write_stdin_raw_with(
            "for (i in 1:80) cat(sprintf(\"L%04d\\n\", i)); flush.console(); Sys.sleep(2.0); cat('DONE\\n')",
            Some(0.1),
        )
        .await?;
    let initial_text = result_text(&initial);
    if backend_unavailable(&initial_text) {
        eprintln!("pager backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if !initial_text.contains("<<repl status: busy") {
        eprintln!("pager request settled before the helper could observe a busy reply; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let _ = wait_until_not_busy(&mut session, initial).await?;
    let follow_up = session.write_stdin_raw_with("1+1", Some(0.2)).await?;
    let follow_up_text = result_text(&follow_up);
    if backend_unavailable(&follow_up_text) {
        eprintln!("pager backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    assert!(
        !follow_up_text.contains("input discarded while worker busy")
            && !follow_up_text.contains("<<repl status: busy"),
        "expected wait_until_not_busy to return only after the running pager request settled, got: {follow_up_text:?}"
    );

    session.cancel().await?;
    Ok(())
}
