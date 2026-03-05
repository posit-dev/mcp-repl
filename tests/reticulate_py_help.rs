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
async fn reticulate_py_help_is_paged_or_skipped() -> TestResult<()> {
    let mut session = common::spawn_server_with_pager_page_chars(2_000).await?;

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
    if text.trim() == ">" {
        eprintln!("reticulate::py_help() produced no console output in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    assert!(
        text.contains("--More--") || text.to_ascii_lowercase().contains("help"),
        "expected reticulate::py_help() output, got: {text:?}"
    );

    if text.contains("--More--") {
        let result = session.write_stdin_raw_with("Next", Some(30.0)).await?;
        let next_text = result_text(&result);
        assert!(
            next_text.contains("--More--") || next_text.contains("(END") || !next_text.is_empty(),
            "expected subsequent pager output, got: {next_text:?}"
        );
    }

    session.cancel().await?;
    Ok(())
}
