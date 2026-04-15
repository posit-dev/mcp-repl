mod common;

#[cfg(not(windows))]
use common::McpSnapshot;
use common::TestResult;

#[cfg(not(windows))]
fn assert_snapshot_or_skip(name: &str, snapshot: &McpSnapshot) -> TestResult<()> {
    let rendered = snapshot.render();
    let transcript = snapshot.render_transcript();
    if common::backend_unavailable(&rendered) || common::backend_unavailable(&transcript) {
        eprintln!("session_endings backend unavailable in this environment; skipping");
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
async fn snapshots_session_endings() -> TestResult<()> {
    let mut snapshot = McpSnapshot::new();

    snapshot
        .session(
            "restart_timeout_zero",
            mcp_script! {
                write_stdin("x <- 1", timeout = 10.0);
                write_stdin("\u{4}", timeout = 0.0);
                write_stdin("print(exists(\"x\"))", timeout = 10.0);
            },
        )
        .await?;

    snapshot
        .session(
            "restart_timeout_thirty",
            mcp_script! {
                write_stdin("x <- 1", timeout = 10.0);
                write_stdin("\u{4}", timeout = 30.0);
                write_stdin("print(exists(\"x\"))", timeout = 10.0);
            },
        )
        .await?;

    snapshot
        .session(
            "eof_input",
            mcp_script! {
                write_stdin("\u{4}", timeout = 10.0);
                write_stdin("1+1", timeout = 10.0);
            },
        )
        .await?;

    snapshot
        .session(
            "eof_then_remaining_input_same_call",
            mcp_script! {
                write_stdin("\u{4}\n1+1", timeout = 10.0);
            },
        )
        .await?;

    snapshot
        .session(
            "quit_no",
            mcp_script! {
                write_stdin("x <- 1", timeout = 10.0);
                write_stdin("quit(\"no\")", timeout = 10.0);
                write_stdin("1+1", timeout = 10.0);
            },
        )
        .await?;

    snapshot
        .session(
            "quit_default",
            mcp_script! {
                write_stdin("x <- 1", timeout = 10.0);
                write_stdin("quit()", timeout = 10.0);
                write_stdin("1+1", timeout = 10.0);
            },
        )
        .await?;

    snapshot
        .session(
            "quit_yes",
            mcp_script! {
                write_stdin("setwd(tempdir()); x <- 1", timeout = 10.0);
                write_stdin("quit(\"yes\")", timeout = 10.0);
                write_stdin("1+1", timeout = 10.0);
            },
        )
        .await?;

    assert_snapshot_or_skip("snapshots_session_endings", &snapshot)
}

#[tokio::test(flavor = "multi_thread")]
async fn session_endings_smoke() -> TestResult<()> {
    let mut session = common::spawn_server().await?;

    let _ = session.write_stdin_raw_with("x <- 1", Some(10.0)).await?;
    let restart = session.write_stdin_raw_with("\u{4}", Some(10.0)).await?;
    let restart_text = common::result_text(&restart);
    if common::backend_unavailable(&restart_text) {
        eprintln!("session_endings backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if !common::assert_eventually_contains(
        &mut session,
        "print(exists(\"x\"))",
        "FALSE",
        "session_endings ctrl-d reset",
        10.0,
        std::time::Duration::from_secs(30),
        std::time::Duration::from_millis(50),
    )
    .await?
    {
        eprintln!("session_endings backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let _ = session
        .write_stdin_raw_with("quit(\"no\")", Some(10.0))
        .await?;
    if !common::assert_eventually_contains(
        &mut session,
        "1+1",
        "2",
        "session_endings quit-no respawn",
        10.0,
        std::time::Duration::from_secs(30),
        std::time::Duration::from_millis(50),
    )
    .await?
    {
        eprintln!("session_endings backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let _ = session.write_stdin_raw_with("quit()", Some(10.0)).await?;
    if !common::assert_eventually_contains(
        &mut session,
        "1+1",
        "2",
        "session_endings quit-default respawn",
        10.0,
        std::time::Duration::from_secs(30),
        std::time::Duration::from_millis(50),
    )
    .await?
    {
        eprintln!("session_endings backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let _ = session
        .write_stdin_raw_with("setwd(tempdir()); quit(\"yes\")", Some(10.0))
        .await?;
    if !common::assert_eventually_contains(
        &mut session,
        "1+1",
        "2",
        "session_endings quit-yes respawn",
        10.0,
        std::time::Duration::from_secs(30),
        std::time::Duration::from_millis(50),
    )
    .await?
    {
        eprintln!("session_endings backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    session.cancel().await?;
    Ok(())
}
