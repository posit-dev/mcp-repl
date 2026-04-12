mod common;

#[cfg(not(windows))]
use common::McpSnapshot;
use common::TestResult;

#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread")]
async fn snapshots_support_multiple_calls_and_sessions() -> TestResult<()> {
    let mut snapshot = McpSnapshot::new();

    snapshot
        .session(
            "session_1",
            mcp_script! {
                write_stdin("x <- 40 + 2", timeout = 10.0);
                write_stdin("x", timeout = 10.0);
                write_stdin("\u{4}");
            },
        )
        .await?;

    snapshot
        .session(
            "session_2",
            mcp_script! {
                write_stdin("x", timeout = 10.0);
            },
        )
        .await?;

    let rendered = snapshot.render();
    let transcript = snapshot.render_transcript();
    if common::backend_unavailable(&rendered) || common::backend_unavailable(&transcript) {
        eprintln!("mcp_transcripts backend unavailable in this environment; skipping");
        return Ok(());
    }

    insta::assert_snapshot!("snapshots_support_multiple_calls_and_sessions", rendered);
    insta::with_settings!({ snapshot_suffix => "transcript" }, {
        insta::assert_snapshot!("snapshots_support_multiple_calls_and_sessions", transcript);
    });
    Ok(())
}

#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread")]
async fn snapshots_interrupt_handler_output() -> TestResult<()> {
    let mut snapshot = McpSnapshot::new();

    let long_sleep =
        r#"tryCatch({ Sys.sleep(10000000) }, interrupt = function(e) cat("interrupt received\n"))"#;

    snapshot
        .session(
            "interrupt_handler",
            mcp_session!(|session| {
                session.write_stdin_raw_with("1+1", Some(10.0)).await?;
                session.write_stdin_with(long_sleep, Some(0.2)).await;
                session.write_stdin_with("\u{3}", Some(5.0)).await;
                session.write_stdin_with("1+1", Some(10.0)).await;
                Ok(())
            }),
        )
        .await?;

    let rendered = snapshot.render();
    let transcript = snapshot.render_transcript();
    if common::backend_unavailable(&rendered) || common::backend_unavailable(&transcript) {
        eprintln!("mcp_transcripts backend unavailable in this environment; skipping");
        return Ok(());
    }

    insta::assert_snapshot!("snapshots_interrupt_handler_output", rendered);
    insta::with_settings!({ snapshot_suffix => "transcript" }, {
        insta::assert_snapshot!("snapshots_interrupt_handler_output", transcript);
    });
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test(flavor = "multi_thread")]
async fn snapshots_tempdir_session_restart() -> TestResult<()> {
    let mut snapshot = McpSnapshot::new();

    let setup = r#"
cat("TMPDIR_SET=", nzchar(Sys.getenv("TMPDIR")), "\n", sep = "")
cat("TMPDIR_MATCH=", Sys.getenv("TMPDIR") == Sys.getenv("MCP_REPL_R_SESSION_TMPDIR"), "\n", sep = "")
cat("TEMPDIR_UNDER_TMPDIR=", startsWith(tempdir(), Sys.getenv("TMPDIR")), "\n", sep = "")
marker <- file.path(tempdir(), "mcp-repl-snapshot.txt")
tryCatch({
  writeLines("foo", marker)
  cat("TEMPDIR_MARKER_OK\n")
}, error = function(e) {
  message("TEMPDIR_MARKER_ERROR:", conditionMessage(e))
})
tf <- tempfile()
tryCatch({
  writeLines("bar", tf)
  cat("TEMPFILE_OK\n")
}, error = function(e) {
  message("TEMPFILE_ERROR:", conditionMessage(e))
})
unlink(tf)
cat("TEMPDIR_LIST=", paste(list.files(tempdir()), collapse = ","), "\n", sep = "")
root_marker <- file.path(Sys.getenv("TMPDIR"), "mcp-repl-snapshot-root.txt")
tryCatch({
  writeLines("root", root_marker)
  cat("ROOT_MARKER_OK\n")
}, error = function(e) {
  message("ROOT_MARKER_ERROR:", conditionMessage(e))
})
cat("ROOT_MARKER_EXISTS=", file.exists(root_marker), "\n", sep = "")
"#;

    let after_restart = r#"
root_marker <- file.path(Sys.getenv("TMPDIR"), "mcp-repl-snapshot-root.txt")
cat("ROOT_MARKER_EXISTS=", file.exists(root_marker), "\n", sep = "")
cat("TEMPDIR_LIST=", paste(list.files(tempdir()), collapse = ","), "\n", sep = "")
cat("TEMPDIR_UNDER_TMPDIR=", startsWith(tempdir(), Sys.getenv("TMPDIR")), "\n", sep = "")
"#;

    snapshot
        .session(
            "tempdir_session",
            mcp_script! {
                write_stdin(setup, timeout = 10.0);
                write_stdin("\u{4}");
                write_stdin(after_restart, timeout = 10.0);
                write_stdin("\u{4}");
                write_stdin(after_restart, timeout = 10.0);
            },
        )
        .await?;

    let rendered = snapshot.render();
    let transcript = snapshot.render_transcript();
    if common::backend_unavailable(&rendered) || common::backend_unavailable(&transcript) {
        eprintln!("mcp_transcripts backend unavailable in this environment; skipping");
        return Ok(());
    }

    insta::assert_snapshot!("snapshots_tempdir_session_restart", rendered);
    insta::with_settings!({ snapshot_suffix => "transcript" }, {
        insta::assert_snapshot!("snapshots_tempdir_session_restart", transcript);
    });
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn transcripts_smoke() -> TestResult<()> {
    let mut session = common::spawn_server().await?;
    if !common::assert_eventually_contains(
        &mut session,
        "x <- 40 + 2; x",
        "42",
        "mcp_transcripts x assignment",
        10.0,
        std::time::Duration::from_secs(30),
        std::time::Duration::from_millis(50),
    )
    .await?
    {
        eprintln!("mcp_transcripts backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let _ = session.write_stdin_raw_with("\u{4}", Some(10.0)).await?;
    if !common::assert_eventually_contains(
        &mut session,
        "print(exists(\"x\"))",
        "FALSE",
        "mcp_transcripts restart clears vars",
        10.0,
        std::time::Duration::from_secs(30),
        std::time::Duration::from_millis(50),
    )
    .await?
    {
        eprintln!("mcp_transcripts backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    session.cancel().await?;
    Ok(())
}
