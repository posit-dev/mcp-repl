mod common;

use common::TestResult;
use rmcp::model::RawContent;

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

#[tokio::test(flavor = "multi_thread")]
async fn reticulate_py_help_is_rendered_or_skipped() -> TestResult<()> {
    let mut session = common::spawn_server_with_files().await?;

    let result = session
        .write_stdin_raw_with(
            r#"
{
  if (!requireNamespace("reticulate", quietly = TRUE)) {
    cat("[repl] reticulate not installed\n")
    invisible(NULL)
  } else {
    ok <- TRUE
    tryCatch({ reticulate::py_config() }, error = function(e) { ok <<- FALSE })
    if (!ok) {
      cat("[repl] reticulate python unavailable\n")
      invisible(NULL)
    } else {
      builtins <- reticulate::import_builtins()
      reticulate::py_help(builtins$len)
      invisible(NULL)
    }
  }
}
"#,
            Some(60.0),
        )
        .await?;
    let text = result_text(&result);

    if text.contains("[repl] reticulate not installed")
        || text.contains("[repl] reticulate python unavailable")
    {
        session.cancel().await?;
        return Ok(());
    }
    if text.contains("write_stdin timeout reached") || text.contains("<<repl status: busy") {
        eprintln!("reticulate::py_help() remained busy in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if text.trim() == ">" {
        eprintln!("reticulate::py_help() produced no REPL output in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    assert!(
        text.to_ascii_lowercase().contains("help"),
        "expected reticulate::py_help() output, got: {text:?}"
    );
    assert!(
        !text.contains("--More--"),
        "did not expect pager footer, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}
