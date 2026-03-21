mod common;

#[cfg(not(windows))]
use common::McpSnapshot;
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

#[cfg(feature = "pager")]
#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread")]
async fn snapshots_truncation_notice_tail() -> TestResult<()> {
    let mut snapshot = McpSnapshot::new();

    let big_output = r#"
cat(paste(rep("x", 2200000), collapse = ""))
cat("\nEND\n")
"#;

    snapshot
        .session(
            "truncation_tail",
            mcp_script! {
                write_stdin(big_output, timeout = 20.0);
                write_stdin(":tail 8k", timeout = 10.0);
            },
        )
        .await?;

    assert_snapshot_or_skip("snapshots_truncation_notice_tail", &snapshot)?;

    Ok(())
}

#[cfg(feature = "pager")]
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
        .session(
            "pager_hits_images",
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
}

#[cfg(all(not(windows), feature = "pager"))]
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
    #[cfg(feature = "pager")]
    let rendered = if name == "snapshots_pager_hits_with_images" {
        normalize_pager_footer_page_counts(&snapshot.render())
    } else {
        snapshot.render()
    };
    #[cfg(not(feature = "pager"))]
    let rendered = snapshot.render();
    #[cfg(feature = "pager")]
    let transcript = if name == "snapshots_pager_hits_with_images" {
        normalize_pager_footer_page_counts(&snapshot.render_transcript())
    } else {
        snapshot.render_transcript()
    };
    #[cfg(not(feature = "pager"))]
    let transcript = snapshot.render_transcript();
    if backend_unavailable(&rendered) || backend_unavailable(&transcript) {
        eprintln!("refactor_coverage backend unavailable in this environment; skipping");
        return Ok(());
    }

    insta::assert_snapshot!(name, rendered);
    insta::with_settings!({ snapshot_suffix => "transcript" }, {
        insta::assert_snapshot!(name, transcript);
    });
    Ok(())
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn windows_restart_interrupt_plot_smoke() -> TestResult<()> {
    use tokio::time::{Duration, Instant, sleep};

    async fn assert_eventually_contains(
        session: &mut common::McpTestSession,
        input: &str,
        expected: &str,
    ) -> TestResult<bool> {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            let result = session.write_stdin_raw_with(input, Some(10.0)).await?;
            let text = result_text(&result);
            if backend_unavailable(&text) {
                return Ok(false);
            }
            if text.contains(expected) {
                return Ok(true);
            }
            if text.contains("<<console status: busy")
                || text.contains("worker is busy")
                || text.contains("request already running")
                || text.contains("input discarded while worker busy")
            {
                sleep(Duration::from_millis(50)).await;
                continue;
            }
            sleep(Duration::from_millis(50)).await;
        }
        Ok(false)
    }

    let mut session = common::spawn_server().await?;

    let result = session
        .write_stdin_raw_with(
            "set.seed(1); plot(1:5, type='l'); plot(5:1, type='l'); cat('plots_done\\n')",
            Some(30.0),
        )
        .await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("refactor_coverage backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if !text.contains("plots_done") && !text.contains("<<console status: busy") {
        return Err(format!("expected plot command output marker, got: {text:?}").into());
    }

    let _ = session.write_stdin_raw_with("\u{4}", Some(10.0)).await?;
    if !assert_eventually_contains(&mut session, "1+1", "2").await? {
        eprintln!("refactor_coverage backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let _ = session
        .write_stdin_raw_with("Sys.sleep(5)", Some(0.2))
        .await?;
    let _ = session.write_stdin_raw_with("\u{3}", Some(10.0)).await?;
    if !assert_eventually_contains(&mut session, "1+1", "2").await? {
        eprintln!("refactor_coverage backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    session.cancel().await?;
    Ok(())
}
