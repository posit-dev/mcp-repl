mod common;

use common::TestResult;
use rmcp::model::RawContent;
use serde_json::{Map, Value, json};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

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

fn result_image_count(result: &rmcp::model::CallToolResult) -> usize {
    result
        .content
        .iter()
        .filter(|item| matches!(item.raw, RawContent::Image(_)))
        .count()
}

fn first_image_index(result: &rmcp::model::CallToolResult) -> Option<usize> {
    result
        .content
        .iter()
        .position(|item| matches!(item.raw, RawContent::Image(_)))
}

fn first_text_index_containing(
    result: &rmcp::model::CallToolResult,
    needle: &str,
) -> Option<usize> {
    result.content.iter().position(|item| match &item.raw {
        RawContent::Text(text) => text.text.contains(needle),
        _ => false,
    })
}

fn disclosed_path(text: &str, suffix: &str) -> Option<PathBuf> {
    let end = text.find(suffix)?.saturating_add(suffix.len());
    let start = text[..end]
        .rfind(|ch: char| ch.is_whitespace() || matches!(ch, '"' | '\'' | '[' | '('))
        .map_or(0, |idx| idx.saturating_add(1));
    Some(PathBuf::from(&text[start..end]))
}

fn events_log_path(text: &str) -> Option<PathBuf> {
    disclosed_path(text, "events.log")
}

fn read_optional(path: &std::path::Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

#[cfg(target_family = "unix")]
fn extract_prefixed_value(text: &str, prefix: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.strip_prefix(prefix))
        .map(str::to_string)
}

#[cfg(target_family = "unix")]
fn first_logged_pid(log: &str) -> Option<u32> {
    log.lines()
        .find_map(|line| line.strip_prefix("pid "))
        .and_then(|pid| pid.parse().ok())
}

#[cfg(target_family = "unix")]
fn process_is_running(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if result == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(target_family = "unix")]
fn wait_for_process_exit(pid: u32) -> TestResult<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if !process_is_running(pid) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    Err(format!("expected process {pid} to exit after session_end respawn").into())
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
    spawn_zod_server_with_extra_env_and_extra_args(control_log, Vec::new(), extra_args).await
}

async fn spawn_zod_server_with_extra_env_and_extra_args(
    control_log: &std::path::Path,
    extra_env: Vec<(&str, &str)>,
    extra_args: Vec<String>,
) -> TestResult<common::McpTestSession> {
    spawn_zod_server_with_extra_env_server_env_and_extra_args(
        control_log,
        extra_env,
        Vec::new(),
        extra_args,
    )
    .await
}

async fn spawn_zod_server_with_extra_env_server_env_and_extra_args(
    control_log: &std::path::Path,
    extra_env: Vec<(&str, &str)>,
    server_env: Vec<(String, String)>,
    extra_args: Vec<String>,
) -> TestResult<common::McpTestSession> {
    let tempdir = tempfile::tempdir()?;
    let spec_path = tempdir.path().join("zod-worker.json");
    let mut env = Map::new();
    env.insert(
        "MCP_REPL_ZOD_CONTROL_LOG".to_string(),
        Value::String(control_log.display().to_string()),
    );
    for (key, value) in extra_env {
        env.insert(key.to_string(), Value::String(value.to_string()));
    }
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
    common::spawn_server_with_args_env(args, server_env).await
}

async fn spawn_zod_server(control_log: &std::path::Path) -> TestResult<common::McpTestSession> {
    spawn_zod_server_with_extra_args(control_log, Vec::new()).await
}

async fn warm_zod_session(session: &common::McpTestSession) -> TestResult<()> {
    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "emit-output-after-input",
                "timeout_ms": 5_000
            }),
        )
        .await?;
    let text = result_text(&result);
    assert!(
        text.contains("after input_line"),
        "zod worker warm-up did not complete as expected, got: {text:?}"
    );
    Ok(())
}

async fn spawn_zod_startup_ready_server(
    control_log: &std::path::Path,
) -> TestResult<common::McpTestSession> {
    spawn_zod_server_with_extra_env_and_extra_args(
        control_log,
        vec![("MCP_REPL_ZOD_STARTUP_READY", "1")],
        Vec::new(),
    )
    .await
}

async fn spawn_zod_delayed_interrupt_ready_server(
    control_log: &std::path::Path,
) -> TestResult<common::McpTestSession> {
    spawn_zod_server_with_extra_env_and_extra_args(
        control_log,
        vec![
            ("MCP_REPL_ZOD_STARTUP_READY", "1"),
            ("MCP_REPL_ZOD_DELAY_READY_AFTER_INTERRUPT_MS", "200"),
        ],
        Vec::new(),
    )
    .await
}

async fn spawn_zod_interrupt_ready_during_ack_server(
    control_log: &std::path::Path,
) -> TestResult<common::McpTestSession> {
    spawn_zod_server_with_extra_env_and_extra_args(
        control_log,
        vec![
            ("MCP_REPL_ZOD_STARTUP_READY", "1"),
            ("MCP_REPL_ZOD_DELAY_READY_AFTER_INTERRUPT_MS", "0"),
            ("MCP_REPL_ZOD_DELAY_INTERRUPT_ACK_MS", "200"),
        ],
        Vec::new(),
    )
    .await
}

async fn spawn_zod_without_interrupt_ack_server(
    control_log: &std::path::Path,
) -> TestResult<common::McpTestSession> {
    spawn_zod_server_with_extra_env_and_extra_args(
        control_log,
        vec![("MCP_REPL_ZOD_SKIP_INTERRUPT_ACK", "1")],
        Vec::new(),
    )
    .await
}

async fn spawn_zod_protocol_error_before_interrupt_ack_server(
    control_log: &std::path::Path,
) -> TestResult<common::McpTestSession> {
    spawn_zod_server_with_extra_env_and_extra_args(
        control_log,
        vec![("MCP_REPL_ZOD_INTERRUPT_PROTOCOL_ERROR_BEFORE_ACK", "1")],
        Vec::new(),
    )
    .await
}

async fn spawn_zod_preemptive_interrupt_ack_server(
    control_log: &std::path::Path,
    marker: &std::path::Path,
) -> TestResult<common::McpTestSession> {
    spawn_zod_server_with_extra_env_and_extra_args(
        control_log,
        vec![(
            "MCP_REPL_ZOD_PREEMPTIVE_INTERRUPT_ACK_MARKER",
            marker.to_str().ok_or("marker path must be valid UTF-8")?,
        )],
        Vec::new(),
    )
    .await
}

