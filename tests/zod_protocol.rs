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

fn result_text_items(result: &rmcp::model::CallToolResult) -> Vec<String> {
    result
        .content
        .iter()
        .filter_map(|item| match &item.raw {
            RawContent::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect()
}

fn result_image_count(result: &rmcp::model::CallToolResult) -> usize {
    result
        .content
        .iter()
        .filter(|item| matches!(item.raw, RawContent::Image(_)))
        .count()
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

async fn spawn_zod_server_with_env(
    env_vars: Vec<(&str, &str)>,
) -> TestResult<common::McpTestSession> {
    spawn_zod_server_with_stdin_env_and_extra_args("pipe", env_vars, Vec::new()).await
}

async fn spawn_zod_server_with_env_and_extra_args(
    env_vars: Vec<(&str, &str)>,
    extra_args: Vec<String>,
) -> TestResult<common::McpTestSession> {
    spawn_zod_server_with_stdin_env_and_extra_args("pipe", env_vars, extra_args).await
}

async fn spawn_zod_server_with_stdin_env_and_extra_args(
    stdin: &str,
    env_vars: Vec<(&str, &str)>,
    extra_args: Vec<String>,
) -> TestResult<common::McpTestSession> {
    let tempdir = tempfile::tempdir()?;
    let spec_path = tempdir.path().join("zod-worker.json");
    let env = env_vars
        .into_iter()
        .map(|(key, value)| (key.to_string(), Value::String(value.to_string())))
        .collect::<Map<String, Value>>();
    let spec = json!({
        "executable": zod_worker_path()?,
        "args": [],
        "working_dir": "inherit",
        "env": env,
        "stdin": stdin,
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

async fn spawn_zod_server() -> TestResult<common::McpTestSession> {
    spawn_zod_server_with_env(Vec::new()).await
}

async fn spawn_zod_v3_server(control_log: &std::path::Path) -> TestResult<common::McpTestSession> {
    spawn_zod_v3_server_with_extra_args(control_log, Vec::new()).await
}

async fn spawn_zod_v3_server_with_extra_args(
    control_log: &std::path::Path,
    extra_args: Vec<String>,
) -> TestResult<common::McpTestSession> {
    let control_log_text = control_log.display().to_string();
    spawn_zod_server_with_env_and_extra_args(
        vec![
            ("MCP_REPL_ZOD_PROTOCOL_VERSION", "3"),
            ("MCP_REPL_ZOD_CONTROL_LOG", control_log_text.as_str()),
        ],
        extra_args,
    )
    .await
}

async fn spawn_zod_v3_stalled_control_server(
    control_log: &std::path::Path,
) -> TestResult<common::McpTestSession> {
    let control_log_text = control_log.display().to_string();
    spawn_zod_server_with_env_and_extra_args(
        vec![
            ("MCP_REPL_ZOD_PROTOCOL_VERSION", "3"),
            ("MCP_REPL_ZOD_CONTROL_LOG", control_log_text.as_str()),
            ("MCP_REPL_ZOD_STALL_CONTROL_READER", "1"),
        ],
        Vec::new(),
    )
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
async fn zod_worker_echoes_input_and_returns_worker_prompt() -> TestResult<()> {
    let session = spawn_zod_server().await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "hello zod",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);

    assert!(
        text.contains("hello zod\n"),
        "expected Zod to receive server-normalized stdin, got: {text:?}"
    );
    assert!(
        text.contains("zod> "),
        "expected worker-supplied prompt in response, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_v3_receives_turn_start_without_raw_stdin() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_v3_server(&control_log).await?;

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
    let session = spawn_zod_v3_stalled_control_server(&control_log).await?;
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
    let session = spawn_zod_v3_server(&control_log).await?;

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
    let session = spawn_zod_v3_server(&control_log).await?;

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
    let session = spawn_zod_v3_server(&control_log).await?;

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
    let session = spawn_zod_v3_server(&control_log).await?;

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
    let session = spawn_zod_v3_server(&control_log).await?;

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
    assert!(
        !interrupted_text.contains("cannot interrupt v3 worker without active turn"),
        "idle Ctrl-C must not require an active v3 turn, got: {interrupted_text:?}"
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
async fn zod_worker_v3_interrupt_after_background_settle_has_no_stale_turn_id() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_v3_server(&control_log).await?;

    let first = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "sleep 50",
                "timeout_ms": 10
            }),
        )
        .await?;
    let first_text = result_text(&first);
    assert!(
        first_text.contains("<<repl status: busy"),
        "expected initial timeout, got: {first_text:?}"
    );
    wait_for_log_contains(&control_log, "idle turn_id=1")?;

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
        "Ctrl-C after background settle must not use stale active-turn state, got: {interrupted_text:?}"
    );

    let log = read_optional(&control_log);
    assert!(
        !log.contains("interrupt turn_id=1"),
        "Ctrl-C after background settle must not send a stale turn-bound interrupt, got log: {log:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_v3_input_line_after_idle_is_protocol_error() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_v3_server(&control_log).await?;

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
    if first_text.contains("input_line") {
        assert!(
            first_text.contains("arrived after idle") || first_text.contains("with no active turn"),
            "expected late input_line to fail closed as protocol error, got: {first_text:?}"
        );
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        first_text.contains("v3> "),
        "expected first turn to complete before reporting late input_line, got: {first_text:?}"
    );

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
        second_text.contains("input_line turn_id 1 arrived after idle")
            || second_text.contains("input_line reported turn_id 1 with no active turn"),
        "expected late input_line to fail closed as protocol error, got: {second_text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_v3_session_end_after_idle_is_protocol_error() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_v3_server(&control_log).await?;

    let first = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "session-end-after-idle",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let first_text = result_text(&first);
    wait_for_log_contains(&control_log, "late_session_end turn_id=1")?;
    if first_text.contains("session_end") {
        assert!(
            first_text.contains("arrived after idle") || first_text.contains("with no active turn"),
            "expected late session_end to fail closed as protocol error, got: {first_text:?}"
        );
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        first_text.contains("v3> "),
        "expected first turn to complete before reporting late session_end, got: {first_text:?}"
    );

    let second = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "after late session end",
                "timeout_ms": 100
            }),
        )
        .await?;
    let second_text = result_text(&second);
    assert!(
        second_text.contains("session_end turn_id 1 arrived after idle")
            || second_text.contains("session_end reported turn_id 1 with no active turn"),
        "expected late session_end to fail closed as protocol error, got: {second_text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_v3_latched_protocol_error_blocks_next_turn_start() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let debug_dir = tempdir.path().join("debug");
    let session = spawn_zod_v3_server_with_extra_args(
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

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_v3_protocol_error_after_timeout_blocks_next_turn_start() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let debug_dir = tempdir.path().join("debug");
    let session = spawn_zod_v3_server_with_extra_args(
        &control_log,
        vec!["--debug-dir".to_string(), debug_dir.display().to_string()],
    )
    .await?;

    let first = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "bad-output-after-sleep 80",
                "timeout_ms": 10
            }),
        )
        .await?;
    let first_text = result_text(&first);
    assert!(
        first_text.contains("<<repl status: busy"),
        "expected initial timeout busy status, got: {first_text:?}"
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
                "input": "must not reach timed-out v3 worker",
                "timeout_ms": 100
            }),
        )
        .await?;
    let second_text = result_text(&second);
    assert!(
        second_text.contains("invalid output_text base64"),
        "expected delayed protocol error on follow-up, got: {second_text:?}"
    );

    let log = read_optional(&control_log);
    assert!(
        !log.contains("must not reach timed-out v3 worker"),
        "delayed protocol error must prevent the next turn_start, got log: {log:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_raw_line_escape_preserves_stdin_bytes() -> TestResult<()> {
    let session = spawn_zod_server().await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "raw-line-escape crlf\r\nraw-line-escape bare\rcoda",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);

    assert!(
        text.contains("raw-line[1]=raw-line-escape crlf\\r\\n\n"),
        "expected Zod to receive existing CRLF bytes, got: {text:?}"
    );
    assert!(
        text.contains("raw-line[2]=raw-line-escape bare\\rcoda\\n\n"),
        "expected Zod to receive bare CR plus one appended LF, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_restart_control_prefix_preserves_newline_tail() -> TestResult<()> {
    let session = spawn_zod_server().await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "\u{4}\nraw-line-escape after",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);

    let poll = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let combined_text = format!("{text}{}", result_text(&poll));

    assert!(
        combined_text
            .contains("[repl] new session started\nraw-line[2]=raw-line-escape after\\n\n"),
        "expected Ctrl-D tail to preserve the immediate newline before follow-up input, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_pipe_launch_records_transport_and_starts_sideband() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let debug_dir = tempdir.path().join("debug");
    let session = spawn_zod_server_with_env_and_extra_args(
        Vec::new(),
        vec!["--debug-dir".to_string(), debug_dir.display().to_string()],
    )
    .await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "transport check",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);

    assert!(
        text.contains("zod> "),
        "expected worker_ready sideband startup to seed the worker prompt, got: {text:?}"
    );

    let events = latest_debug_events(&debug_dir)?;
    let spawn_begin = events
        .iter()
        .find(|entry| entry["event"] == "worker_spawn_begin")
        .ok_or_else(|| "missing worker_spawn_begin event".to_string())?;
    assert_eq!(spawn_begin["payload"]["stdin_transport"], "pipe");

    session.cancel().await?;
    Ok(())
}

