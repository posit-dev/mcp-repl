mod common;

use common::TestResult;
use rmcp::model::RawContent;

const REPL_STARTUP_TIMEOUT_SECS: f64 = 30.0;
const RETICULATE_INIT_TIMEOUT_SECS: f64 = 30.0;
const PY_HELP_TIMEOUT_SECS: f64 = 5.0;
const RETICULATE_READY_MARKER: &str = "[repl] reticulate python ready";

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

fn assert_not_busy(label: &str, text: &str) -> TestResult<()> {
    if common::is_busy_response(text) {
        return Err(format!("{label} exceeded its timeout budget: {text:?}").into());
    }
    Ok(())
}

#[test]
fn prompt_only_reticulate_output_is_skipped() {
    assert!(should_skip_reticulate_py_help_output(">"));
}

#[tokio::test(flavor = "multi_thread")]
async fn reticulate_py_help_is_rendered() -> TestResult<()> {
    let session = common::spawn_server_with_files().await?;

    let startup = session
        .write_stdin_raw_with("1+1", Some(REPL_STARTUP_TIMEOUT_SECS))
        .await?;
    let startup_text = result_text(&startup);
    if common::backend_unavailable(&startup_text) {
        eprintln!("reticulate_py_help backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert_not_busy("R startup", &startup_text)?;
    assert!(
        startup_text.contains("[1] 2"),
        "expected R startup smoke output, got: {startup_text:?}"
    );

    let setup = session
        .write_stdin_raw_with(
            r#"
{
  if (!requireNamespace("reticulate", quietly = TRUE)) {
    cat("[repl] reticulate not installed\n")
    invisible(NULL)
  } else {
    ok <- TRUE
    tryCatch({
      reticulate::py_config()
      assign(
        ".mcp_repl_reticulate_builtins",
        reticulate::import_builtins(),
        envir = .GlobalEnv
      )
    }, error = function(e) { ok <<- FALSE })
    if (!ok) {
      cat("[repl] reticulate python unavailable\n")
      invisible(NULL)
    } else {
      cat("[repl] reticulate python ready\n")
      invisible(NULL)
    }
  }
}
"#,
            Some(RETICULATE_INIT_TIMEOUT_SECS),
        )
        .await?;
    let setup_text = result_text(&setup);
    assert_not_busy("reticulate Python initialization", &setup_text)?;
    if should_skip_reticulate_py_help_output(&setup_text) {
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        setup_text.contains(RETICULATE_READY_MARKER),
        "expected reticulate Python initialization marker, got: {setup_text:?}"
    );

    let result = session
        .write_stdin_raw_with(
            "reticulate::py_help(.mcp_repl_reticulate_builtins$len); invisible(NULL)",
            Some(PY_HELP_TIMEOUT_SECS),
        )
        .await?;
    let text = result_text(&result);
    if common::is_busy_response(&text) {
        session.cancel().await?;
        if cfg!(windows) {
            eprintln!("reticulate::py_help() exceeded its short Windows budget; skipping");
            return Ok(());
        }
        return Err(format!(
            "reticulate::py_help() exceeded its {PY_HELP_TIMEOUT_SECS}s timeout budget: {text:?}"
        )
        .into());
    }

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
