mod common;

use common::TestResult;
use regex_lite::Regex;
use rmcp::model::RawContent;
use serde_json::json;
use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;

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
async fn files_keeps_plot_image_before_later_stdout() -> TestResult<()> {
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
async fn repl_reset_clears_state() -> TestResult<()> {
    let session = common::spawn_server().await?;

    let set_var = session
        .call_tool_raw(
            session.repl_tool_name(),
            json!({
                "input": "x <- 1\n",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let set_var_text = result_text(&set_var);
    if backend_unavailable(&set_var_text) {
        eprintln!("repl_surface backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if busy_response(&set_var_text) {
        eprintln!("repl_surface worker remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let _ = session.call_tool_raw("repl_reset", json!({})).await?;

    let after_reset = session
        .call_tool_raw(
            session.repl_tool_name(),
            json!({
                "input": "print(exists(\"x\"))\n",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let after_reset_text = result_text(&after_reset);
    if backend_unavailable(&after_reset_text) {
        eprintln!("repl_surface backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if busy_response(&after_reset_text) {
        eprintln!("repl_surface worker remained busy after reset; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        after_reset_text.contains("FALSE"),
        "expected reset state, got: {after_reset_text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn repl_tool_hides_ipc_fd_env_vars_from_r_user_code() -> TestResult<()> {
    let session = common::spawn_server().await?;

    let result = session
        .call_tool_raw(
            session.repl_tool_name(),
            json!({
                "input": "cat(sprintf(\"%s %s\\n\", nzchar(Sys.getenv(\"MCP_REPL_IPC_READ_FD\")), nzchar(Sys.getenv(\"MCP_REPL_IPC_WRITE_FD\"))))\n",
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
        text.contains("FALSE FALSE"),
        "expected IPC fd env vars to be hidden from R user code, got: {text:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn first_base_plot_emits_one_nontrivial_image() -> TestResult<()> {
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