#[cfg(target_family = "unix")]
#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_pty_launch_keeps_sideband_separate_and_captures_visible_output()
-> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let debug_dir = tempdir.path().join("debug");
    let session = spawn_zod_server_with_stdin_env_and_extra_args(
        "pty",
        Vec::new(),
        vec!["--debug-dir".to_string(), debug_dir.display().to_string()],
    )
    .await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "raw-prompt-then-sleep 0",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);

    assert!(
        text.contains("raw-prompt-then-sleep 0\r\n"),
        "expected PTY echo with CRLF translation, got: {text:?}"
    );
    assert!(
        text.contains("zod> raw stdout\r\n"),
        "expected visible stdout from the PTY master, got: {text:?}"
    );
    assert!(
        text.contains("zod> "),
        "expected worker_ready/readline sideband prompt to complete the turn, got: {text:?}"
    );

    let events = latest_debug_events(&debug_dir)?;
    let spawn_begin = events
        .iter()
        .find(|entry| entry["event"] == "worker_spawn_begin")
        .ok_or_else(|| "missing worker_spawn_begin event".to_string())?;
    assert_eq!(spawn_begin["payload"]["stdin_transport"], "pty");

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_preserves_existing_trailing_newline() -> TestResult<()> {
    let session = spawn_zod_server().await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "already newline\n",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);

    assert!(
        text.contains("already newline\n"),
        "expected Zod to receive existing newline, got: {text:?}"
    );
    assert!(
        !text.contains("already newline\n\n"),
        "server must not append a second trailing newline, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_reset_requests_shutdown_by_closing_stdin_only() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let shutdown_log = tempdir.path().join("shutdown.log");
    let shutdown_log_text = shutdown_log.display().to_string();
    let session = spawn_zod_server_with_env(vec![(
        "MCP_REPL_ZOD_SHUTDOWN_LOG",
        shutdown_log_text.as_str(),
    )])
    .await?;

    let first = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "before reset",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let first_text = result_text(&first);
    assert!(
        first_text.contains("before reset\n"),
        "expected Zod worker to start before reset, got: {first_text:?}"
    );

    let reset = session.call_tool_raw("repl_reset", json!({})).await?;
    let reset_text = result_text(&reset);
    assert!(
        reset_text.contains("new session started"),
        "expected reset to start a replacement session, got: {reset_text:?}"
    );

    session.cancel().await?;
    let log = fs::read_to_string(&shutdown_log)?;
    assert!(
        log.contains("stdin_eof"),
        "expected reset to close worker stdin, got log: {log:?}"
    );
    assert!(
        !log.contains("control_session_end"),
        "reset must not send a sideband shutdown command, got log: {log:?}"
    );
    assert!(
        !log.contains("sideband_shutdown"),
        "shutdown reason must come from stdin EOF, got log: {log:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_preserves_client_stdin_bytes_and_appended_newline() -> TestResult<()> {
    let session = spawn_zod_server().await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "report-raw-line supplied crlf\r\nreport-raw-line trailing carriage\r",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);

    assert!(
        text.contains("raw-line-debug: report-raw-line supplied crlf\\r\\n\n"),
        "expected client-supplied newline bytes to reach Zod unchanged, got: {text:?}"
    );
    assert!(
        text.contains("raw-line-debug: report-raw-line trailing carriage\\r\\n\n"),
        "expected server to append one final newline after trailing carriage return, got: {text:?}"
    );
    assert!(
        !text.contains("raw-line-debug: report-raw-line trailing carriage\\r\\n\\n\n"),
        "server must not append more than one newline, got: {text:?}"
    );
    assert!(
        text.contains("zod> "),
        "expected worker prompt after CRLF input, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_preserves_prompt_shaped_stdout() -> TestResult<()> {
    let session = spawn_zod_server().await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": ">>> ",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);

    assert!(
        text.contains(">>> \n"),
        "expected prompt-shaped stdout to be preserved, got: {text:?}"
    );
    assert!(
        text.contains("zod> "),
        "expected worker-supplied prompt in response, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_raw_prompt_shaped_stdout_does_not_complete_turn() -> TestResult<()> {
    let session = spawn_zod_server().await?;

    let first = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "raw-prompt-then-sleep 150",
                "timeout_ms": 10
            }),
        )
        .await?;
    let first_text = result_text(&first);
    assert!(
        first_text.contains("zod> raw stdout\n"),
        "expected raw prompt-shaped stdout to remain visible, got: {first_text:?}"
    );
    assert!(
        first_text.contains("<<repl status: busy"),
        "raw prompt-shaped stdout must not complete the turn, got: {first_text:?}"
    );

    let poll = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let poll_text = result_text(&poll);
    assert!(
        poll_text.contains("zod> "),
        "expected later poll to observe sideband prompt, got: {poll_text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_empty_prompt_uses_generic_wait_status() -> TestResult<()> {
    let session = spawn_zod_server().await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "wait ",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);

    assert!(
        text.contains("<<repl status: waiting for stdin>>"),
        "expected generic wait status for empty worker prompt, got: {text:?}"
    );
    assert!(
        !text.contains("zod> "),
        "did not expect fabricated Zod prompt for empty worker prompt, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_timeout_poll_waits_for_unsatisfied_prompt() -> TestResult<()> {
    let session = spawn_zod_server().await?;

    let first = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "sleep 150",
                "timeout_ms": 10
            }),
        )
        .await?;
    let first_text = result_text(&first);
    assert!(
        first_text.contains("<<repl status: busy"),
        "expected timeout busy status, got: {first_text:?}"
    );

    let poll = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let poll_text = result_text(&poll);
    assert!(
        poll_text.contains("zod> "),
        "expected later poll to observe worker prompt, got: {poll_text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_buffered_prompt_does_not_complete_turn() -> TestResult<()> {
    let session = spawn_zod_server().await?;

    let first = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "prompt-then-sleep 150\nbuffered input",
                "timeout_ms": 10
            }),
        )
        .await?;
    let first_text = result_text(&first);
    assert!(
        first_text.contains("<<repl status: busy"),
        "prompt with remaining active stdin must not complete the turn, got: {first_text:?}"
    );

    let poll = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let poll_text = result_text(&poll);
    assert!(
        poll_text.contains("buffered input\n") && poll_text.contains("zod> "),
        "expected poll to complete after buffered input was accounted for, got: {poll_text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_busy_follow_up_does_not_reach_stdin() -> TestResult<()> {
    let session = spawn_zod_server().await?;

    let first = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "sleep 150",
                "timeout_ms": 10
            }),
        )
        .await?;
    let first_text = result_text(&first);
    assert!(
        first_text.contains("<<repl status: busy"),
        "expected timeout busy status, got: {first_text:?}"
    );

    let busy = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "second input",
                "timeout_ms": 10
            }),
        )
        .await?;
    let busy_text = result_text(&busy);
    assert!(
        busy_text.contains("busy") || busy_text.contains("discarded"),
        "expected busy follow-up response, got: {busy_text:?}"
    );

    let poll = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let poll_text = result_text(&poll);
    assert!(
        !poll_text.contains("second input"),
        "busy follow-up should not have reached Zod stdin, got: {poll_text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_idle_protocol_error_is_latched_for_next_request() -> TestResult<()> {
    let session = spawn_zod_server().await?;

    let first = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "timeline after-readline-start delay-ms 2000 raw-output-text-invalid-base64",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let first_text = result_text(&first);
    assert!(
        first_text.contains("zod> "),
        "expected first request to finish before delayed protocol error, got: {first_text:?}"
    );

    tokio::time::sleep(std::time::Duration::from_millis(2_500)).await;

    let second = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "after idle protocol error",
                "timeout_ms": 100
            }),
        )
        .await?;
    let second_text = result_text(&second);
    assert!(
        second_text.contains("invalid output_text base64"),
        "expected latched protocol error on next request, got: {second_text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_invalid_output_base64_is_protocol_error() -> TestResult<()> {
    let session = spawn_zod_server().await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "bad-output-base64",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);
    assert!(
        text.contains("invalid output_text base64"),
        "expected invalid base64 protocol error, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_startup_protocol_error_after_ready_is_reported() -> TestResult<()> {
    let session =
        spawn_zod_server_with_env(vec![("MCP_REPL_ZOD_STARTUP_PROTOCOL_ERROR", "1")]).await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "after bad startup",
                "timeout_ms": 100
            }),
        )
        .await?;
    let text = result_text(&result);
    assert!(
        text.contains("invalid output_text base64"),
        "expected startup protocol error to be reported, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_protocol_error_after_timeout_is_reported_on_follow_up() -> TestResult<()> {
    let session = spawn_zod_server().await?;

    let first = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "bad-output-after-sleep 80",
                "timeout_ms": 10
            }),
        )
        .await?;
    let first_text = result_text(&first);
    assert!(
        first_text.contains("<<repl status: busy"),
        "expected initial timeout busy status, got: {first_text:?}"
    );

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    let second = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "after delayed protocol error",
                "timeout_ms": 100
            }),
        )
        .await?;
    let second_text = result_text(&second);
    assert!(
        second_text.contains("invalid output_text base64"),
        "expected delayed protocol error on follow-up, got: {second_text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_readline_input_mismatch_is_protocol_error() -> TestResult<()> {
    let session = spawn_zod_server().await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "misreport-input different",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);
    assert!(
        text.contains("readline_input_bytes bytes does not match active stdin"),
        "expected readline_input_bytes accounting protocol error, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_split_utf8_byte_accounting_completes_request() -> TestResult<()> {
    let session = spawn_zod_server().await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "read-split-utf8-tail\né\n",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);
    assert!(
        text.contains("split-tail bytes: [195, 169]"),
        "expected split UTF-8 tail bytes to be accounted, got: {text:?}"
    );
    assert!(
        !text.contains("<<repl status: busy") && !text.contains("worker protocol error"),
        "split UTF-8 byte accounting should complete cleanly, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_old_readline_input_frame_is_protocol_error() -> TestResult<()> {
    let session = spawn_zod_server().await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "old-readline-input",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);
    assert!(
        text.contains("invalid worker sideband JSON") && text.contains("readline_input"),
        "expected old readline_input frame to be rejected, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_old_readline_discard_frame_is_protocol_error() -> TestResult<()> {
    let session = spawn_zod_server().await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "old-readline-discard",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);
    assert!(
        text.contains("invalid worker sideband JSON") && text.contains("readline_discard"),
        "expected old readline_discard frame to be rejected, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_output_after_session_end_is_protocol_error() -> TestResult<()> {
    let session = spawn_zod_server().await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "bad-output-after-session-end",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);
    assert!(
        text.contains("worker sideband message after session_end"),
        "expected output-after-session-end protocol error, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_invalid_session_end_reason_is_protocol_error() -> TestResult<()> {
    let session = spawn_zod_server().await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "bad-session-end-reason",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);
    assert!(
        text.contains("invalid session_end reason"),
        "expected invalid session_end reason protocol error, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_repl_reset_closes_active_stdin_without_shutdown_text() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let shutdown_log = tempdir.path().join("shutdown.log");
    let shutdown_log_env = shutdown_log.display().to_string();
    let session = spawn_zod_server_with_env(vec![(
        "MCP_REPL_ZOD_SHUTDOWN_LOG",
        shutdown_log_env.as_str(),
    )])
    .await?;

    let active = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "read-user-stdin",
                "timeout_ms": 10
            }),
        )
        .await?;
    let active_text = result_text(&active);
    assert!(
        active_text.contains("<<repl status: busy"),
        "expected active stdin read to time out before reset, got: {active_text:?}"
    );

    let reset = session.call_tool_raw("repl_reset", json!({})).await?;
    let reset_text = result_text(&reset);
    assert!(
        reset_text.contains("new session started"),
        "expected repl_reset to respawn the Zod worker, got: {reset_text:?}"
    );

    let shutdown_log_text = fs::read_to_string(&shutdown_log).unwrap_or_default();
    assert!(
        !shutdown_log_text.contains("user-stdin:exit\n"),
        "repl_reset must not send shutdown text to an active request, got: {shutdown_log_text:?}"
    );
    assert!(
        shutdown_log_text.contains("user-stdin:<eof>\n"),
        "expected reset to close active stdin instead, got: {shutdown_log_text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_repl_reset_can_exercise_slow_graceful_shutdown() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let shutdown_log = tempdir.path().join("shutdown.log");
    let shutdown_log_env = shutdown_log.display().to_string();
    let session = spawn_zod_server_with_env(vec![(
        "MCP_REPL_ZOD_SHUTDOWN_LOG",
        shutdown_log_env.as_str(),
    )])
    .await?;

    let configured = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "slow-shutdown 25",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let configured_text = result_text(&configured);
    assert!(
        configured_text.contains("zod> "),
        "expected Zod to accept slow shutdown hook, got: {configured_text:?}"
    );

    let reset = session.call_tool_raw("repl_reset", json!({})).await?;
    let reset_text = result_text(&reset);
    assert!(
        reset_text.contains("new session started"),
        "expected repl_reset to respawn after slow shutdown, got: {reset_text:?}"
    );

    let shutdown_log_text = fs::read_to_string(&shutdown_log).unwrap_or_default();
    assert!(
        shutdown_log_text.contains("stdin_eof\n"),
        "expected repl_reset to close worker stdin, got: {shutdown_log_text:?}"
    );
    assert!(
        !shutdown_log_text.contains("user-stdin:exit\n"),
        "reset must not send shutdown text to stdin, got: {shutdown_log_text:?}"
    );
    assert!(
        shutdown_log_text.contains("shutdown:delay-ms:25\n"),
        "expected Zod to record the slow shutdown hook, got: {shutdown_log_text:?}"
    );
    assert!(
        shutdown_log_text.contains("shutdown:delay-complete\n"),
        "expected Zod to complete the slow shutdown hook, got: {shutdown_log_text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_can_hold_shutdown_open_for_escalation_tests() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let shutdown_log = tempdir.path().join("shutdown.log");
    let shutdown_log_env = shutdown_log.display().to_string();
    let session = spawn_zod_server_with_env(vec![(
        "MCP_REPL_ZOD_SHUTDOWN_LOG",
        shutdown_log_env.as_str(),
    )])
    .await?;

    let configured = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "hang-shutdown",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let configured_text = result_text(&configured);
    assert!(
        configured_text.contains("zod> "),
        "expected Zod to accept hanging shutdown hook, got: {configured_text:?}"
    );

    let exiting = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "exit",
                "timeout_ms": 100
            }),
        )
        .await?;
    let exiting_text = result_text(&exiting);
    assert!(
        exiting_text.contains("<<repl status: busy"),
        "expected hanging shutdown hook to leave exit pending, got: {exiting_text:?}"
    );

    let shutdown_log_text = fs::read_to_string(&shutdown_log).unwrap_or_default();
    assert!(
        shutdown_log_text.contains("user-stdin:exit\n"),
        "expected Zod to receive exit before hanging, got: {shutdown_log_text:?}"
    );
    assert!(
        shutdown_log_text.contains("shutdown:hang\n"),
        "expected Zod to record the hanging shutdown hook, got: {shutdown_log_text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_preserves_mixed_output_order() -> TestResult<()> {
    let session = spawn_zod_server().await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "mixed-output",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let items = result_text_items(&result);
    let joined = items.join("");
    let before = joined.find("stdout-before\n");
    let middle = joined.find("stderr-middle\n");
    let after = joined.find("stdout-after\n");
    assert!(
        matches!((before, middle, after), (Some(before), Some(middle), Some(after)) if before < middle && middle < after),
        "expected mixed output in sideband order, got: {items:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_emits_image_output() -> TestResult<()> {
    let session = spawn_zod_server().await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "image",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    assert_eq!(
        result_image_count(&result),
        1,
        "expected one Zod image, got content: {:?}",
        result.content
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_interrupt_tail_runs_after_recovery() -> TestResult<()> {
    let session = spawn_zod_server().await?;

    let first = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "interruptible 1000",
                "timeout_ms": 10
            }),
        )
        .await?;
    let first_text = result_text(&first);
    assert!(
        first_text.contains("<<repl status: busy"),
        "expected timeout busy status, got: {first_text:?}"
    );

    let interrupted = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "\u{3}tail after interrupt",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&interrupted);
    assert!(
        text.contains("tail after interrupt\n"),
        "expected interrupt tail to run after recovery, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_control_prefix_preserves_immediate_newline_tail() -> TestResult<()> {
    let session = spawn_zod_server().await?;

    let first = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "interruptible 1000",
                "timeout_ms": 10
            }),
        )
        .await?;
    let first_text = result_text(&first);
    assert!(
        first_text.contains("<<repl status: busy"),
        "expected timeout busy status, got: {first_text:?}"
    );

    let interrupted = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "\u{3}\nreport-leading-empty",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&interrupted);
    assert!(
        text.contains("previous empty line: observed\n"),
        "expected Zod to receive the immediate newline before the tail, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[cfg(target_family = "unix")]
#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_reports_sideband_and_os_interrupt_facts() -> TestResult<()> {
    let session = spawn_zod_server().await?;

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
        "expected timeout busy status, got: {first_text:?}"
    );

    let interrupted = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "\u{3}tail after interrupt facts",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&interrupted);
    assert!(
        text.contains("sideband interrupt: observed\n"),
        "expected Zod to report the sideband interrupt notification, got: {text:?}"
    );
    assert!(
        text.contains("os interrupt: observed\n"),
        "expected Zod to report the OS interrupt, got: {text:?}"
    );
    assert!(
        text.contains("tail after interrupt facts\n"),
        "expected interrupt tail to run after recovery, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_interrupt_discards_buffered_tail_before_follow_up() -> TestResult<()> {
    let session = spawn_zod_server().await?;

    let first = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "discard-on-interrupt 1000\nSHOULD_NOT_RUN",
                "timeout_ms": 10
            }),
        )
        .await?;
    let first_text = result_text(&first);
    assert!(
        first_text.contains("<<repl status: busy"),
        "expected timeout busy status, got: {first_text:?}"
    );

    let interrupted = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "\u{3}after discard",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&interrupted);
    assert!(
        text.contains("after discard\n"),
        "expected follow-up tail to run after recovery, got: {text:?}"
    );
    assert!(
        !text.contains("SHOULD_NOT_RUN"),
        "expected buffered pre-interrupt tail to be discarded, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_interrupt_discards_split_utf8_buffer_bytes() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let debug_dir = tempdir.path().join("debug");
    let session = spawn_zod_server_with_env_and_extra_args(
        Vec::new(),
        vec!["--debug-dir".to_string(), debug_dir.display().to_string()],
    )
    .await?;

    let first = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "read-one-byte-then-discard-on-interrupt 1000\né",
                "timeout_ms": 10
            }),
        )
        .await?;
    let first_text = result_text(&first);
    assert!(
        first_text.contains("<<repl status: busy"),
        "expected timeout busy status, got: {first_text:?}"
    );

    let interrupted = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "\u{3}after split discard",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&interrupted);
    assert!(
        text.contains("after split discard\n"),
        "expected follow-up tail to run after split UTF-8 discard, got: {text:?}"
    );
    assert!(
        !text.contains("worker protocol error"),
        "split UTF-8 discard should account raw bytes, got: {text:?}"
    );
    assert!(
        !text.contains("new session started"),
        "split UTF-8 discard should not restart the worker, got: {text:?}"
    );

    let spawn_count = latest_debug_events(&debug_dir)?
        .iter()
        .filter(|entry| entry["event"] == "worker_spawn_begin")
        .count();
    assert_eq!(
        spawn_count, 1,
        "split UTF-8 discard should not replace the worker"
    );

    session.cancel().await?;
    Ok(())
}
