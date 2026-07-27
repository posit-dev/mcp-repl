#![cfg(windows)]

mod common;

use common::TestResult;

const RETICULATE_LIBRARY_ENV: &str = "MCP_REPL_RETICULATE_LIBRARY";
const RETICULATE_PYTHON_ENV: &str = "MCP_REPL_RETICULATE_PYTHON";

fn configured_path(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

#[tokio::test(flavor = "multi_thread")]
async fn reticulate_handler_interrupts_r_input_without_stale_python_state() -> TestResult<()> {
    let Some(reticulate_library) = configured_path(RETICULATE_LIBRARY_ENV) else {
        eprintln!(
            "set {RETICULATE_LIBRARY_ENV} to run the real Windows reticulate interrupt regression"
        );
        return Ok(());
    };
    let Some(python) = configured_path(RETICULATE_PYTHON_ENV) else {
        eprintln!(
            "set {RETICULATE_PYTHON_ENV} to run the real Windows reticulate interrupt regression"
        );
        return Ok(());
    };

    let session = common::spawn_server_with_args(vec![
        "--oversized-output".to_string(),
        "files".to_string(),
        "--sandbox".to_string(),
        "danger-full-access".to_string(),
    ])
    .await?;
    let setup = format!(
        r#"
.libPaths(c({}, .libPaths()))
Sys.setenv(RETICULATE_PYTHON = {})
suppressPackageStartupMessages(library(reticulate))
cfg <- py_config()
reticulate:::install_interrupt_handlers()
py_run_string("mcp_repl_reticulate_value = 41")
cat("RETICULATE_HANDLER_INSTALLED", as.character(cfg$version), "\n")
tryCatch(
  {{
    value <- readline("reticulate-native-input> ")
    cat("RETICULATE_UNEXPECTED_INPUT", value, "\n")
  }},
  interrupt = function(e) cat("RETICULATE_R_INTERRUPTED\n")
)
"#,
        serde_json::to_string(&reticulate_library)?,
        serde_json::to_string(&python)?,
    );

    let waiting = session.write_stdin_raw_with(setup, Some(30.0)).await?;
    let waiting_text = common::result_text(&waiting);
    if common::backend_unavailable(&waiting_text) {
        eprintln!("R backend unavailable; skipping reticulate interrupt regression");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        waiting_text.contains("RETICULATE_HANDLER_INSTALLED")
            && waiting_text.contains("reticulate-native-input> "),
        "reticulate did not initialize and enter managed input: {waiting_text:?}"
    );

    let interrupted = session
        .write_stdin_raw_unterminated_with("\u{3}", Some(15.0))
        .await?;
    let interrupted_text = common::result_text(&interrupted);
    assert!(
        interrupted_text.contains("RETICULATE_R_INTERRUPTED"),
        "reticulate's native handler did not deliver R's interrupt condition: {interrupted_text:?}"
    );
    assert!(
        !interrupted_text.contains("RETICULATE_UNEXPECTED_INPUT"),
        "managed R input returned instead of interrupting: {interrupted_text:?}"
    );

    let follow_up = session
        .write_stdin_raw_with(
            r#"
cat("RETICULATE_R_FOLLOWUP", 6 * 7, "\n")
cat("RETICULATE_PY_FOLLOWUP", py_eval("mcp_repl_reticulate_value + 1"), "\n")
"#,
            Some(15.0),
        )
        .await?;
    let follow_up_text = common::result_text(&follow_up);
    assert!(
        follow_up_text.contains("RETICULATE_R_FOLLOWUP 42")
            && follow_up_text.contains("RETICULATE_PY_FOLLOWUP 42"),
        "R or reticulate Python retained stale interrupt state: {follow_up_text:?}"
    );
    assert!(
        !follow_up_text.contains("KeyboardInterrupt"),
        "a stale Python interrupt reached the reticulate follow-up: {follow_up_text:?}"
    );

    session.cancel().await?;
    Ok(())
}