async fn spawn_zod_pager_server(
    control_log: &std::path::Path,
    page_chars: u64,
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
    common::spawn_server_with_args_env(
        vec![
            "--worker-spec".to_string(),
            spec_path.display().to_string(),
            "--sandbox".to_string(),
            "danger-full-access".to_string(),
            "--oversized-output".to_string(),
            "pager".to_string(),
        ],
        vec![(
            "MCP_REPL_PAGER_PAGE_CHARS".to_string(),
            page_chars.to_string(),
        )],
    )
    .await
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
        Value::String("5000".to_string()),
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

#[cfg(target_family = "unix")]
async fn spawn_zod_fail_once_ready_server(
    control_log: &std::path::Path,
    marker_path: &std::path::Path,
) -> TestResult<common::McpTestSession> {
    use std::os::unix::fs::PermissionsExt;

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let root = common::checkout_test_temp_parent("zod-ready-failure")?.join(nanos.to_string());
    std::fs::create_dir_all(&root)?;
    let wrapper_path = root.join("zod-fail-once.sh");
    std::fs::write(
        &wrapper_path,
        r#"#!/bin/sh
if [ ! -e "$MCP_REPL_ZOD_FAIL_ONCE_MARKER" ]; then
  printf first > "$MCP_REPL_ZOD_FAIL_ONCE_MARKER"
  exit 0
fi
exec "$MCP_REPL_ZOD_REAL_WORKER"
"#,
    )?;
    let mut permissions = std::fs::metadata(&wrapper_path)?.permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&wrapper_path, permissions)?;

    let spec_path = root.join("zod-worker.json");
    let mut env = Map::new();
    env.insert(
        "MCP_REPL_ZOD_CONTROL_LOG".to_string(),
        Value::String(control_log.display().to_string()),
    );
    env.insert(
        "MCP_REPL_ZOD_REAL_WORKER".to_string(),
        Value::String(zod_worker_path()?.display().to_string()),
    );
    env.insert(
        "MCP_REPL_ZOD_FAIL_ONCE_MARKER".to_string(),
        Value::String(marker_path.display().to_string()),
    );
    let spec = json!({
        "executable": wrapper_path,
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
async fn zod_worker_v5_receives_input_batch_without_raw_stdin() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_server(&control_log).await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "hello v5",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);

    assert!(
        text.contains("v5-output: hello v5\n"),
        "expected v5 worker to receive input through input_batch, got: {text:?}"
    );
    assert!(
        !text.contains("v5> hello v5"),
        "leading generated input_line echo should be absent, got: {text:?}"
    );
    assert!(
        text.contains("v5> "),
        "expected worker prompt after v5 output, got: {text:?}"
    );

    let log = wait_for_log_contains(&control_log, "input_batch input=hello v5")?;
    assert!(
        !log.contains("stdin:"),
        "v5 server path must not write request text to raw stdin, got log: {log:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_startup_ready_accepts_first_input_without_prompt_wait() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_startup_ready_server(&control_log).await?;

    let result = tokio::time::timeout(
        Duration::from_secs(3),
        session.call_tool_raw(
            "repl",
            json!({
                "input": "prompt-free startup",
                "timeout_ms": 20_000
            }),
        ),
    )
    .await;
    let result = match result {
        Ok(result) => result?,
        Err(_) => {
            session.cancel().await?;
            panic!(
                "startup ready should accept first input without waiting for input_wait timeout"
            );
        }
    };
    let text = result_text(&result);

    assert!(
        text.contains("v5-output: prompt-free startup\n"),
        "expected custom worker startup ready to accept first input, got: {text:?}"
    );
    let log = wait_for_log_contains(&control_log, "ready")?;
    assert!(
        log.contains("input_batch input=prompt-free startup"),
        "expected first input after startup ready, got log: {log:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_interrupt_prefix_does_not_wait_for_fresh_ready() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_delayed_interrupt_ready_server(&control_log).await?;

    let result = session
        .write_stdin_raw_with("\u{3}after interrupt settle", Some(10.0))
        .await?;
    let text = result_text(&result);
    assert!(
        text.contains("v5-output: after interrupt settle\n"),
        "expected interrupt tail to run after settle window, got: {text:?}"
    );

    let log = wait_for_log_contains(
        &control_log,
        "fresh_ready_after_interrupt_suppressed_for_pending_input",
    )?;
    let input_idx = log
        .find("input_batch input=after interrupt settle")
        .expect("interrupt tail input log should be present");
    let suppressed_idx = log
        .find("fresh_ready_after_interrupt_suppressed_for_pending_input")
        .expect("delayed readiness should be suppressed by pending input");
    assert!(
        input_idx < suppressed_idx,
        "expected tail input before delayed fresh readiness, got log: {log:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_interrupt_prefix_accepts_ready_during_ack_wait() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_interrupt_ready_during_ack_server(&control_log).await?;

    let result = session
        .write_stdin_raw_with("\u{3}after ready during ack", Some(10.0))
        .await?;
    let text = result_text(&result);
    assert!(
        text.contains("v5-output: after ready during ack\n"),
        "expected interrupt tail to run after readiness emitted during ack wait, got: {text:?}"
    );
    assert!(
        !text.contains("<<repl status: busy"),
        "interrupt tail should not time out after readiness emitted during ack wait, got: {text:?}"
    );

    let log = wait_for_log_contains(&control_log, "input_batch input=after ready during ack")?;
    let ready_idx = log
        .find("fresh_ready_after_interrupt")
        .expect("fresh readiness log should be present");
    let ack_idx = log
        .find("interrupt_ack interrupt_id=")
        .expect("interrupt ack log should be present");
    let input_idx = log
        .find("input_batch input=after ready during ack")
        .expect("interrupt tail input log should be present");
    assert!(
        ready_idx < ack_idx,
        "test must cover readiness observed during ack wait, got log: {log:?}"
    );
    assert!(
        ack_idx < input_idx,
        "expected tail input only after interrupt ack wait finished, got log: {log:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_pager_hidden_input_echoes_do_not_evict_visible_output() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_pager_server(&control_log, 4_000).await?;

    let hidden_payload = "h".repeat(300_000);
    let input = format!(
        "repeat-output 1700000\nsilent {hidden_payload}\nsilent {hidden_payload}\nsilent {hidden_payload}"
    );
    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": input,
                "timeout_ms": 30_000
            }),
        )
        .await?;
    let text = result_text(&result);

    session.cancel().await?;

    assert!(
        text.contains("ZOD_BEGIN"),
        "hidden input echoes should not evict the beginning of visible pager output, got: {text:?}"
    );
    assert!(
        text.contains("--More--"),
        "expected the anchored output to remain paged, got: {text:?}"
    );
    assert!(
        !text.contains("output gap detected") && !text.contains("output truncated"),
        "hidden input echoes should not create a visible output gap, got: {text:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_pager_leading_hidden_input_echo_does_not_consume_first_page_budget() -> TestResult<()>
{
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_pager_server(&control_log, 4_000).await?;

    let hidden_payload = "h".repeat(30_000);
    let input = format!("silent {hidden_payload}\nrepeat-output 10000");
    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": input,
                "timeout_ms": 30_000
            }),
        )
        .await?;
    let text = result_text(&result);

    session.cancel().await?;

    assert!(
        text.contains("ZOD_BEGIN"),
        "leading hidden input echo should not evict the first visible pager output, got: {text:?}"
    );
    assert!(
        text.contains("--More--"),
        "expected the visible output to remain paged, got: {text:?}"
    );
    assert!(
        !text.contains("silent "),
        "leading input echo should stay hidden in pager replies, got: {text:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_pager_refresh_keeps_later_input_echo_hidden() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_pager_server(&control_log, 4_000).await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "pager-refresh-input-echo",
                "timeout_ms": 50
            }),
        )
        .await?;
    let first_text = result_text(&result);
    assert!(
        first_text.contains("<<repl status: busy") && first_text.contains("--More--"),
        "expected timed-out request with active pager, got: {first_text:?}"
    );

    wait_for_log_contains(&control_log, "refresh_pager_tail")?;
    let tail = session
        .call_tool_raw(
            "repl",
            json!({
                "input": ":seek @8500",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let tail_text = result_text(&tail);

    session.cancel().await?;

    assert!(
        tail_text.contains("ZOD_REFRESH_TAIL"),
        "expected refreshed pager tail output, got: {tail_text:?}"
    );
    assert!(
        !tail_text.contains("v5> refreshed-hidden-echo"),
        "refreshed input echo should remain transcript-only in pager output, got: {tail_text:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_pager_hidden_input_echo_before_stderr_does_not_add_blank_line() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_pager_server(&control_log, 4_000).await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "emit-stderr-after-input",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);

    session.cancel().await?;

    assert!(
        text.starts_with("stderr: boom\n"),
        "hidden input echoes should not create a leading blank line before stderr, got: {text:?}"
    );
    assert!(
        !text.contains("v5> emit-stderr-after-input"),
        "leading generated input_line echo should be absent, got: {text:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_files_reset_clears_stderr_prefix_state() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_server(&control_log).await?;

    let first = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "partial-stderr",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let first_text = result_text(&first);
    assert!(
        first_text.contains("stderr: partial"),
        "expected first reply to drain partial stderr, got: {first_text:?}"
    );

    let reset = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "\u{4}",
                "timeout_ms": 1_000
            }),
        )
        .await?;
    let reset_text = result_text(&reset);
    assert!(
        reset_text.contains("[repl] new session started"),
        "expected reset to restart the worker, got: {reset_text:?}"
    );

    let stderr_after_stderr = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "emit-stderr-after-input",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let stderr_after_stderr_text = result_text(&stderr_after_stderr);
    assert!(
        stderr_after_stderr_text.starts_with("stderr: boom\n"),
        "reset should restore stderr prefixing after partial stderr, got: {stderr_after_stderr_text:?}"
    );

    let stdout = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "partial-stdout",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let stdout_text = result_text(&stdout);
    assert!(
        stdout_text.contains("partial"),
        "expected reply to drain partial stdout, got: {stdout_text:?}"
    );

    let reset = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "\u{4}",
                "timeout_ms": 1_000
            }),
        )
        .await?;
    let reset_text = result_text(&reset);
    assert!(
        reset_text.contains("[repl] new session started"),
        "expected second reset to restart the worker, got: {reset_text:?}"
    );

    let stderr_after_stdout = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "emit-stderr-after-input",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let stderr_after_stdout_text = result_text(&stderr_after_stdout);
    assert!(
        stderr_after_stdout_text.starts_with("stderr: boom\n"),
        "reset should not add stale stdout separation before fresh stderr, got: {stderr_after_stdout_text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_stderr_label_starts_after_unterminated_stdout() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_server(&control_log).await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "partial-stdout-then-newline-stderr",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);

    session.cancel().await?;

    assert!(
        text.contains("partial\nstderr: \nerr\n"),
        "stderr label should start on a fresh line after partial stdout, got: {text:?}"
    );
    assert!(
        !text.contains("partialstderr:"),
        "stderr label should not be concatenated to partial stdout, got: {text:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_hidden_input_echo_preserves_unterminated_stdout_before_stderr() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_server(&control_log).await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "partial-stdout\nemit-stderr-after-input",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);

    session.cancel().await?;

    assert!(
        text.contains("partial\nstderr: boom\n"),
        "hidden input echo should not hide unterminated stdout before stderr, got: {text:?}"
    );
    assert!(
        !text.contains("partialstderr:"),
        "stderr label should not be concatenated to partial stdout across hidden input echo, got: {text:?}"
    );
    assert!(
        !text.contains("v5> emit-stderr-after-input"),
        "generated input_line echo should be absent, got: {text:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_files_clean_session_end_flushes_partial_utf8_before_notice() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_server(&control_log).await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "partial-utf8-then-exit",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);

    session.cancel().await?;

    assert!(
        text.contains("\\xC3\n[repl] session ended\n"),
        "session-end notice should start after the flushed partial UTF-8 tail, got: {text:?}"
    );
    assert!(
        !text.contains("\\xC3[repl]"),
        "session-end notice should not be concatenated to an escaped UTF-8 tail, got: {text:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_files_timeout_drains_event_after_incomplete_utf8() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_server(&control_log).await?;

    let timed_out = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "split-utf8-before-delayed-image",
                "timeout_ms": 50
            }),
        )
        .await?;
    let timeout_text = result_text(&timed_out);
    assert!(
        timeout_text.contains("<<repl status: busy"),
        "expected the delayed UTF-8 request to time out, got: {timeout_text:?}"
    );
    assert!(
        !timeout_text.contains("é"),
        "timeout reply should not wait for the delayed UTF-8 tail grace, got: {timeout_text:?}"
    );
    assert_eq!(
        result_image_count(&timed_out),
        1,
        "timeout reply should include the image emitted before the timeout"
    );

    let completed = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&completed);

    session.cancel().await?;

    assert!(
        text.contains("\\xA9"),
        "the delayed UTF-8 continuation should remain available on the follow-up poll, got: {text:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_files_timeout_drains_stderr_after_incomplete_utf8() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_server(&control_log).await?;

    let timed_out = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "partial-utf8-stderr-then-sleep",
                "timeout_ms": 300
            }),
        )
        .await?;
    let timeout_text = result_text(&timed_out);

    session.cancel().await?;

    assert!(
        timeout_text.contains("<<repl status: busy"),
        "expected the delayed UTF-8 request to time out, got: {timeout_text:?}"
    );
    assert!(
        timeout_text.contains("\\xC3"),
        "timeout drain should flush an incomplete leading UTF-8 tail, got: {timeout_text:?}"
    );
    assert!(
        timeout_text.contains("stderr: tail-visible\n"),
        "timeout drain should expose later stderr after an incomplete UTF-8 tail, got: {timeout_text:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_files_timeout_does_not_wait_for_utf8_tail_grace_after_expiry() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let release_path = tempdir.path().join("release-utf8-tail");
    let release = release_path
        .to_str()
        .ok_or("release path must be valid utf-8")?
        .to_string();
    let session = spawn_zod_server_with_extra_env_and_extra_args(
        &control_log,
        vec![("MCP_REPL_ZOD_UTF8_TAIL_RELEASE", release.as_str())],
        Vec::new(),
    )
    .await?;
    warm_zod_session(&session).await?;

    let timed_out = tokio::time::timeout(
        Duration::from_secs(5),
        session.call_tool_raw(
            "repl",
            json!({
                "input": "partial-utf8-then-wait-for-release",
                "timeout_ms": 50
            }),
        ),
    )
    .await
    .map_err(|_| "timeout reply waited for unreleased UTF-8 tail")??;
    let timeout_text = result_text(&timed_out);
    let control_text = wait_for_log_contains(&control_log, "waiting_utf8_tail_release")?;

    assert!(
        timeout_text.contains("<<repl status: busy"),
        "expected the incomplete UTF-8 request to time out, got: {timeout_text:?}"
    );
    assert!(
        !timeout_text.contains("\\xC3"),
        "timeout reply should keep the incomplete UTF-8 tail pending, got: {timeout_text:?}"
    );
    assert!(
        !release_path.exists(),
        "timeout reply should return before the test releases the UTF-8 tail; control log: {control_text:?}"
    );

    fs::write(&release_path, "go")?;
    let completed = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let completed_text = result_text(&completed);
    session.cancel().await?;

    assert!(
        completed_text.contains("é"),
        "follow-up poll should drain the released UTF-8 sequence, got: {completed_text:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_pager_session_end_flushes_partial_utf8_before_notice() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_pager_server(&control_log, 4_000).await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "partial-utf8-then-exit",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);

    session.cancel().await?;

    assert!(
        text.contains("\\xC3\n[repl] session ended\n"),
        "pager final reply should flush the partial UTF-8 tail before the session-end notice, got: {text:?}"
    );
    assert!(
        !text.contains("\\xC3[repl]"),
        "session-end notice should not be concatenated to an escaped UTF-8 tail, got: {text:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_pager_timeout_drains_event_after_incomplete_utf8() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_pager_server(&control_log, 4_000).await?;

    let timed_out = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "split-utf8-before-delayed-image",
                "timeout_ms": 50
            }),
        )
        .await?;
    let timeout_text = result_text(&timed_out);

    assert!(
        timeout_text.contains("<<repl status: busy"),
        "expected the delayed UTF-8 request to time out, got: {timeout_text:?}"
    );
    assert!(
        !timeout_text.contains("é"),
        "pager timeout should not wait for the delayed UTF-8 tail grace, got: {timeout_text:?}"
    );
    assert_eq!(
        result_image_count(&timed_out),
        1,
        "pager timeout should include the image emitted before the timeout"
    );

    let completed = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&completed);

    session.cancel().await?;

    assert!(
        text.contains("\\xA9"),
        "the delayed UTF-8 continuation should remain available on the pager follow-up poll, got: {text:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_pager_preserves_equal_offset_update_notice_before_image() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_pager_server(&control_log, 1_000).await?;

    let initial = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "output-source-image",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    assert_eq!(
        result_image_count(&initial),
        1,
        "expected initial image to establish update state"
    );

    let update = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "output-image-update-with-tail",
                "timeout_ms": 10_000
            }),
        )
        .await?;

    session.cancel().await?;

    let notice = "[repl] image update from previous request shown as a new image";
    let notice_index = first_text_index_containing(&update, notice)
        .ok_or("expected image update notice in pager reply")?;
    let image_index = first_image_index(&update).ok_or_else(|| {
        format!(
            "expected updated image in pager reply, got content order: {:?}",
            update.content
        )
    })?;
    assert!(
        notice_index < image_index,
        "expected equal-offset update notice before image, got content order: {:?}",
        update.content
    );

    Ok(())
}

