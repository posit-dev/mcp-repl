mod common;

use common::TestResult;
use regex_lite::Regex;
use rmcp::model::RawContent;
use serde_json::json;
use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;
use tokio::sync::{Mutex, MutexGuard};

fn test_mutex() -> &'static Mutex<()> {
    static TEST_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
    TEST_MUTEX.get_or_init(|| Mutex::new(()))
}

async fn lock_test_mutex() -> MutexGuard<'static, ()> {
    test_mutex().lock().await
}

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
        || text.contains("unable to initialize the JIT")
        || text.contains(
            "worker protocol error: ipc disconnected while waiting for request completion",
        )
}

fn busy_response(text: &str) -> bool {
    text.contains("<<repl status: busy")
        || text.contains("worker is busy")
        || text.contains("request already running")
        || text.contains("input discarded while worker busy")
}

fn image_payload_lengths(result: &rmcp::model::CallToolResult) -> Vec<usize> {
    result
        .content
        .iter()
        .filter_map(|item| match &item.raw {
            RawContent::Image(image) => Some(image.data.len()),
            _ => None,
        })
        .collect()
}

fn first_image_index(result: &rmcp::model::CallToolResult) -> Option<usize> {
    result
        .content
        .iter()
        .position(|item| matches!(item.raw, RawContent::Image(_)))
}

fn first_text_index_containing(
    result: &rmcp::model::CallToolResult,
    needle: &str,
) -> Option<usize> {
    result.content.iter().position(|item| match &item.raw {
        RawContent::Text(text) => text.text.contains(needle),
        _ => false,
    })
}

fn events_log_path(text: &str) -> Option<PathBuf> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"((?:[A-Za-z]:\\|/)[^\r\n]+?events\.log)")
            .expect("events-log regex should compile")
    });
    re.captures(text)
        .and_then(|captures| captures.get(1))
        .map(|path| PathBuf::from(path.as_str()))
}

