#![allow(clippy::await_holding_lock)]

mod common;

use common::TestResult;
use serde_json::{Map, Value, json};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

fn test_mutex() -> &'static Mutex<()> {
    static TEST_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
    TEST_MUTEX.get_or_init(|| Mutex::new(()))
}

fn server_path() -> TestResult<PathBuf> {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_mcp-repl") {
        return Ok(PathBuf::from(path));
    }

    let mut path = std::env::current_exe()?;
    path.pop();
    path.pop();
    let mut candidate = path.join("mcp-repl");
    if cfg!(windows) {
        candidate.set_extension("exe");
    }
    if candidate.exists() {
        return Ok(candidate);
    }
    Err("unable to locate mcp-repl test binary".into())
}

async fn spawn_custom_r_worker_server() -> TestResult<common::McpTestSession> {
    let tempdir = tempfile::tempdir()?;
    let spec_path = tempdir.path().join("r-worker.json");
    let env = Map::from_iter([(
        "MCP_REPL_INTERPRETER".to_string(),
        Value::String("r".to_string()),
    )]);
    let spec = json!({
        "executable": server_path()?,
        "args": ["worker"],
        "working_dir": "inherit",
        "env": env,
        "stdin": "pipe",
        "sandbox": "server"
    });
    std::fs::write(&spec_path, serde_json::to_vec_pretty(&spec)?)?;
    common::spawn_server_with_args(vec![
        "--worker-spec".to_string(),
        spec_path.display().to_string(),
        "--sandbox".to_string(),
        "danger-full-access".to_string(),
        "--oversized-output".to_string(),
        "files".to_string(),
    ])
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn custom_r_worker_spec_uses_generic_protocol() -> TestResult<()> {
    let _guard = test_mutex()
        .lock()
        .map_err(|_| "r_protocol test mutex poisoned")?;
    let session = spawn_custom_r_worker_server().await?;

    let result = session
        .write_stdin_raw_unterminated_with("1+1", Some(2.0))
        .await?;
    let text = common::result_text(&result);
    if common::backend_unavailable(&text) {
        eprintln!("r_protocol backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;

    assert!(
        text.contains("2"),
        "expected custom-launched R worker to evaluate through generic protocol, got: {text:?}"
    );
    assert!(
        text.contains(">"),
        "expected worker-supplied R prompt in response, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn custom_r_worker_spec_handles_readline_follow_up() -> TestResult<()> {
    let _guard = test_mutex()
        .lock()
        .map_err(|_| "r_protocol test mutex poisoned")?;
    let session = spawn_custom_r_worker_server().await?;

    let waiting = session
        .write_stdin_raw_unterminated_with("value <- readline('FIRST> ')", Some(2.0))
        .await?;
    let waiting_text = common::result_text(&waiting);
    if common::backend_unavailable(&waiting_text) {
        eprintln!("r_protocol backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        waiting_text.contains("FIRST> "),
        "expected R readline prompt, got: {waiting_text:?}"
    );

    let answered = session
        .write_stdin_raw_unterminated_with("alpha", Some(2.0))
        .await?;
    let answered_text = common::result_text(&answered);
    assert!(
        answered_text.contains(">"),
        "expected R to return to top-level prompt after readline input, got: {answered_text:?}"
    );

    let printed = session
        .write_stdin_raw_unterminated_with("cat(value, '\\n')", Some(2.0))
        .await?;
    let printed_text = common::result_text(&printed);
    session.cancel().await?;

    assert!(
        printed_text.contains("alpha"),
        "expected readline follow-up input to reach R, got: {printed_text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn custom_r_worker_spec_discards_buffered_input_on_interrupt() -> TestResult<()> {
    let _guard = test_mutex()
        .lock()
        .map_err(|_| "r_protocol test mutex poisoned")?;
    let mut session = spawn_custom_r_worker_server().await?;

    let first = session
        .write_stdin_raw_unterminated_with("Sys.sleep(5)\ncat('SHOULD_NOT_RUN\\n')", Some(0.1))
        .await?;
    let first_text = common::result_text(&first);
    if common::backend_unavailable(&first_text) {
        eprintln!("r_protocol backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        common::is_busy_response(&first_text),
        "expected long R request to time out, got: {first_text:?}"
    );

    let interrupted = session
        .write_stdin_raw_unterminated_with("\u{3}cat('AFTER_INTERRUPT\\n')", Some(10.0))
        .await?;
    let interrupted = common::wait_until_not_busy(
        &mut session,
        interrupted,
        std::time::Duration::from_millis(50),
        std::time::Duration::from_secs(10),
    )
    .await?;
    let interrupted_text = common::result_text(&interrupted);
    session.cancel().await?;

    assert!(
        interrupted_text.contains("AFTER_INTERRUPT"),
        "expected interrupt tail to run after R recovery, got: {interrupted_text:?}"
    );
    assert!(
        !interrupted_text.contains("SHOULD_NOT_RUN"),
        "expected buffered pre-interrupt input to be discarded, got: {interrupted_text:?}"
    );
    Ok(())
}
