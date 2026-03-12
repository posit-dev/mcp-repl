mod common;

use common::TestResult;
use rmcp::model::RawContent;
use serde_json::json;
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
        || text.contains("options(\"defaultPackages\") was not found")
        || text.contains("worker io error: Broken pipe")
}

fn overflow_path(text: &str) -> Option<PathBuf> {
    let marker = "full response at ";
    let start = text.find(marker)? + marker.len();
    let end = text[start..]
        .find('\n')
        .map(|idx| start + idx)
        .unwrap_or(text.len());
    Some(PathBuf::from(text[start..end].trim()))
}

#[tokio::test(flavor = "multi_thread")]
async fn file_show_overflows_to_server_managed_file_and_survives_reset() -> TestResult<()> {
    let mut session = common::spawn_server().await?;

    let result = session
        .write_stdin_raw_with(
            "line <- paste(rep(\"x\", 200), collapse = \"\"); tf <- tempfile(\"mcp-console-file-show-\"); writeLines(sprintf(\"file_show_line%04d %s\", 1:200, line), tf); file.show(tf, delete.file = TRUE); invisible(NULL)",
            Some(30.0),
        )
        .await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("r_file_show backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    assert!(
        text.contains("output truncated"),
        "expected truncation notice in output, got: {text:?}"
    );
    let overflow_path = overflow_path(&text)
        .ok_or_else(|| format!("expected overflow path in truncation notice, got: {text:?}"))?;
    assert!(
        overflow_path.is_absolute(),
        "expected absolute overflow path, got: {overflow_path:?}"
    );
    let file_name = overflow_path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| format!("overflow path missing filename: {overflow_path:?}"))?;
    assert!(
        file_name.starts_with("repl-response-"),
        "unexpected overflow filename: {file_name}"
    );
    assert!(
        file_name.ends_with(".txt"),
        "expected .txt overflow filename: {file_name}"
    );

    let path_literal = serde_json::to_string(overflow_path.as_os_str().to_string_lossy().as_ref())?;
    let verify_result = session
        .write_stdin_raw_with(
            format!(
                "lines <- readLines({path_literal}, warn = FALSE); matches <- lines[grepl(\"^file_show_line\", lines)]; cat(\"OVERFLOW_EXISTS=\", file.exists({path_literal}), \"\\n\", sep = \"\"); cat(\"OVERFLOW_MATCH_COUNT=\", length(matches), \"\\n\", sep = \"\"); cat(\"OVERFLOW_FIRST_MATCH=\", matches[1], \"\\n\", sep = \"\"); cat(\"OVERFLOW_LAST_MATCH=\", matches[length(matches)], \"\\n\", sep = \"\")"
            ),
            Some(30.0),
        )
        .await?;
    let verify_text = result_text(&verify_result);
    assert!(
        verify_text.contains("OVERFLOW_EXISTS=TRUE"),
        "expected overflow file to exist, got: {verify_text:?}"
    );
    assert!(
        verify_text.contains("OVERFLOW_MATCH_COUNT=200"),
        "expected 200 file.show lines in overflow file, got: {verify_text:?}"
    );
    assert!(
        verify_text.contains("OVERFLOW_FIRST_MATCH=file_show_line0001"),
        "expected first file.show line in overflow file, got: {verify_text:?}"
    );
    assert!(
        verify_text.contains("OVERFLOW_LAST_MATCH=file_show_line0200"),
        "expected last file.show line in overflow file, got: {verify_text:?}"
    );

    let reset_result = session.call_tool_raw("repl_reset", json!({})).await?;
    let reset_text = result_text(&reset_result);
    assert!(
        !backend_unavailable(&reset_text),
        "repl_reset unexpectedly failed: {reset_text:?}"
    );

    let persists_result = session
        .write_stdin_raw_with(
            format!(
                "cat(\"OVERFLOW_PERSISTS=\", file.exists({path_literal}), \"\\n\", sep = \"\")"
            ),
            Some(30.0),
        )
        .await?;
    let persists_text = result_text(&persists_result);
    assert!(
        persists_text.contains("OVERFLOW_PERSISTS=TRUE"),
        "expected overflow file to survive repl_reset, got: {persists_text:?}"
    );

    session.cancel().await?;
    Ok(())
}
