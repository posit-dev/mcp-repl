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
    common::spawn_server_with_args(args).await
}

async fn spawn_zod_server(control_log: &std::path::Path) -> TestResult<common::McpTestSession> {
    spawn_zod_server_with_extra_args(control_log, Vec::new()).await
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
async fn zod_worker_interrupt_prefix_waits_for_fresh_ready() -> TestResult<()> {
    let tempdir = tempfile::tempdir()?;
    let control_log = tempdir.path().join("control.log");
    let session = spawn_zod_delayed_interrupt_ready_server(&control_log).await?;

    let result = session
        .write_stdin_raw_with("\u{3}after fresh ready", Some(10.0))
        .await?;
    let text = result_text(&result);
    assert!(
        text.contains("v5-output: after fresh ready\n"),
        "expected interrupt tail to run after fresh readiness, got: {text:?}"
    );

    let log = wait_for_log_contains(&control_log, "fresh_ready_after_interrupt")?;
    let fresh_ready_idx = log
        .find("fresh_ready_after_interrupt")
        .expect("fresh readiness log should be present");
    let input_idx = log
        .find("input_batch input=after fresh ready")
        .expect("interrupt tail input log should be present");
    assert!(
        fresh_ready_idx < input_idx,
        "expected tail input only after fresh readiness, got log: {log:?}"
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
async fn zod_worker_v5_interrupt_is_payload_free() -> TestResult<()> {
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
        !log.contains("interrupt input_id"),
        "interrupt must not carry input identity, got log: {log:?}"
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

    let log = read_optional(&control_log);
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
