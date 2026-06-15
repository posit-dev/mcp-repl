mod common;

use common::TestResult;
use rmcp::model::RawContent;
use serde_json::{Map, Value, json};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

static ZOD_WORKER_PATH: OnceLock<Result<PathBuf, String>> = OnceLock::new();

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

fn read_optional(path: &std::path::Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

fn wait_for_log_contains(path: &std::path::Path, needle: &str) -> TestResult<String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let text = read_optional(path);
        if text.contains(needle) {
            return Ok(text);
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!("expected {needle:?} in {}, got {text:?}", path.display()).into());
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn wait_for_debug_event_message_contains(
    debug_dir: &std::path::Path,
    event: &str,
    needle: &str,
) -> TestResult<Value> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut latest = Vec::new();
    loop {
        if let Ok(events) = latest_debug_events(debug_dir) {
            latest = events;
            if let Some(entry) = latest.iter().find(|entry| {
                entry["event"] == event
                    && entry["payload"]["message"]
                        .as_str()
                        .is_some_and(|message| message.contains(needle))
            }) {
                return Ok(entry.clone());
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "expected {event:?} containing {needle:?} in {}, got {latest:?}",
                debug_dir.display()
            )
            .into());
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn zod_worker_path() -> TestResult<PathBuf> {
    match ZOD_WORKER_PATH.get_or_init(build_zod_worker) {
        Ok(path) => Ok(path.clone()),
        Err(err) => Err(err.clone().into()),
    }
}

fn build_zod_worker() -> Result<PathBuf, String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let output = Command::new(cargo)
        .arg("build")
        .arg("--example")
        .arg("zod-worker")
        .arg("--manifest-path")
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
        .output()
        .map_err(|err| format!("failed to run cargo build --example zod-worker: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo build --example zod-worker failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let mut target_dir = std::env::current_exe()
        .map_err(|err| format!("failed to resolve current test executable: {err}"))?;
    target_dir.pop();
    target_dir.pop();
    let exe_name = if cfg!(windows) {
        "zod-worker.exe"
    } else {
        "zod-worker"
    };
    let path = target_dir.join("examples").join(exe_name);
    if path.exists() {
        return Ok(path);
    }

    Err(format!(
        "unable to locate zod-worker test example at {}",
        path.display()
    ))
}

async fn spawn_zod_server_with_extra_args(
    control_log: &std::path::Path,
    extra_args: Vec<String>,
) -> TestResult<common::McpTestSession> {
    let tempdir = tempfile::tempdir()?;
    let spec_path = tempdir.path().join("zod-worker.json");
    let mut env = Map::new();
    env.insert(
        "MCP_REPL_ZOD_CONTROL_LOG".to_string(),
        Value::String(control_log.display().to_string()),
    );
    let spec = json!({
        "executable": zod_worker_path()?,
        "args": [],
        "working_dir": "inherit",
        "env": env,
        "stdin": "pipe",
        "sandbox": "server"
    });
    std::fs::write(&spec_path, serde_json::to_vec_pretty(&spec)?)?;
    let mut args = vec![
        "--worker-spec".to_string(),
        spec_path.display().to_string(),
        "--sandbox".to_string(),
        "danger-full-access".to_string(),
        "--oversized-output".to_string(),
        "files".to_string(),
    ];
    args.extend(extra_args);
    common::spawn_server_with_args(args).await
}

async fn spawn_zod_server(control_log: &std::path::Path) -> TestResult<common::McpTestSession> {
    spawn_zod_server_with_extra_args(control_log, Vec::new()).await
}

async fn spawn_zod_stalled_control_server(
    control_log: &std::path::Path,
) -> TestResult<common::McpTestSession> {
    let tempdir = tempfile::tempdir()?;
    let spec_path = tempdir.path().join("zod-worker.json");
    let mut env = Map::new();
    env.insert(
        "MCP_REPL_ZOD_CONTROL_LOG".to_string(),
        Value::String(control_log.display().to_string()),
    );
    env.insert(
        "MCP_REPL_ZOD_STALL_CONTROL_READER".to_string(),
        Value::String("1".to_string()),
    );
    let spec = json!({
        "executable": zod_worker_path()?,
        "args": [],
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

fn latest_debug_events(debug_dir: &std::path::Path) -> TestResult<Vec<Value>> {
    let mut sessions = fs::read_dir(debug_dir)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    sessions.sort();
    let session_dir = sessions
        .last()
        .cloned()
        .ok_or_else(|| "missing debug session directory".to_string())?;
    let log_text = fs::read_to_string(session_dir.join("events.jsonl"))?;
    Ok(log_text
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()?)
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_v3_receives_turn_start_without_raw_stdin() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_server(&control_log).await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "hello v3",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);

    assert!(
        text.contains("v3-output: hello v3\n"),
        "expected v3 worker to receive input through turn_start, got: {text:?}"
    );
    assert!(
        !text.contains("v3> hello v3"),
        "default reply must elide synthetic input_line echo, got: {text:?}"
    );

    let log = wait_for_log_contains(&control_log, "turn_start turn_id=1 input=hello v3")?;
    assert!(
        !log.contains("stdin:"),
        "v3 server path must not write request text to raw stdin, got log: {log:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_v3_turn_start_write_respects_timeout_when_control_reader_stalls()
-> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_stalled_control_server(&control_log).await?;
    let input = "x".repeat(2 * 1024 * 1024);

    let result = tokio::time::timeout(
        Duration::from_secs(3),
        session.call_tool_raw(
            "repl",
            json!({
                "input": input,
                "timeout_ms": 100
            }),
        ),
    )
    .await;
    let result = match result {
        Ok(result) => result?,
        Err(_) => {
            session.cancel().await?;
            panic!("v3 turn_start write did not respect timeout_ms");
        }
    };
    let text = result_text(&result);
    assert!(
        text.contains("worker response timed out"),
        "expected bounded turn_start write timeout, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_v3_input_line_is_ordered_before_output_text_but_elided() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_server(&control_log).await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "emit-output-after-input",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);

    assert!(
        text.contains("after input_line\n"),
        "expected output_text after input_line, got: {text:?}"
    );
    assert!(
        !text.contains("v3> emit-output-after-input"),
        "input_line is structural and should not be rendered by default, got: {text:?}"
    );

    let log = wait_for_log_contains(
        &control_log,
        "input_line turn_id=1 text=emit-output-after-input\\n",
    )?;
    let input_line = log
        .find("input_line turn_id=1")
        .ok_or_else(|| "missing input_line log".to_string())?;
    let output_text = log
        .find("output_text turn_id=1")
        .ok_or_else(|| "missing output_text log".to_string())?;
    assert!(
        input_line < output_text,
        "expected worker to emit input_line before output_text, got log: {log:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_v3_idle_turn_id_completes_without_readline_start() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_server(&control_log).await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "idle-only",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);

    assert!(
        !text.contains("<<repl status: busy"),
        "idle(turn_id) should complete the turn, got: {text:?}"
    );
    assert!(
        text.contains("v3> "),
        "expected idle prompt from v3 worker, got: {text:?}"
    );
    wait_for_log_contains(&control_log, "idle turn_id=1")?;

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_v3_busy_follow_up_does_not_send_second_turn_start() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_server(&control_log).await?;

    let first = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "interrupt-report 5000",
                "timeout_ms": 10
            }),
        )
        .await?;
    let first_text = result_text(&first);
    assert!(
        first_text.contains("<<repl status: busy"),
        "expected initial timeout, got: {first_text:?}"
    );

    let busy = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "second v3 input",
                "timeout_ms": 10
            }),
        )
        .await?;
    let busy_text = result_text(&busy);
    assert!(
        busy_text.contains("busy") || busy_text.contains("discarded"),
        "expected busy follow-up response, got: {busy_text:?}"
    );

    let interrupted = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "\u{3}",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let interrupted_text = result_text(&interrupted);
    assert!(
        interrupted_text.contains("sideband interrupt: observed"),
        "expected active turn to settle after interrupt, got: {interrupted_text:?}"
    );

    let log = wait_for_log_contains(&control_log, "idle turn_id=1")?;
    assert!(
        !log.contains("second v3 input"),
        "busy follow-up must not reach the active v3 worker, got log: {log:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_v3_interrupt_carries_active_turn_id() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_server(&control_log).await?;

    let first = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "interrupt-report 1000",
                "timeout_ms": 10
            }),
        )
        .await?;
    let first_text = result_text(&first);
    assert!(
        first_text.contains("<<repl status: busy"),
        "expected initial timeout, got: {first_text:?}"
    );

    let interrupted = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "\u{3}",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let interrupted_text = result_text(&interrupted);
    assert!(
        interrupted_text.contains("sideband interrupt: observed"),
        "expected v3 worker to observe sideband interrupt, got: {interrupted_text:?}"
    );

    wait_for_log_contains(&control_log, "interrupt turn_id=1")?;

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_v3_idle_interrupt_does_not_require_active_turn() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_server(&control_log).await?;

    let first = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "idle-only",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let first_text = result_text(&first);
    assert!(
        first_text.contains("v3> "),
        "expected v3 worker to settle before idle interrupt, got: {first_text:?}"
    );

    let interrupted = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "\u{3}",
                "timeout_ms": 100
            }),
        )
        .await?;
    let interrupted_text = result_text(&interrupted);
    assert_ne!(
        interrupted.is_error,
        Some(true),
        "idle Ctrl-C must remain a non-error control reply, got: {interrupted_text:?}"
    );

    let log = read_optional(&control_log);
    assert!(
        !log.contains("interrupt turn_id="),
        "idle Ctrl-C must not send a turn-bound sideband interrupt, got log: {log:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_v3_input_line_after_idle_is_protocol_error() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_server(&control_log).await?;

    let first = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "late-input-line-after-idle",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let first_text = result_text(&first);
    wait_for_log_contains(&control_log, "late_input_line turn_id=1")?;
    if !first_text.contains("input_line") {
        assert!(
            first_text.contains("v3> "),
            "expected first turn to complete before late input_line, got: {first_text:?}"
        );
    }

    let second = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "after late input line",
                "timeout_ms": 100
            }),
        )
        .await?;
    let second_text = result_text(&second);
    assert!(
        first_text.contains("input_line") || second_text.contains("input_line"),
        "expected late input_line to fail closed as protocol error, got first={first_text:?} second={second_text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_v3_latched_protocol_error_blocks_next_turn_start() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let debug_dir = tempdir.path().join("debug");
    let session = spawn_zod_server_with_extra_args(
        &control_log,
        vec!["--debug-dir".to_string(), debug_dir.display().to_string()],
    )
    .await?;

    let first = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "bad-output-after-idle 500",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let first_text = result_text(&first);
    assert!(
        first_text.contains("v3> "),
        "expected first turn to complete before delayed protocol error, got: {first_text:?}"
    );
    wait_for_debug_event_message_contains(
        &debug_dir,
        "worker_protocol_error_latched",
        "invalid output_text base64",
    )?;

    let second = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "must not reach v3 worker",
                "timeout_ms": 100
            }),
        )
        .await?;
    let second_text = result_text(&second);
    assert!(
        second_text.contains("invalid output_text base64"),
        "expected latched protocol error before next v3 turn, got: {second_text:?}"
    );

    let log = read_optional(&control_log);
    assert!(
        !log.contains("must not reach v3 worker"),
        "latched protocol error must prevent the next turn_start, got log: {log:?}"
    );

    session.cancel().await?;
    Ok(())
}
