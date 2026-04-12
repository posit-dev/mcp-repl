mod common;

#[cfg(not(windows))]
use common::McpSnapshot;
use common::TestResult;

#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread")]
async fn sends_input_to_r_console_snapshot() -> TestResult<()> {
    let mut snapshot = McpSnapshot::new();
    snapshot
        .session(
            "default",
            mcp_script! {
                write_stdin("1+1", timeout = 10.0);
            },
        )
        .await?;

    let rendered = snapshot.render();
    let transcript = snapshot.render_transcript();
    if common::backend_unavailable(&rendered) || common::backend_unavailable(&transcript) {
        eprintln!("server_smoke backend unavailable in this environment; skipping");
        return Ok(());
    }

    insta::assert_snapshot!("sends_input_to_r_console", rendered);
    insta::with_settings!({ snapshot_suffix => "transcript" }, {
        insta::assert_snapshot!("sends_input_to_r_console", transcript);
    });
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sends_input_to_r_console_smoke() -> TestResult<()> {
    let mut session = common::spawn_server().await?;
    let first = session.write_stdin_raw_with("1+1", Some(30.0)).await?;
    let result = common::wait_until_not_busy(
        &mut session,
        first,
        std::time::Duration::from_millis(100),
        std::time::Duration::from_secs(30),
    )
    .await?;
    let text = common::result_text(&result);
    if common::backend_unavailable(&text) {
        eprintln!("server_smoke backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;
    assert!(text.contains("2"), "expected 2 in output, got: {text:?}");
    Ok(())
}
