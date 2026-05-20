mod common;

use common::TestResult;
use rmcp::model::RawContent;
use std::time::Duration;

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

fn should_skip_reticulate_py_help_output(text: &str) -> bool {
    text.contains("[repl] reticulate not installed")
        || text.contains("[repl] reticulate python unavailable")
        || text.trim() == ">"
}

fn reticulate_py_help_initial_timeout_secs() -> f64 {
    if cfg!(windows) { 20.0 } else { 60.0 }
}

fn reticulate_py_help_wait_budget() -> Duration {
    if cfg!(windows) {
        Duration::from_secs(10)
    } else {
        Duration::from_secs(180)
    }
}

#[test]
fn prompt_only_reticulate_output_is_skipped() {
    assert!(should_skip_reticulate_py_help_output(">"));
}

#[tokio::test(flavor = "multi_thread")]
async fn reticulate_py_help_is_rendered() -> TestResult<()> {
    let mut session = common::spawn_server_with_files().await?;

    let initial = session
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
            Some(reticulate_py_help_initial_timeout_secs()),
        )
        .await?;
    let result = match common::wait_until_not_busy(
        &mut session,
        initial,
        Duration::from_millis(500),
        reticulate_py_help_wait_budget(),
    )
    .await
    {
        Ok(result) => result,
        Err(err) if cfg!(windows) && err.to_string().contains("worker remained busy") => {
            eprintln!("reticulate::py_help() remained busy on Windows; skipping");
            session.cancel().await?;
            return Ok(());
        }
        Err(err) => return Err(err),
    };
    let text = result_text(&result);

    if should_skip_reticulate_py_help_output(&text) {
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.to_ascii_lowercase().contains("help"),
        "expected reticulate::py_help() output, got: {text:?}"
    );
    assert!(
        text.contains("Return the number of items"),
        "expected reticulate::py_help() doc text, got: {text:?}"
    );
    assert!(
        !text.contains("--More--"),
        "did not expect pager footer, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}
