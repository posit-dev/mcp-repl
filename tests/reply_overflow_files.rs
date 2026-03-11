mod common;

use common::TestResult;
use rmcp::model::RawContent;
use serde_json::json;
use std::fs;
use std::path::PathBuf;

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
}

fn extract_bracketed_path(text: &str, marker: &str) -> Option<PathBuf> {
    text.lines().find_map(|line| {
        let start = line.find(marker)?;
        let rest = &line[start + marker.len()..];
        let end = rest.find(']').unwrap_or(rest.len());
        Some(PathBuf::from(&rest[..end]))
    })
}

fn extract_bracketed_paths(text: &str, marker: &str) -> Vec<PathBuf> {
    text.lines()
        .filter_map(|line| {
            let start = line.find(marker)?;
            let rest = &line[start + marker.len()..];
            let end = rest.find(']').unwrap_or(rest.len());
            Some(PathBuf::from(&rest[..end]))
        })
        .collect()
}

fn small_overflow_args() -> Vec<String> {
    vec![
        "--config".to_string(),
        "reply_overflow.text.preview_bytes=64".to_string(),
        "--config".to_string(),
        "reply_overflow.text.spill_bytes=64".to_string(),
    ]
}

#[tokio::test(flavor = "multi_thread")]
async fn oversized_output_defaults_to_files_and_reset_clears_reply_files() -> TestResult<()> {
    let mut session = common::spawn_server_with_args(small_overflow_args()).await?;

    let result = session
        .write_stdin_raw_with("cat(strrep('x', 400))", Some(10.0))
        .await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("reply overflow backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    assert!(
        text.contains("[reply files: "),
        "expected files reply annotation, got: {text}"
    );
    assert!(
        !text.contains("--More--"),
        "files mode should not activate pager, got: {text}"
    );

    let text_path = extract_bracketed_path(&text, "[full text saved to ")
        .ok_or("missing full text file annotation")?;
    assert!(
        text_path.exists(),
        "expected saved file at {}",
        text_path.display()
    );
    assert_eq!(fs::read_to_string(&text_path)?, "x".repeat(400));

    let _ = session.call_tool_raw("repl_reset", json!({})).await?;
    assert!(
        !text_path.exists(),
        "repl_reset should clear prior reply files: {}",
        text_path.display()
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn images_overflow_to_files_when_preview_limit_is_exceeded() -> TestResult<()> {
    let mut args = small_overflow_args();
    args.extend([
        "--config".to_string(),
        "reply_overflow.images.preview_count=1".to_string(),
        "--config".to_string(),
        "reply_overflow.images.spill_count=1".to_string(),
    ]);
    let mut session = common::spawn_server_with_args(args).await?;

    let result = session
        .write_stdin_raw_with(
            "plot(1:5); plot(2:6); plot(3:7); cat('plots_done\\n')",
            Some(30.0),
        )
        .await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("reply overflow backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let inline_images = result
        .content
        .iter()
        .filter(|item| matches!(&item.raw, RawContent::Image(_)))
        .count();
    let image_paths = extract_bracketed_paths(&text, "[saved image: ");
    assert_eq!(
        inline_images, 1,
        "expected one inline image preview, got: {text}"
    );
    assert!(
        image_paths.len() >= 2,
        "expected omitted images to be written to files, got: {text}"
    );
    for path in image_paths {
        assert!(
            path.exists(),
            "expected saved image file at {}",
            path.display()
        );
    }

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn reply_files_survive_worker_respawn() -> TestResult<()> {
    let mut session = common::spawn_server_with_args(small_overflow_args()).await?;

    let result = session
        .write_stdin_raw_with("cat(strrep('x', 400))", Some(10.0))
        .await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("reply overflow backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let text_path = extract_bracketed_path(&text, "[full text saved to ")
        .ok_or("missing full text file annotation")?;
    assert!(
        text_path.exists(),
        "expected saved file at {}",
        text_path.display()
    );

    let _ = session
        .write_stdin_raw_with("quit('no')", Some(10.0))
        .await?;
    let follow_up = session.write_stdin_raw_with("1+1", Some(10.0)).await?;
    let follow_text = result_text(&follow_up);
    assert!(
        !backend_unavailable(&follow_text),
        "expected worker respawn after session end, got: {follow_text}"
    );
    assert!(
        text_path.exists(),
        "reply files should survive worker respawn: {}",
        text_path.display()
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn reply_text_file_captures_output_larger_than_ring() -> TestResult<()> {
    let mut session = common::spawn_server_with_args(small_overflow_args()).await?;

    let result = session
        .write_stdin_raw_with(
            "cat(rawToChar(as.raw(c(66, 69, 71, 73, 78, 10))), strrep('x', 2200000), rawToChar(as.raw(c(10, 69, 78, 68, 10))), sep = '')",
            Some(60.0),
        )
        .await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("reply overflow backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let text_path = extract_bracketed_path(&text, "[full text saved to ")
        .ok_or("missing full text file annotation")?;
    let saved = fs::read_to_string(&text_path)?;
    assert!(saved.contains("BEGIN\n"), "missing BEGIN marker");
    assert!(saved.contains("\nEND\n"), "missing END marker");
    assert!(
        saved.len() > 2_000_000,
        "expected saved output larger than ring limit, got {} bytes",
        saved.len()
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn r_options_update_behavior_and_reset_restores_launch_defaults() -> TestResult<()> {
    let mut session = common::spawn_server_with_args(small_overflow_args()).await?;

    let set_options = session
        .write_stdin_raw_with(
            "invisible(options(mcp.reply_overflow.behavior = 'pager'))",
            Some(10.0),
        )
        .await?;
    let set_options_text = result_text(&set_options);
    if backend_unavailable(&set_options_text) {
        eprintln!("reply overflow backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let pager_result = session
        .write_stdin_raw_with(
            "for (i in 1:200) cat(sprintf('line%04d %s\\n', i, strrep('x', 40)))",
            Some(10.0),
        )
        .await?;
    let pager_text = result_text(&pager_result);
    assert!(
        pager_text.contains("--More--"),
        "expected pager after runtime option update, got: {pager_text}"
    );
    assert!(
        !pager_text.contains("[reply files: "),
        "pager mode should not emit files annotations, got: {pager_text}"
    );

    let _ = session.call_tool_raw("repl_reset", json!({})).await?;

    let files_result = session
        .write_stdin_raw_with("cat(strrep('x', 400))", Some(10.0))
        .await?;
    let files_text = result_text(&files_result);
    assert!(
        files_text.contains("[reply files: "),
        "expected files mode after repl_reset restored defaults, got: {files_text}"
    );
    assert!(
        !files_text.contains("--More--"),
        "files mode should stay non-modal after reset, got: {files_text}"
    );

    session.cancel().await?;
    Ok(())
}