#[cfg(target_family = "unix")]
#[tokio::test(flavor = "multi_thread")]
async fn zod_raw_split_utf8_survives_input_wait_marker() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_server(&control_log).await?;

    let first = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "raw-split-utf8-around-input-wait",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let first_text = result_text(&first);

    wait_for_log_contains(&control_log, "input_wait")?;
    std::thread::sleep(Duration::from_millis(250));

    let completed = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&completed);
    let combined_text = format!("{first_text}{text}");

    session.cancel().await?;

    assert!(
        combined_text.contains("é\n"),
        "split raw UTF-8 should render as one character after input_wait, got: {combined_text:?}"
    );
    assert!(
        !combined_text.contains("\\xC3") && !combined_text.contains("\\xA9"),
        "split raw UTF-8 should not render as escaped bytes, got: {combined_text:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_pager_output_text_matching_input_line_remains_visible() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_pager_server(&control_log, 4_000).await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "output-matching-input-line",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);

    session.cancel().await?;

    assert!(
        text.contains("v5> output-matching-input-line\nVISIBLE\n"),
        "output_text that matches input_line metadata should remain visible in pager mode, got: {text:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_files_completion_settles_split_utf8_tail_before_request_boundary() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_server(&control_log).await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "split-utf8-after-completion",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);

    session.cancel().await?;

    assert!(
        text.contains("é\n"),
        "completion should combine delayed UTF-8 continuation bytes before the request boundary, got: {text:?}"
    );
    assert!(
        !text.contains("\\xC3") && !text.contains("\\xA9"),
        "completion should not seal split UTF-8 bytes when the continuation arrives during settle, got: {text:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_files_completion_keeps_stable_wait_after_utf8_recovery() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_server(&control_log).await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "split-utf8-then-more-after-completion",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);

    session.cancel().await?;

    assert!(
        text.contains("é after\n"),
        "completion should keep settling after UTF-8 recovery near the grace deadline, got: {text:?}"
    );
    assert!(
        !text.contains("\\xC3") && !text.contains("\\xA9"),
        "completion should not seal split UTF-8 bytes when the continuation arrives during settle, got: {text:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_files_completion_bounds_stable_wait_after_utf8_recovery() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_server(&control_log).await?;
    warm_zod_session(&session).await?;

    let start = std::time::Instant::now();
    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "split-utf8-then-continuous-output-after-completion",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let elapsed = start.elapsed();
    let text = result_text(&result);

    session.cancel().await?;

    assert!(
        elapsed < Duration::from_millis(1_600),
        "completion should cap UTF-8 settle even when later output prevents stability; elapsed {elapsed:?}, got: {text:?}"
    );
    assert!(
        text.contains("é"),
        "completion should include UTF-8 recovery before the cap, got: {text:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_files_request_boundary_resets_stderr_after_sealed_utf8_tail() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let late_stderr_marker = tempdir.path().join("late-stderr-marker");
    let late_stderr_marker_env = late_stderr_marker.display().to_string();
    let session = spawn_zod_server_with_extra_env_and_extra_args(
        &control_log,
        vec![(
            "MCP_REPL_ZOD_LATE_STDERR_MARKER",
            late_stderr_marker_env.as_str(),
        )],
        Vec::new(),
    )
    .await?;

    let first = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "partial-stderr-utf8-then-late-stderr-after-completion",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let first_text = result_text(&first);
    assert!(
        !first_text.contains("\\xC3"),
        "completed request should keep an incomplete UTF-8 tail detached, got: {first_text:?}"
    );

    wait_for_log_contains(&control_log, "waiting_late_stderr_marker")?;
    std::fs::write(&late_stderr_marker, b"go")?;
    wait_for_log_contains(&control_log, "late_stderr_after_completion")?;

    let second = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let second_text = result_text(&second);

    session.cancel().await?;

    assert!(
        second_text.contains("stderr: \\xC3stderr: after\n"),
        "request boundary should reset stderr rendering after sealing the prior UTF-8 tail, got: {second_text:?}"
    );
    assert!(
        !second_text.contains("stderr: \\xC3after\n"),
        "stderr rendering state leaked across the sealed UTF-8 tail, got: {second_text:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_v5_split_utf8_stdout_survives_interleaved_stderr() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_server(&control_log).await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "split-utf8-interleaved-stderr",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);

    session.cancel().await?;

    assert!(
        text.contains("é"),
        "split stdout UTF-8 should render as one character, got: {text:?}"
    );
    assert!(
        text.contains("stderr: err\n"),
        "interleaved stderr should remain visible, got: {text:?}"
    );
    let stdout_index = text
        .find("é")
        .expect("split stdout UTF-8 should render as one character");
    let stderr_index = text
        .find("stderr: err\n")
        .expect("interleaved stderr should remain visible");
    assert!(
        stdout_index < stderr_index,
        "split stdout UTF-8 should keep its original position before stderr, got: {text:?}"
    );
    assert!(
        !text.contains("\\xC3") && !text.contains("\\xA9"),
        "split stdout UTF-8 should not render as escaped bytes, got: {text:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_v5_split_utf8_stdout_stays_before_interleaved_image() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_server(&control_log).await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "split-utf8-before-image",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);

    session.cancel().await?;

    let image_index = result
        .content
        .iter()
        .position(|item| matches!(item.raw, RawContent::Image(_)))
        .ok_or("expected interleaved image in reply")?;
    let text_before_image = result.content[..image_index]
        .iter()
        .filter_map(|item| match &item.raw {
            RawContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");

    assert!(
        text_before_image.contains("é"),
        "split stdout UTF-8 should render before the following image, got contents: {:?}",
        result.content
    );
    assert!(
        text.contains("é"),
        "split stdout UTF-8 should render as one character, got: {text:?}"
    );
    assert!(
        !text.contains("\\xC3") && !text.contains("\\xA9"),
        "split stdout UTF-8 should not render as escaped bytes, got: {text:?}"
    );

    Ok(())
}

#[cfg(target_family = "unix")]
#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_ready_failure_releases_ipc_for_next_launch() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let marker_path = tempdir.path().join("failed-once");
    let session = spawn_zod_fail_once_ready_server(&control_log, &marker_path).await?;

    let first = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "second launch works",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let first_text = result_text(&first);
    if first_text.contains("v5-output: second launch works\n") {
        assert!(
            marker_path.is_file(),
            "expected the wrapper to exercise the ready-failure launch"
        );
        session.cancel().await?;
        return Ok(());
    }

    assert!(
        first_text.contains("worker error: worker protocol error")
            && first_text.contains("worker_ready"),
        "expected first launch to fail while waiting for worker_ready, got: {first_text:?}"
    );

    let second = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "second launch works",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let second_text = result_text(&second);
    assert!(
        second_text.contains("v5-output: second launch works\n"),
        "expected second launch to use a fresh IPC connection, got: {second_text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[cfg(target_family = "unix")]
#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_session_end_respawn_terminates_old_worker() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_server(&control_log).await?;

    let first = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "session-end-park",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let first_text = result_text(&first);
    assert!(
        first_text.contains("v5>") || first_text.contains("[repl] session ended"),
        "expected first worker prompt or session end, got: {first_text:?}"
    );

    let log = wait_for_log_contains(&control_log, "park_after_session_end")?;
    let old_pid = first_logged_pid(&log).ok_or("expected zod worker pid in control log")?;

    let second = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "write-session-temp-marker",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let mut second_text = result_text(&second);
    if !second_text.contains("session-temp-marker: ") {
        assert!(
            second_text.contains("session ended") || second_text.contains("session_end"),
            "expected session_end before respawn, got: {second_text:?}"
        );
        let third = session
            .call_tool_raw(
                "repl",
                json!({
                    "input": "write-session-temp-marker",
                    "timeout_ms": 10_000
                }),
            )
            .await?;
        second_text = result_text(&third);
    }
    let marker = extract_prefixed_value(&second_text, "session-temp-marker: ")
        .map(PathBuf::from)
        .ok_or_else(|| format!("expected respawned worker temp marker, got: {second_text:?}"))?;
    assert!(
        marker.exists(),
        "expected respawned worker marker to exist before old worker cleanup: {}",
        marker.display()
    );

    let exit_result = wait_for_process_exit(old_pid);
    assert!(
        marker.exists(),
        "old worker cleanup removed respawned worker temp marker: {}",
        marker.display()
    );
    session.cancel().await?;
    exit_result
}

#[cfg(target_family = "unix")]
#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_session_end_respawn_drops_late_raw_stdout_from_old_worker() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let late_raw_marker = tempdir.path().join("late-raw-marker");
    let late_raw_marker_env = late_raw_marker.display().to_string();
    let session = spawn_zod_server_with_extra_env_and_extra_args(
        &control_log,
        vec![("MCP_REPL_ZOD_LATE_RAW_MARKER", late_raw_marker_env.as_str())],
        Vec::new(),
    )
    .await?;

    let first = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "session-end-raw-after-marker",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let first_text = result_text(&first);
    assert!(
        first_text.contains("v5>") || first_text.contains("[repl] session ended"),
        "expected first worker prompt or session end, got: {first_text:?}"
    );
    wait_for_log_contains(&control_log, "waiting_late_raw_marker")?;

    let control_log_for_thread = control_log.clone();
    let late_raw_marker_for_thread = late_raw_marker.clone();
    let marker_writer = std::thread::spawn(move || -> TestResult<()> {
        wait_for_log_contains(
            &control_log_for_thread,
            "input_batch input=sleep 1000\\nfresh-after-respawn",
        )?;
        std::fs::write(&late_raw_marker_for_thread, b"go")?;
        Ok(())
    });

    let second = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "sleep 1000\nfresh-after-respawn",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    marker_writer
        .join()
        .map_err(|_| "late raw marker writer panicked")??;
    assert!(
        late_raw_marker.exists(),
        "expected test marker to trigger old worker raw stdout"
    );

    let second_text = result_text(&second);
    assert!(
        second_text.contains("v5-output: fresh-after-respawn\n"),
        "expected replacement worker output, got: {second_text:?}"
    );
    assert!(
        !second_text.contains("STALE_RAW_AFTER_SESSION_END"),
        "old worker raw stdout leaked into replacement reply: {second_text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_files_bundle_preserves_image_after_large_hidden_input_echo() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_server_with_extra_env_server_env_and_extra_args(
        &control_log,
        Vec::new(),
        vec![(
            "MCP_REPL_OUTPUT_BUNDLE_MAX_BYTES".to_string(),
            "36000".to_string(),
        )],
        Vec::new(),
    )
    .await?;

    let hidden = "h".repeat(12_000);
    let input = format!("silent {hidden}\noutput-image-bytes 12000");
    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": input,
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);

    session.cancel().await?;

    assert_ne!(
        result.is_error,
        Some(true),
        "hidden echo image bundle returned an error: {text:?}"
    );
    assert!(
        text.contains("later content omitted"),
        "expected tight bundle quota to report omitted hidden transcript tail, got: {text:?}"
    );
    assert_eq!(
        result_image_count(&result),
        1,
        "expected the later reply-visible image to remain visible, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_files_bundle_records_hidden_echo_omission_before_later_visible_text() -> TestResult<()>
{
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_server_with_extra_env_server_env_and_extra_args(
        &control_log,
        Vec::new(),
        vec![(
            "MCP_REPL_OUTPUT_BUNDLE_MAX_BYTES".to_string(),
            "14000".to_string(),
        )],
        Vec::new(),
    )
    .await?;

    let hidden = "h".repeat(24_000);
    let input = format!("silent {hidden}\nvisible-after-omission\noutput-image-bytes 100");
    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": input,
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);
    let events_log = events_log_path(&text)
        .unwrap_or_else(|| panic!("expected mixed output bundle events.log, got: {text:?}"));
    let bundle_dir = events_log
        .parent()
        .unwrap_or_else(|| panic!("events.log missing parent: {events_log:?}"));
    let transcript = fs::read_to_string(bundle_dir.join("transcript.txt"))?;
    let events = fs::read_to_string(&events_log)?;

    session.cancel().await?;

    assert_ne!(
        result.is_error,
        Some(true),
        "hidden echo ordering bundle returned an error: {text:?}"
    );
    let omission_row = events
        .lines()
        .position(|line| line.contains("output bundle quota reached"))
        .unwrap_or_else(|| panic!("expected omission row in events.log, got: {events:?}"));
    let visible_row = events
        .lines()
        .position(|line| {
            text_row_byte_range(line)
                .and_then(|(start, end)| transcript.get(start..end))
                .is_some_and(|slice| slice.contains("v5-output: visible-after-omission"))
        })
        .unwrap_or_else(|| {
            panic!("expected a text row for later visible output in events.log, got: {events:?}")
        });
    assert!(
        omission_row < visible_row,
        "expected omission row before later visible text row, got events={events:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_files_bundle_reports_hidden_echo_dropped_for_later_raw_text() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_server_with_extra_env_server_env_and_extra_args(
        &control_log,
        Vec::new(),
        vec![(
            "MCP_REPL_OUTPUT_BUNDLE_MAX_BYTES".to_string(),
            (64 * 1024 * 1024).to_string(),
        )],
        Vec::new(),
    )
    .await?;

    let payload = "i".repeat(512 * 1024);
    let mut input = String::new();
    for index in 0..127 {
        input.push_str(&format!("silent {index:03}-{payload}\n"));
    }
    input.push_str("repeat-output 600000\n");
    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": input,
                "timeout_ms": 30_000
            }),
        )
        .await?;
    let text = result_text(&result);
    let transcript_path = disclosed_path(&text, "transcript.txt")
        .unwrap_or_else(|| panic!("expected text output bundle, got: {text:?}"));
    let transcript = fs::read_to_string(&transcript_path)?;

    session.cancel().await?;

    assert_ne!(
        result.is_error,
        Some(true),
        "raw-text echo eviction bundle returned an error: {text:?}"
    );
    assert!(
        text.contains("later content omitted"),
        "expected dropped input echoes to be reported, got: {text:?}"
    );
    assert!(
        transcript.contains("ZOD_BEGIN") && transcript.contains("ZOD_END"),
        "expected later raw text output to survive input echo eviction, got: {transcript:?}"
    );
    assert!(
        !transcript.contains("silent 000-"),
        "expected the first hidden input echo to be evicted, got: {transcript:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_files_bundle_reports_omitted_input_echoes_past_timeline_capacity() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_server_with_extra_env_server_env_and_extra_args(
        &control_log,
        Vec::new(),
        vec![(
            "MCP_REPL_OUTPUT_BUNDLE_MAX_BYTES".to_string(),
            "1048576".to_string(),
        )],
        Vec::new(),
    )
    .await?;

    let payload = "i".repeat(512 * 1024);
    let mut input = String::new();
    for index in 0..140 {
        input.push_str(&format!("silent {index:03}-{payload}\n"));
    }
    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": input,
                "timeout_ms": 30_000
            }),
        )
        .await?;
    let text = result_text(&result);
    let bundle_path = events_log_path(&text)
        .or_else(|| disclosed_path(&text, "transcript.txt"))
        .unwrap_or_else(|| panic!("expected input echo output bundle, got: {text:?}"));
    let bundle_dir = bundle_path
        .parent()
        .unwrap_or_else(|| panic!("bundle path missing parent: {bundle_path:?}"));
    let transcript = fs::read_to_string(bundle_dir.join("transcript.txt"))?;

    session.cancel().await?;

    assert_ne!(
        result.is_error,
        Some(true),
        "oversized input echo bundle returned an error: {text:?}"
    );
    assert!(
        text.contains("later content omitted"),
        "expected omitted input echoes to be reported, got: {text:?}"
    );
    assert!(
        transcript.contains("silent 000-"),
        "expected files-mode head retention to keep the first input echo, got: {transcript:?}"
    );
    assert!(
        !transcript.contains("silent 139-"),
        "expected later input echoes past timeline capacity to be omitted, got: {transcript:?}"
    );
    Ok(())
}

fn text_row_byte_range(line: &str) -> Option<(usize, usize)> {
    let range = line.strip_prefix('T')?.split(" bytes=").nth(1)?;
    let range = range.split_whitespace().next()?;
    let (start, end) = range.split_once('-')?;
    Some((start.parse().ok()?, end.parse().ok()?))
}

#[cfg(target_family = "unix")]
#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_session_end_respawn_drops_late_sideband_from_old_worker() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let late_sideband_marker = tempdir.path().join("late-sideband-marker");
    let late_sideband_marker_env = late_sideband_marker.display().to_string();
    let session = spawn_zod_server_with_extra_env_and_extra_args(
        &control_log,
        vec![(
            "MCP_REPL_ZOD_LATE_SIDEBAND_MARKER",
            late_sideband_marker_env.as_str(),
        )],
        Vec::new(),
    )
    .await?;

    let first = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "session-end-sideband-after-marker",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let first_text = result_text(&first);
    assert!(
        first_text.contains("v5>") || first_text.contains("[repl] session ended"),
        "expected first worker prompt or session end, got: {first_text:?}"
    );
    wait_for_log_contains(&control_log, "waiting_late_sideband_marker")?;

    let control_log_for_thread = control_log.clone();
    let late_sideband_marker_for_thread = late_sideband_marker.clone();
    let marker_writer = std::thread::spawn(move || -> TestResult<()> {
        wait_for_log_contains(
            &control_log_for_thread,
            "input_batch input=sleep 1000\\nfresh-after-sideband-respawn",
        )?;
        std::fs::write(&late_sideband_marker_for_thread, b"go")?;
        Ok(())
    });

    let second = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "sleep 1000\nfresh-after-sideband-respawn",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    marker_writer
        .join()
        .map_err(|_| "late sideband marker writer panicked")??;
    assert!(
        late_sideband_marker.exists(),
        "expected test marker to trigger old worker sideband output"
    );
    wait_for_log_contains(&control_log, "late_sideband_output_after_session_end")?;

    let second_text = result_text(&second);
    assert!(
        second_text.contains("v5-output: fresh-after-sideband-respawn\n"),
        "expected replacement worker output, got: {second_text:?}"
    );
    assert!(
        !second_text.contains("STALE_SIDEBAND_AFTER_SESSION_END"),
        "old worker sideband output leaked into replacement reply: {second_text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_v5_input_batch_write_respects_timeout_when_control_reader_stalls()
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
            panic!("v5 input_batch write did not respect timeout_ms");
        }
    };
    let text = result_text(&result);
    assert!(
        text.contains("worker response timed out"),
        "expected bounded input_batch write timeout, got: {text:?}"
    );
    let log = wait_for_log_contains(&control_log, "control_reader_stalled")?;
    assert!(
        !log.contains("input_batch input="),
        "stalled fixture must not consume the timed-out input batch, got log: {log:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_v5_input_line_is_ordered_before_output_text_and_rendered() -> TestResult<()> {
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
        !text.contains("v5> emit-output-after-input"),
        "leading generated input_line echo should be absent, got: {text:?}"
    );
    assert!(
        text.contains("v5> "),
        "expected worker prompt after output_text, got: {text:?}"
    );

    let log = wait_for_log_contains(&control_log, "input_line text=emit-output-after-input\\n")?;
    let input_line = log
        .find("input_line text=emit-output-after-input")
        .ok_or_else(|| "missing input_line log".to_string())?;
    let output_text = log
        .find("output_text")
        .ok_or_else(|| "missing output_text log".to_string())?;
    assert!(
        input_line < output_text,
        "expected worker to emit input_line before output_text, got log: {log:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_v5_output_text_matching_input_line_remains_visible() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_server(&control_log).await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "output-matching-input-line",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);

    assert!(
        text.contains("v5> output-matching-input-line\nVISIBLE\n"),
        "output_text that matches input_line metadata should remain visible, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_v5_input_wait_completes_batch() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_server(&control_log).await?;

    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "input-wait-only",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let text = result_text(&result);

    assert!(
        !text.contains("<<repl status: busy"),
        "input_wait should complete the input batch, got: {text:?}"
    );
    assert!(
        text.contains("v5> "),
        "expected input_wait prompt from v5 worker, got: {text:?}"
    );
    wait_for_log_contains(&control_log, "input_wait")?;

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_v5_busy_follow_up_does_not_send_second_input_batch() -> TestResult<()> {
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
                "input": "second v5 input",
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
        "expected active input to settle after interrupt, got: {interrupted_text:?}"
    );

    let log = wait_for_log_contains(&control_log, "input_wait")?;
    assert!(
        !log.contains("second v5 input"),
        "busy follow-up must not reach the active v5 worker, got log: {log:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_v5_interrupt_carries_interrupt_id() -> TestResult<()> {
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
        "expected v5 worker to observe sideband interrupt, got: {interrupted_text:?}"
    );
    #[cfg(target_family = "unix")]
    assert!(
        interrupted_text.contains("os interrupt: observed"),
        "expected v5 worker to observe OS interrupt, got: {interrupted_text:?}"
    );

    let log = wait_for_log_contains(&control_log, "interrupt")?;
    assert!(
        log.contains("interrupt interrupt_id="),
        "interrupt must carry an interrupt identity, got log: {log:?}"
    );
    assert!(
        !log.contains("interrupt input_id"),
        "interrupt must not carry input identity, got log: {log:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_interrupt_ack_precedes_os_interrupt_observation() -> TestResult<()> {
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
        "expected sideband interrupt observation, got: {interrupted_text:?}"
    );
    #[cfg(target_family = "unix")]
    assert!(
        interrupted_text.contains("os interrupt: observed"),
        "expected OS interrupt observation, got: {interrupted_text:?}"
    );

    let log = wait_for_log_contains(&control_log, "os_interrupt_observed")?;
    let ack_idx = log
        .find("interrupt_ack interrupt_id=")
        .expect("interrupt ack log should be present");
    let os_idx = log
        .find("os_interrupt_observed")
        .expect("OS interrupt observation log should be present");
    assert!(
        ack_idx < os_idx,
        "expected ack before runtime observed OS interrupt, got log: {log:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_interrupt_ack_timeout_still_sends_os_interrupt() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_without_interrupt_ack_server(&control_log).await?;

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
    #[cfg(target_family = "unix")]
    assert!(
        interrupted_text.contains("os interrupt: observed"),
        "expected OS interrupt despite missing ack, got: {interrupted_text:?}"
    );

    let log = wait_for_log_contains(&control_log, "interrupt_ack_suppressed")?;
    assert!(
        log.contains("interrupt"),
        "expected sideband interrupt to be received before ack suppression, got: {log:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_interrupt_ack_wait_protocol_error_fails_closed() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_protocol_error_before_interrupt_ack_server(&control_log).await?;

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
        interrupted_text.contains("worker protocol error: invalid output_text base64"),
        "expected ack wait protocol error to fail closed, got: {interrupted_text:?}"
    );
    assert!(
        interrupted.is_error.unwrap_or(false),
        "expected protocol error interrupt result to set isError"
    );

    let log = wait_for_log_contains(&control_log, "interrupt_protocol_error_before_ack")?;
    assert!(
        log.contains("interrupt"),
        "expected worker to receive interrupt before protocol error, got: {log:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_rejects_preemptive_interrupt_ack() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let marker = tempdir.path().join("release-preemptive-ack");
    let session = spawn_zod_preemptive_interrupt_ack_server(&control_log, &marker).await?;

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

    fs::write(&marker, b"go")?;
    let log = wait_for_log_contains(&control_log, "preemptive_interrupt_ack interrupt_id=1")?;
    assert!(
        !log.contains("interrupt interrupt_id=1"),
        "preemptive ack must be emitted before the server sends interrupt 1, got log: {log:?}"
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
        interrupted_text.contains("worker protocol error: interrupt_ack for unsent interrupt"),
        "expected preemptive ack to fail closed, got: {interrupted_text:?}"
    );
    assert!(
        interrupted.is_error.unwrap_or(false),
        "expected preemptive ack interrupt result to set isError"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_interrupt_ack_wait_respects_tiny_timeout() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_without_interrupt_ack_server(&control_log).await?;

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

    let started = Instant::now();
    let interrupted = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "\u{3}",
                "timeout_ms": 10
            }),
        )
        .await?;
    let elapsed = started.elapsed();
    let interrupted_text = result_text(&interrupted);
    assert!(
        elapsed < Duration::from_millis(80),
        "interrupt ack wait ignored tiny timeout; elapsed {elapsed:?}, reply {interrupted_text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_interrupt_tail_settle_respects_tiny_timeout() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_server(&control_log).await?;

    warm_zod_session(&session).await?;
    let started = Instant::now();
    let result = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "\u{3}emit-output-after-input",
                "timeout_ms": 5
            }),
        )
        .await?;
    let elapsed = started.elapsed();
    let text = result_text(&result);
    assert!(
        text.contains("timeout"),
        "expected tiny interrupt-tail timeout, got: {text:?}"
    );
    assert!(
        elapsed < Duration::from_millis(45),
        "interrupt tail settle ignored tiny timeout; elapsed {elapsed:?}, reply {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_v5_input_wait_interrupt_is_sent_without_active_input() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_server(&control_log).await?;

    let first = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "input-wait-only",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let first_text = result_text(&first);
    assert!(
        first_text.contains("v5> "),
        "expected v5 worker to settle before input-wait interrupt, got: {first_text:?}"
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
        "input-wait Ctrl-C must remain a non-error control reply, got: {interrupted_text:?}"
    );
    assert!(
        !interrupted_text.contains("<<repl status: busy"),
        "input-wait Ctrl-C must use cached readiness instead of timing out, got: {interrupted_text:?}"
    );

    let log = wait_for_log_contains(&control_log, "interrupt")?;
    assert!(
        log.contains("interrupt"),
        "input-wait Ctrl-C must send payload-free sideband interrupt, got log: {log:?}"
    );
    assert!(
        !log.contains("interrupt input_id"),
        "input-wait Ctrl-C must not send an identity-bearing interrupt, got log: {log:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zod_worker_v5_input_line_after_input_wait_is_protocol_error() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_server(&control_log).await?;

    let first = session
        .call_tool_raw(
            "repl",
            json!({
                "input": "late-input-line-after-input-wait",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let first_text = result_text(&first);
    wait_for_log_contains(&control_log, "late_input_line")?;
    if !first_text.contains("input_line") {
        assert!(
            first_text.contains("v5> "),
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
async fn zod_worker_v5_latched_protocol_error_blocks_next_input_batch() -> TestResult<()> {
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
                "input": "bad-output-after-input-wait 500",
                "timeout_ms": 10_000
            }),
        )
        .await?;
    let first_text = result_text(&first);
    assert!(
        first_text.contains("v5> "),
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
                "input": "must not reach v5 worker",
                "timeout_ms": 100
            }),
        )
        .await?;
    let second_text = result_text(&second);
    assert!(
        second_text.contains("invalid output_text base64"),
        "expected latched protocol error before next v5 turn, got: {second_text:?}"
    );

    let log = read_optional(&control_log);
    assert!(
        !log.contains("must not reach v5 worker"),
        "latched protocol error must prevent the next input_batch, got log: {log:?}"
    );

    session.cancel().await?;
    Ok(())
}
