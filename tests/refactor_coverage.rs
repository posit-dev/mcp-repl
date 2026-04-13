mod common;

#[cfg(not(windows))]
use common::McpSnapshot;
use common::TestResult;

#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread")]
async fn snapshots_restart_and_interrupt_with_plots() -> TestResult<()> {
    let mut snapshot = McpSnapshot::new();

    snapshot
        .session(
            "restart_interrupt",
            mcp_script! {
                write_stdin(r#"
set.seed(1)
plot(1:5, type = "l")
"#, timeout = 10.0);
                write_stdin(r#"
plot(5:1, type = "l")
cat("plots_done\n")
"#, timeout = 10.0);
                write_stdin("\u{4}");
                write_stdin("1+1", timeout = 10.0);
                write_stdin("Sys.sleep(5)", timeout = 0.2);
                write_stdin("\u{3}", timeout = 5.0);
                write_stdin("1+1", timeout = 10.0);
            },
        )
        .await?;

    assert_snapshot_or_skip("snapshots_restart_and_interrupt_with_plots", &snapshot)?;

    Ok(())
}

#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread")]
async fn snapshots_browser_prompt_and_continue() -> TestResult<()> {
    let mut snapshot = McpSnapshot::new();

    snapshot
        .session(
            "browser_prompt",
            mcp_script! {
                write_stdin("browser()", timeout = 10.0);
                write_stdin("c", timeout = 10.0);
                write_stdin("1+1", timeout = 10.0);
            },
        )
        .await?;

    assert_snapshot_or_skip("snapshots_browser_prompt_and_continue", &snapshot)?;

    Ok(())
}

#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread")]
async fn snapshots_truncation_notice_tail() -> TestResult<()> {
    let mut snapshot = McpSnapshot::new();

    let big_output = r#"
cat(paste(rep("x", 2200000), collapse = ""))
cat("\nEND\n")
"#;

    snapshot
        .pager_session(
            "truncation_tail",
            300,
            mcp_script! {
                write_stdin(big_output, timeout = 20.0);
                write_stdin(":tail 8k", timeout = 10.0);
            },
        )
        .await?;

    assert_snapshot_or_skip("snapshots_truncation_notice_tail", &snapshot)?;

    Ok(())
}

#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread")]
async fn snapshots_pager_hits_with_images() -> TestResult<()> {
    let mut snapshot = McpSnapshot::new();

    let output = r##"
set.seed(1)
cat("# Title\n")
for (i in 1:60) cat("alpha line ", i, "\n", sep = "")
plot(1:5, type = "l")
for (i in 1:60) cat("beta line ", i, "\n", sep = "")
plot(5:1, type = "l")
for (i in 1:60) cat("gamma line ", i, "\n", sep = "")
"##;

    snapshot
        .pager_session(
            "pager_hits_images",
            300,
            mcp_script! {
                write_stdin(output, timeout = 10.0);
                write_stdin(":hits alpha", timeout = 10.0);
                write_stdin(":seek 0", timeout = 10.0);
                write_stdin(":hits beta", timeout = 10.0);
            },
        )
        .await?;

    assert_snapshot_or_skip("snapshots_pager_hits_with_images", &snapshot)?;

    Ok(())
}

fn initial_plot_command_completed(text: &str) -> bool {
    text.contains("plots_done") || common::is_busy_response(text) || text.contains("--More-- (")
}

#[cfg(not(windows))]
fn normalize_pager_footer_page_counts(text: &str) -> String {
    let marker = "--More-- (";
    let mut out = String::with_capacity(text.len());
    let mut idx = 0;
    while let Some(pos) = text[idx..].find(marker) {
        let abs = idx + pos;
        out.push_str(&text[idx..abs]);
        out.push_str(marker);

        let start = abs + marker.len();
        let bytes = text.as_bytes();
        let mut end = start;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }

        if end > start && end < bytes.len() && bytes[end] == b'p' {
            out.push('N');
            idx = end;
            continue;
        }

        out.push_str(&text[start..end]);
        idx = end;
    }
    out.push_str(&text[idx..]);
    out
}

#[cfg(not(windows))]
fn assert_snapshot_or_skip(name: &str, snapshot: &McpSnapshot) -> TestResult<()> {
    let rendered = if name == "snapshots_pager_hits_with_images" {
        normalize_pager_footer_page_counts(&snapshot.render())
    } else {
        snapshot.render()
    };
    let transcript = if name == "snapshots_pager_hits_with_images" {
        normalize_pager_footer_page_counts(&snapshot.render_transcript())
    } else {
        snapshot.render_transcript()
    };
    if common::backend_unavailable(&rendered) || common::backend_unavailable(&transcript) {
        eprintln!("refactor_coverage backend unavailable in this environment; skipping");
        return Ok(());
    }

    insta::assert_snapshot!(name, rendered);
    insta::with_settings!({ snapshot_suffix => "transcript" }, {
        insta::assert_snapshot!(name, transcript);
    });
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_interrupt_plot_smoke() -> TestResult<()> {
    let mut session = common::spawn_server().await?;

    let result = session
        .write_stdin_raw_with(
            "set.seed(1); plot(1:5, type='l'); plot(5:1, type='l'); cat('plots_done\\n')",
            Some(30.0),
        )
        .await?;
    let text = common::result_text(&result);
    if common::backend_unavailable(&text) {
        eprintln!("refactor_coverage backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if !initial_plot_command_completed(&text) {
        return Err(format!("expected plot command output marker, got: {text:?}").into());
    }

    let _ = session.write_stdin_raw_with("\u{4}", Some(10.0)).await?;
    if !common::assert_eventually_contains(
        &mut session,
        "1+1",
        "2",
        "refactor_coverage restart recovery",
        10.0,
        std::time::Duration::from_secs(30),
        std::time::Duration::from_millis(50),
    )
    .await?
    {
        eprintln!("refactor_coverage backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let _ = session
        .write_stdin_raw_with("Sys.sleep(5)", Some(0.2))
        .await?;
    let _ = session.write_stdin_raw_with("\u{3}", Some(10.0)).await?;
    if !common::assert_eventually_contains(
        &mut session,
        "1+1",
        "2",
        "refactor_coverage interrupt recovery",
        10.0,
        std::time::Duration::from_secs(30),
        std::time::Duration::from_millis(50),
    )
    .await?
    {
        eprintln!("refactor_coverage backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    session.cancel().await?;
    Ok(())
}