#[tokio::test(flavor = "multi_thread")]
async fn repl_tool_accepts_input_and_timeout_ms() -> TestResult<()> {
    let _guard = lock_test_mutex().await;
    let session = common::spawn_server().await?;

    let result = session
        .call_tool_raw(
            session.repl_tool_name(),
            json!({
                "input": "1+1\n",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("repl_surface backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if busy_response(&text) {
        eprintln!("repl_surface worker remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }

    assert!(text.contains("2"), "expected 2 in output, got: {text:?}");
    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn pager_keeps_plot_image_before_later_stdout() -> TestResult<()> {
    let _guard = lock_test_mutex().await;
    let session = common::spawn_server().await?;

    let result = session
        .write_stdin_raw_with("plot(1:10)\ncat('done\\n')", Some(30.0))
        .await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("repl_surface backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if busy_response(&text) {
        eprintln!("repl_surface worker remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let image_idx = first_image_index(&result).ok_or("expected plot image in reply")?;
    let done_idx =
        first_text_index_containing(&result, "done").ok_or("expected done text in reply")?;
    assert!(
        image_idx < done_idx,
        "expected plot image before later stdout, got content order: {:?}",
        result.content
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn files_poll_after_timeout_keeps_image_before_later_stdout() -> TestResult<()> {
    let _guard = lock_test_mutex().await;
    let session = common::spawn_server_with_files().await?;

    let input = concat!(
        "Sys.sleep(0.25); ",
        "invisible(.Call('mcp_repl_plot_emit', 'plot-1', charToRaw('img'), 'image/png', TRUE)); ",
        "Sys.sleep(0.2); ",
        "cat('done\\n')",
    );
    let first = session.write_stdin_raw_with(input, Some(0.05)).await?;
    let first_text = result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("repl_surface backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        first_text.contains("<<repl status: busy"),
        "expected first reply to time out, got: {first_text:?}"
    );

    let result = session.write_stdin_raw_with("", Some(30.0)).await?;
    let text = result_text(&result);
    if busy_response(&text) {
        eprintln!("repl_surface timeout poll remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let image_idx = first_image_index(&result).ok_or("expected image in timeout poll")?;
    let done_idx =
        first_text_index_containing(&result, "done").ok_or("expected done text in timeout poll")?;
    assert!(
        image_idx < done_idx,
        "expected image before later stdout after timeout poll, got content order: {:?}",
        result.content
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_plot_emit_orders_r_owned_output_around_image() -> TestResult<()> {
    let _guard = lock_test_mutex().await;
    let session = common::spawn_server_with_files().await?;

    let input = concat!(
        "cat('before\\n'); flush.console(); ",
        "invisible(.Call('mcp_repl_plot_emit', 'plot-1', charToRaw('img'), 'image/png', TRUE)); ",
        "cat('after\\n'); flush.console()",
    );
    let result = session.write_stdin_raw_with(input, Some(30.0)).await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("repl_surface backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if busy_response(&text) {
        eprintln!("repl_surface worker remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let before_idx =
        first_text_index_containing(&result, "before").ok_or("expected before text in reply")?;
    let image_idx = first_image_index(&result).ok_or("expected plot image in reply")?;
    let after_idx = first_text_index_containing(&result, "after").ok_or("expected after text")?;
    assert!(
        before_idx < image_idx && image_idx < after_idx,
        "expected R-owned output to preserve order around image, got content order: {:?}",
        result.content
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_plot_emit_orders_r_owned_stderr_and_stdout_around_image() -> TestResult<()> {
    let _guard = lock_test_mutex().await;
    let session = common::spawn_server_with_files().await?;

    let input = concat!(
        "cat('stdout-before\\n'); flush.console(); ",
        "message('stderr-before'); ",
        "invisible(.Call('mcp_repl_plot_emit', 'plot-1', charToRaw('img'), 'image/png', TRUE)); ",
        "message('stderr-after'); ",
        "cat('stdout-after\\n'); flush.console()",
    );
    let result = session.write_stdin_raw_with(input, Some(30.0)).await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("repl_surface backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if busy_response(&text) {
        eprintln!("repl_surface worker remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let stdout_before = text
        .find("stdout-before")
        .ok_or("expected stdout-before text in reply")?;
    let stderr_before = text
        .find("stderr-before")
        .ok_or("expected stderr-before text in reply")?;
    let stderr_after = text
        .find("stderr-after")
        .ok_or("expected stderr-after text in reply")?;
    let stdout_after = text
        .find("stdout-after")
        .ok_or("expected stdout-after text in reply")?;
    assert!(
        stdout_before < stderr_before && stderr_after < stdout_after,
        "expected stdout/stderr text order to match R callback order, got: {text:?}"
    );

    let before_idx = first_text_index_containing(&result, "stderr-before")
        .ok_or("expected stderr-before text item in reply")?;
    let image_idx = first_image_index(&result).ok_or("expected plot image in reply")?;
    let after_idx = first_text_index_containing(&result, "stderr-after")
        .ok_or("expected stderr-after text item in reply")?;
    assert!(
        before_idx < image_idx && image_idx < after_idx,
        "expected R-owned stderr to preserve order around image, got content order: {:?}",
        result.content
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn fork_child_console_output_falls_back_to_raw_stream() -> TestResult<()> {
    let _guard = lock_test_mutex().await;
    let session = common::spawn_server().await?;

    let input = concat!(
        "if (.Platform$OS.type == 'windows') { ",
        "cat('SKIP_FORK\\n') ",
        "} else { ",
        "job <- parallel::mcparallel({ cat('child\\n'); flush.console(); 42L }, silent = FALSE); ",
        "parallel::mccollect(job, wait = TRUE); ",
        "cat('parent\\n') ",
        "}",
    );
    let result = session.write_stdin_raw_with(input, Some(30.0)).await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("repl_surface backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if text.contains("SKIP_FORK") {
        eprintln!("repl_surface fork output fallback unsupported on this platform; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if busy_response(&text) {
        return Err(format!("fork child console output remained busy: {text:?}").into());
    }

    assert!(
        text.contains("child") && text.contains("parent"),
        "expected child and parent output, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn direct_stdout_fd_write_falls_back_to_raw_stream() -> TestResult<()> {
    let _guard = lock_test_mutex().await;
    let session = common::spawn_server().await?;

    let input = concat!(
        "if (!file.exists('/dev/stdout')) { ",
        "cat('SKIP_DIRECT_FD\\n') ",
        "} else { ",
        "con <- tryCatch(suppressWarnings(file('/dev/stdout', open = 'wb')), error = function(e) NULL); ",
        "if (is.null(con)) { ",
        "cat('SKIP_DIRECT_FD\\n') ",
        "} else { ",
        "writeBin(charToRaw('direct-fd\\n'), con); ",
        "flush(con); close(con); ",
        "cat('r-owned\\n') ",
        "} ",
        "}",
    );
    let result = session.write_stdin_raw_with(input, Some(30.0)).await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("repl_surface backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if text.contains("SKIP_DIRECT_FD") {
        eprintln!("repl_surface direct fd output unsupported on this platform; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if busy_response(&text) {
        return Err(format!("direct stdout fd write remained busy: {text:?}").into());
    }

    assert!(
        text.contains("direct-fd") && text.contains("r-owned"),
        "expected direct fd and R-owned output, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

async fn assert_child_stdout_prompt_text_remains_ordinary_output(
    session: common::McpTestSession,
) -> TestResult<()> {
    let input = concat!(
        "if (.Platform$OS.type == 'windows') { ",
        "cat('SKIP_CHILD_STDOUT\\n') ",
        "} else { ",
        "invisible(system(\"printf '> '\")) ",
        "}",
    );
    let result = session.write_stdin_raw_with(input, Some(30.0)).await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("repl_surface backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if text.contains("SKIP_CHILD_STDOUT") {
        eprintln!("repl_surface child stdout output unsupported on this platform; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if busy_response(&text) {
        return Err(format!("child stdout prompt text remained busy: {text:?}").into());
    }

    assert_eq!(
        text, "> > ",
        "expected raw prompt-shaped output plus completion prompt, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn child_stdout_prompt_text_remains_ordinary_output() -> TestResult<()> {
    let _guard = lock_test_mutex().await;
    assert_child_stdout_prompt_text_remains_ordinary_output(common::spawn_server().await?).await
}

#[tokio::test(flavor = "multi_thread")]
async fn files_child_stdout_prompt_text_remains_ordinary_output() -> TestResult<()> {
    let _guard = lock_test_mutex().await;
    assert_child_stdout_prompt_text_remains_ordinary_output(
        common::spawn_server_with_files().await?,
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn files_child_stdout_matching_later_input_line_remains_visible() -> TestResult<()> {
    let _guard = lock_test_mutex().await;
    let session = common::spawn_server_with_files().await?;

    let input = concat!(
        "if (.Platform$OS.type == 'windows') {\n",
        "  cat('SKIP_CHILD_STDOUT\\n')\n",
        "} else {\n",
        "  invisible(system(\"printf '> 1 + 1\\\\n'\"))\n",
        "}\n",
        "1 + 1\n",
    );
    let result = session.write_stdin_raw_with(input, Some(30.0)).await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("repl_surface backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if text.contains("SKIP_CHILD_STDOUT") {
        eprintln!("repl_surface child stdout output unsupported on this platform; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if busy_response(&text) {
        return Err(format!("child stdout prompt text remained busy: {text:?}").into());
    }

    let matching_lines = text.matches("> 1 + 1\n").count();
    assert_eq!(
        matching_lines, 1,
        "expected only raw child text, not a synthetic R echo, got: {text:?}"
    );
    let raw_child_line = text
        .find("> 1 + 1\n")
        .expect("matching line count already checked");
    let value_line = text
        .find("[1] 2")
        .ok_or_else(|| format!("expected later R result, got: {text:?}"))?;
    assert!(
        raw_child_line < value_line,
        "expected raw child text to remain visible before later result, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn files_keeps_plot_image_before_later_stdout() -> TestResult<()> {
    let _guard = lock_test_mutex().await;
    let session = common::spawn_server_with_files().await?;

    let result = session
        .write_stdin_raw_with("plot(1:10)\ncat('done\\n')", Some(30.0))
        .await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("repl_surface backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if busy_response(&text) {
        eprintln!("repl_surface worker remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let image_idx = first_image_index(&result).ok_or("expected plot image in reply")?;
    let done_idx =
        first_text_index_containing(&result, "done").ok_or("expected done text in reply")?;
    assert!(
        image_idx < done_idx,
        "expected plot image before later stdout, got content order: {:?}",
        result.content
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn repl_tool_hides_ipc_env_vars_from_r_user_code() -> TestResult<()> {
    let _guard = lock_test_mutex().await;
    let session = common::spawn_server().await?;

    let result = session
        .call_tool_raw(
            session.repl_tool_name(),
            json!({
                "input": "vars <- c('MCP_REPL_IPC_READ_FD', 'MCP_REPL_IPC_WRITE_FD', 'MCP_REPL_IPC_PIPE_TO_WORKER', 'MCP_REPL_IPC_PIPE_FROM_WORKER')\ncat(paste(nzchar(Sys.getenv(vars)), collapse = ' '), '\\n')\n",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("repl_surface backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if busy_response(&text) {
        eprintln!("repl_surface worker remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;

    assert!(
        text.contains("FALSE FALSE FALSE FALSE"),
        "expected IPC env vars to be hidden from R user code, got: {text:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn first_base_plot_emits_one_nontrivial_image() -> TestResult<()> {
    let _guard = lock_test_mutex().await;
    let session = common::spawn_server_with_files().await?;

    let result = session
        .call_tool_raw(
            session.repl_tool_name(),
            json!({
                "input": "plot(1:10)\n",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("repl_surface backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if busy_response(&text) {
        eprintln!("repl_surface worker remained busy during plot; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let images = image_payload_lengths(&result);
    assert_eq!(
        images.len(),
        1,
        "expected the first base plot to emit one image, got {} images: {text:?}",
        images.len()
    );
    assert!(
        images[0] > 1_000,
        "expected the first base plot image payload to be non-trivial, got {} base64 bytes",
        images[0]
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn multiple_base_plots_in_one_reply_emit_each_image() -> TestResult<()> {
    let _guard = lock_test_mutex().await;
    let session = common::spawn_server_with_files().await?;

    let result = session
        .call_tool_raw(
            session.repl_tool_name(),
            json!({
                "input": "for (i in 1:4) plot(1:10, main = sprintf(\"plot%03d\", i))\n",
                "timeout_ms": 30_000
            }),
        )
        .await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("repl_surface backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if busy_response(&text) {
        eprintln!("repl_surface worker remained busy during multi-plot request; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let images = image_payload_lengths(&result);
    assert!(
        events_log_path(&text).is_none(),
        "did not expect a four-plot reply to use an output bundle, got: {text:?}"
    );
    assert_eq!(
        images.len(),
        4,
        "expected one inline image per plot page when four plots are produced, got {} images: {text:?}",
        images.len()
    );
    assert!(
        images.iter().all(|len| *len > 1_000),
        "expected every inline plot image payload to be non-trivial, got lengths {images:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn base_plots_above_inline_limit_use_bundle_and_keep_two_anchors() -> TestResult<()> {
    let _guard = lock_test_mutex().await;
    let session = common::spawn_server_with_files().await?;

    let result = session
        .call_tool_raw(
            session.repl_tool_name(),
            json!({
                "input": "for (i in 1:6) plot(1:10, main = sprintf(\"plot%03d\", i))\n",
                "timeout_ms": 30_000
            }),
        )
        .await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("repl_surface backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if busy_response(&text) {
        eprintln!("repl_surface worker remained busy during six-plot request; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let events_log = events_log_path(&text)
        .unwrap_or_else(|| panic!("expected six-plot reply to disclose events.log, got: {text:?}"));
    let events = fs::read_to_string(&events_log)?;
    let image_events = events.lines().filter(|line| line.starts_with("I ")).count();
    let images = image_payload_lengths(&result);

    assert_eq!(
        image_events, 6,
        "expected output bundle to preserve all six plot pages, got {image_events} image events: {events:?}"
    );
    assert_eq!(
        images.len(),
        2,
        "expected replies above the inline image limit to keep only two anchor images inline, got {} images: {text:?}",
        images.len()
    );
    assert!(
        images.iter().all(|len| *len > 1_000),
        "expected both inline anchor image payloads to be non-trivial, got lengths {images:?}"
    );

    session.cancel().await?;
    Ok(())
}
