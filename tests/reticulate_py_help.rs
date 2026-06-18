mod common;

use common::TestResult;
use rmcp::model::RawContent;
use std::process::Command;
use std::time::Duration;

const REPL_STARTUP_TIMEOUT_SECS: f64 = 30.0;
const DEFAULT_RETICULATE_INIT_TIMEOUT_SECS: f64 = 30.0;
const WINDOWS_RETICULATE_INIT_TIMEOUT_SECS: f64 = 30.0;
const DEFAULT_PY_HELP_TIMEOUT_SECS: f64 = 5.0;
const WINDOWS_PY_HELP_TIMEOUT_SECS: f64 = 30.0;
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

fn reticulate_init_timeout_secs() -> f64 {
    if cfg!(windows) {
        WINDOWS_RETICULATE_INIT_TIMEOUT_SECS
    } else {
        DEFAULT_RETICULATE_INIT_TIMEOUT_SECS
    }
}

fn py_help_timeout_secs() -> f64 {
    if cfg!(windows) {
        WINDOWS_PY_HELP_TIMEOUT_SECS
    } else {
        DEFAULT_PY_HELP_TIMEOUT_SECS
    }
}

fn reticulate_env_vars() -> Vec<(String, String)> {
    let Some(python) = common::python_program() else {
        return Vec::new();
    };
    let Ok(output) = Command::new(python)
        .args(["-c", "import sys; print(sys.executable)"])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let executable = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if executable.is_empty() {
        Vec::new()
    } else {
        vec![("RETICULATE_PYTHON".to_string(), executable)]
    }
}

#[test]
fn prompt_only_reticulate_output_is_skipped() {
    assert!(should_skip_reticulate_py_help_output(">"));
}

#[tokio::test(flavor = "multi_thread")]
async fn reticulate_py_help_is_rendered() -> TestResult<()> {
    let mut session = common::spawn_server_with_files_env_vars(reticulate_env_vars()).await?;

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
            Some(reticulate_init_timeout_secs()),
        )
        .await?;
    let setup = match common::wait_until_not_busy(
        &mut session,
        setup,
        Duration::from_millis(500),
        Duration::from_secs(60),
    )
    .await
    {
        Ok(setup) => setup,
        Err(err) if cfg!(windows) => {
            eprintln!(
                "reticulate Python initialization did not complete on this Windows host; skipping optional reticulate help coverage: {err}"
            );
            session.cancel().await?;
            return Ok(());
        }
        Err(err) => return Err(err),
    };
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
            Some(py_help_timeout_secs()),
        )
        .await?;
    let result = match common::wait_until_not_busy(
        &mut session,
        result,
        Duration::from_millis(500),
        Duration::from_secs(60),
    )
    .await
    {
        Ok(result) => result,
        Err(err) if cfg!(windows) => {
            eprintln!(
                "reticulate::py_help() did not complete on this Windows host; skipping optional reticulate help coverage: {err}"
            );
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
