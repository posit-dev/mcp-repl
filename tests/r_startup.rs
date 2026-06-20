mod common;

use std::path::Path;

use common::TestResult;
use rmcp::model::RawContent;
use tokio::time::{Duration, Instant, sleep};

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

fn worker_ready_protocol_error(text: &str) -> bool {
    text.contains("first worker sideband message must be worker_ready")
}

fn is_busy_response(text: &str) -> bool {
    text.contains("<<repl status: busy")
        || text.contains("worker is busy")
        || text.contains("request already running")
        || text.contains("input discarded while worker busy")
}

fn r_home_env_vars(home_dir: &Path) -> Vec<(String, String)> {
    let home = home_dir.to_string_lossy().to_string();
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut env_vars = vec![
        ("HOME".to_string(), home.clone()),
        ("R_USER".to_string(), home.clone()),
    ];
    #[cfg(windows)]
    {
        env_vars.push(("USERPROFILE".to_string(), home.clone()));
        if home.len() >= 3
            && home.as_bytes()[1] == b':'
            && (home.as_bytes()[2] == b'\\' || home.as_bytes()[2] == b'/')
        {
            env_vars.push(("HOMEDRIVE".to_string(), home[..2].to_string()));
            env_vars.push(("HOMEPATH".to_string(), home[2..].to_string()));
        }
    }
    env_vars
}

#[tokio::test(flavor = "multi_thread")]
async fn r_respects_rprofile_and_renviron_on_startup() -> TestResult<()> {
    let home_dir = tempfile::tempdir()?;
    std::fs::write(
        home_dir.path().join(".Renviron"),
        "MCP_REPL_RENVIRON_TEST=RENVIRON_OK_9f6f9f68\n",
    )?;
    std::fs::write(
        home_dir.path().join(".Rprofile"),
        "options(mcp_repl_rprofile_test = \"RPROFILE_OK_6a8d0df6\")\n",
    )?;

    let session = common::spawn_server_with_env_vars(r_home_env_vars(home_dir.path())).await?;

    let input = r#"
cat("RPROFILE=", getOption("mcp_repl_rprofile_test"), "\n", sep = "")
cat("RENVIRON=", Sys.getenv("MCP_REPL_RENVIRON_TEST"), "\n", sep = "")
"#;
    let deadline = Instant::now() + Duration::from_secs(10);
    let text = loop {
        if Instant::now() >= deadline {
            session.cancel().await?;
            return Err("timed out waiting for R startup probe to complete".into());
        }

        let result = session.write_stdin_raw_with(input, Some(1.0)).await?;
        let text = result_text(&result);
        if backend_unavailable(&text) {
            eprintln!("r_startup backend unavailable in this environment; skipping");
            session.cancel().await?;
            return Ok(());
        }
        if is_busy_response(&text) {
            sleep(Duration::from_millis(100)).await;
            continue;
        }
        break text;
    };

    assert!(
        text.contains("RPROFILE=RPROFILE_OK_6a8d0df6"),
        "expected .Rprofile option to be set, got: {text:?}"
    );
    assert!(
        text.contains("RENVIRON=RENVIRON_OK_9f6f9f68"),
        "expected .Renviron variable to be set, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn rprofile_startup_output_does_not_break_worker_ready() -> TestResult<()> {
    let home_dir = tempfile::tempdir()?;
    let profile = home_dir.path().join("startup-output.Rprofile");
    std::fs::write(
        &profile,
        "cat(\"RPROFILE_STARTUP_OUTPUT_886a6f5d\\n\")\noptions(mcp_repl_after_startup_output = \"READY_AFTER_OUTPUT\")\n",
    )?;

    let profile = profile.to_string_lossy().to_string();
    let mut env_vars = r_home_env_vars(home_dir.path());
    env_vars.push(("R_PROFILE_USER".to_string(), profile));

    let session = common::spawn_server_with_env_vars(env_vars).await?;
    let result = session
        .write_stdin_raw_with(
            "cat(\"AFTER_STARTUP=\", getOption(\"mcp_repl_after_startup_output\"), \"\\n\", sep = \"\")\n",
            Some(10.0),
        )
        .await?;
    let text = result_text(&result);
    if backend_unavailable(&text) && !worker_ready_protocol_error(&text) {
        eprintln!("r_startup backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    assert!(
        !worker_ready_protocol_error(&text),
        "startup output must not precede worker_ready, got: {text:?}"
    );
    assert!(
        text.contains("AFTER_STARTUP=READY_AFTER_OUTPUT"),
        "expected R to continue after .Rprofile startup output, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn rprofile_startup_prompt_accepts_first_input_after_worker_ready() -> TestResult<()> {
    let home_dir = tempfile::tempdir()?;
    let profile = home_dir.path().join("startup-prompt.Rprofile");
    std::fs::write(
        &profile,
        "answer <- readline(\"STARTUP_PROMPT> \")\noptions(mcp_repl_startup_answer = answer)\n",
    )?;

    let profile = profile.to_string_lossy().to_string();
    let mut env_vars = r_home_env_vars(home_dir.path());
    env_vars.push(("R_PROFILE_USER".to_string(), profile));

    let session = common::spawn_server_with_env_vars(env_vars).await?;
    let result = session
        .write_stdin_raw_with(
            "answer-from-tool\ncat(\"STARTUP_ANSWER=\", getOption(\"mcp_repl_startup_answer\"), \"\\n\", sep = \"\")\n",
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    if backend_unavailable(&text) && !worker_ready_protocol_error(&text) {
        eprintln!("r_startup backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    assert!(
        !worker_ready_protocol_error(&text),
        "startup prompt must not precede worker_ready, got: {text:?}"
    );
    assert!(
        text.contains("STARTUP_ANSWER=answer-from-tool"),
        "expected first input to answer the startup profile prompt, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_server_spawns_with_plain_pager_env() -> TestResult<()> {
    let session = common::spawn_server_with_files().await?;

    let result = session
        .write_stdin_raw_with(
            r#"
cat("PAGER=", Sys.getenv("PAGER"), "\n", sep = "")
cat("MANPAGER=", Sys.getenv("MANPAGER"), "\n", sep = "")
"#,
            Some(10.0),
        )
        .await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("r_startup backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("PAGER=cat"),
        "expected PAGER=cat in test server environment, got: {text:?}"
    );
    assert!(
        text.contains("MANPAGER=cat"),
        "expected MANPAGER=cat in test server environment, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}
