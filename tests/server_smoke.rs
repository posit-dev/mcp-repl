mod common;

#[cfg(not(windows))]
use common::McpSnapshot;
use common::TestResult;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::time;

fn resolve_mcp_repl_path() -> TestResult<PathBuf> {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_mcp-repl") {
        return Ok(PathBuf::from(path));
    }

    let mut path = std::env::current_exe()?;
    path.pop();
    path.pop();
    let mut candidate_path = path;
    candidate_path.push("mcp-repl");
    if cfg!(windows) {
        candidate_path.set_extension("exe");
    }
    if candidate_path.exists() {
        return Ok(candidate_path);
    }

    Err("unable to locate mcp-repl test binary".into())
}

#[test]
fn ipc_disconnect_is_not_treated_as_backend_unavailable() {
    let text = "worker protocol error: ipc disconnected while waiting for request completion";
    assert!(
        !common::backend_unavailable(text),
        "request-completion IPC disconnects should fail tests instead of skipping them"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn real_server_startup_stderr_omits_routine_notice() -> TestResult<()> {
    let exe = resolve_mcp_repl_path()?;
    let output = time::timeout(
        Duration::from_secs(15),
        Command::new(exe)
            .args(["--interpreter", "python", "--sandbox", "danger-full-access"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output(),
    )
    .await
    .map_err(|_| "server startup timed out")??;

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("starting mcp-repl server"),
        "routine startup notice should not be written to stderr: {stderr:?}"
    );
    assert!(
        !stderr.trim().is_empty(),
        "closed-stdin startup failure should still write diagnostic stderr"
    );
    Ok(())
}

#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread")]
async fn sends_input_to_r_console_snapshot() -> TestResult<()> {
    let mut snapshot = McpSnapshot::new();
    snapshot
        .session(
            "default",
            mcp_script! {
                write_stdin("1+1", timeout = 10.0);
            },
        )
        .await?;

    let rendered = snapshot.render();
    let transcript = snapshot.render_transcript();
    if common::backend_unavailable(&rendered) || common::backend_unavailable(&transcript) {
        eprintln!("server_smoke backend unavailable in this environment; skipping");
        return Ok(());
    }

    insta::assert_snapshot!("sends_input_to_r_console", rendered);
    insta::with_settings!({ snapshot_suffix => "transcript" }, {
        insta::assert_snapshot!("sends_input_to_r_console", transcript);
    });
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn retries_input_after_rejected_busy_response() -> TestResult<()> {
    let mut session = common::spawn_server().await?;

    let busy = session
        .write_stdin_raw_with("Sys.sleep(2)", Some(0.1))
        .await?;
    let busy_text = common::result_text(&busy);
    if common::backend_unavailable(&busy_text) {
        eprintln!("server_smoke backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        common::is_busy_response(&busy_text),
        "expected initial sleep to keep the worker busy, got: {busy_text:?}"
    );

    let dropped = session.write_stdin_raw_with("1+1", Some(0.5)).await?;
    let dropped_text = common::result_text(&dropped);
    assert!(
        common::is_busy_response(&dropped_text),
        "expected follow-up input to be rejected while busy, got: {dropped_text:?}"
    );

    let result = common::wait_until_ready_with_input_retry(
        &mut session,
        "1+1",
        dropped,
        5.0,
        std::time::Duration::from_millis(50),
        std::time::Duration::from_secs(10),
    )
    .await?;
    let text = common::result_text(&result);

    session.cancel().await?;

    assert!(
        text.contains("2"),
        "expected retried input to evaluate after busy worker drained, got: {text:?}"
    );
    Ok(())
}
