#![allow(clippy::await_holding_lock)]

mod common;

use common::{McpTestSession, TestResult};
use rmcp::model::{CallToolResult, RawContent};
use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::{Builder, TempDir, tempdir};

const SANDBOX_STATE_META_CAPABILITY: &str = "codex/sandbox-state-meta";
const MISSING_INHERITED_STATE_MESSAGE: &str =
    "--sandbox inherit requested but no client sandbox state was provided";
const INLINE_TEXT_BUDGET_CHARS: usize = 3500;
const INLINE_TEXT_HARD_SPILL_THRESHOLD_CHARS: usize = INLINE_TEXT_BUDGET_CHARS * 5 / 4;
const UNDER_HARD_SPILL_TEXT_LEN: usize = INLINE_TEXT_BUDGET_CHARS + 200;
const OVER_HARD_SPILL_TEXT_LEN: usize = INLINE_TEXT_HARD_SPILL_THRESHOLD_CHARS + 200;

fn test_mutex() -> &'static Mutex<()> {
    static TEST_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
    TEST_MUTEX.get_or_init(|| Mutex::new(()))
}

fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    test_mutex().lock().unwrap_or_else(|err| err.into_inner())
}

fn collect_text(result: &CallToolResult) -> String {
    let text = result
        .content
        .iter()
        .filter_map(|content| match &content.raw {
            RawContent::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    text.lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            !(trimmed.starts_with("> ") || trimmed.starts_with("+ ") || trimmed == ">")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn home_env_vars(home_dir: &Path) -> Vec<(String, String)> {
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

fn linux_sandbox_exe_value(use_legacy_landlock: bool) -> Value {
    #[cfg(target_os = "linux")]
    {
        if use_legacy_landlock {
            Value::Null
        } else {
            Value::String("/tmp/codex-linux-sandbox".to_string())
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = use_legacy_landlock;
        Value::Null
    }
}

fn codex_sandbox_state_meta(
    sandbox_policy: Value,
    sandbox_cwd: &Path,
    use_legacy_landlock: bool,
) -> Value {
    json!({
        SANDBOX_STATE_META_CAPABILITY: {
            "sandboxPolicy": sandbox_policy,
            "sandboxCwd": sandbox_cwd,
            "useLegacyLandlock": use_legacy_landlock,
            "codexLinuxSandboxExe": linux_sandbox_exe_value(use_legacy_landlock),
        }
    })
}

fn workspace_write_meta(sandbox_cwd: &Path) -> Value {
    codex_sandbox_state_meta(
        json!({
            "type": "workspace-write",
            "writable_roots": [],
            "network_access": false,
            "exclude_tmpdir_env_var": false,
            "exclude_slash_tmp": false,
        }),
        sandbox_cwd,
        /*use_legacy_landlock*/ false,
    )
}

fn read_only_meta(sandbox_cwd: &Path) -> Value {
    codex_sandbox_state_meta(json!({"type": "read-only"}), sandbox_cwd, false)
}

fn full_access_meta(sandbox_cwd: &Path) -> Value {
    codex_sandbox_state_meta(json!({"type": "danger-full-access"}), sandbox_cwd, false)
}

fn encode_path(path: &Path) -> TestResult<String> {
    Ok(serde_json::to_string(&path.to_string_lossy().to_string())?)
}

fn bundle_transcript_path(text: &str) -> Option<std::path::PathBuf> {
    disclosed_path(text, "transcript.txt")
}

fn disclosed_path(text: &str, suffix: &str) -> Option<std::path::PathBuf> {
    let end = text.find(suffix)?.saturating_add(suffix.len());
    let start = text[..end]
        .rfind(|ch: char| ch.is_whitespace() || matches!(ch, '"' | '\'' | '[' | '('))
        .map_or(0, |idx| idx.saturating_add(1));
    Some(std::path::PathBuf::from(&text[start..end]))
}

fn outside_workspace_target(label: &str) -> TestResult<std::path::PathBuf> {
    let base = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
        .ok_or_else(|| "missing HOME/USERPROFILE for sandbox test target".to_string())?;
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    Ok(base.join(format!(".mcp-repl-{label}-{nanos}.txt")))
}

fn repo_scratch_dir(label: &str) -> TestResult<TempDir> {
    Ok(Builder::new()
        .prefix(&format!(".tmp-{label}-"))
        .tempdir_in(env!("CARGO_MANIFEST_DIR"))?)
}

fn write_file_code(path: &Path) -> TestResult<String> {
    let target = encode_path(path)?;
    Ok(format!(
        r#"
target <- {target}
tryCatch({{
  writeLines("allowed", target)
  cat("WRITE_OK\n")
}}, error = function(e) {{
  message("WRITE_ERROR:", conditionMessage(e))
}})
"#
    ))
}

fn variable_probe_code() -> &'static str {
    r#"cat(sprintf("X_EXISTS:%s\n", exists("x")))"#
}

fn backend_unavailable(text: &str) -> bool {
    common::backend_unavailable(text)
}

async fn spawn_inherit_server(cwd: &Path) -> TestResult<McpTestSession> {
    common::spawn_server_with_args_env_and_cwd(
        vec!["--sandbox".to_string(), "inherit".to_string()],
        Vec::new(),
        Some(cwd.to_path_buf()),
    )
    .await
}

async fn spawn_inherit_server_with_env(
    cwd: &Path,
    env: Vec<(String, String)>,
) -> TestResult<McpTestSession> {
    common::spawn_server_with_args_env_and_cwd(
        vec!["--sandbox".to_string(), "inherit".to_string()],
        env,
        Some(cwd.to_path_buf()),
    )
    .await
}

async fn spawn_inherit_then_workspace_write_server(cwd: &Path) -> TestResult<McpTestSession> {
    common::spawn_server_with_args_env_and_cwd(
        vec![
            "--sandbox".to_string(),
            "inherit".to_string(),
            "--sandbox".to_string(),
            "workspace-write".to_string(),
        ],
        Vec::new(),
        Some(cwd.to_path_buf()),
    )
    .await
}

async fn spawn_inherit_files_server(
    cwd: &Path,
    env: Vec<(String, String)>,
) -> TestResult<McpTestSession> {
    common::spawn_server_with_args_env_and_cwd(
        vec![
            "--sandbox".to_string(),
            "inherit".to_string(),
            "--oversized-output".to_string(),
            "files".to_string(),
        ],
        env,
        Some(cwd.to_path_buf()),
    )
    .await
}

async fn spawn_inherit_pager_server(cwd: &Path, page_chars: u64) -> TestResult<McpTestSession> {
    common::spawn_server_with_args_env_and_cwd_and_pager_page_chars(
        vec!["--sandbox".to_string(), "inherit".to_string()],
        Vec::new(),
        Some(cwd.to_path_buf()),
        page_chars,
    )
    .await
}

fn timeout_then_tail_code() -> &'static str {
    r#"
Sys.sleep(0.2)
cat("MID\n")
flush.console()
Sys.sleep(1.0)
cat("TAIL\n")
flush.console()
"#
}

fn timeout_then_done_code() -> &'static str {
    r#"
Sys.sleep(0.2)
cat("DONE\n")
flush.console()
"#
}

fn timeout_then_done_code_after(wait_secs: f64) -> String {
    format!(
        r#"
Sys.sleep({wait_secs:.3})
cat("DONE\n")
flush.console()
"#
    )
}

fn timeout_then_large_completion_code() -> &'static str {
    Box::leak(
        format!(
            "small <- paste(rep('s', {UNDER_HARD_SPILL_TEXT_LEN}), collapse = ''); \
             big <- paste(rep('t', {OVER_HARD_SPILL_TEXT_LEN}), collapse = ''); \
             cat('FIRST_START\\n'); \
             cat(small); \
             cat('\\nFIRST_END\\n'); \
             flush.console(); \
             Sys.sleep(0.5); \
             cat('SECOND_START\\n'); \
             cat(big); \
             cat('\\nSECOND_END\\n'); \
             flush.console()"
        )
        .into_boxed_str(),
    )
}

fn timeout_then_large_completion_and_quit_code() -> &'static str {
    Box::leak(
        format!(
            "small <- paste(rep('s', {UNDER_HARD_SPILL_TEXT_LEN}), collapse = ''); \
             big <- paste(rep('t', {OVER_HARD_SPILL_TEXT_LEN}), collapse = ''); \
             cat('FIRST_START\\n'); \
             cat(small); \
             cat('\\nFIRST_END\\n'); \
             flush.console(); \
             Sys.sleep(0.5); \
             cat('SECOND_START\\n'); \
             cat(big); \
             cat('\\nSECOND_END\\n'); \
             flush.console(); \
             quit('no')"
        )
        .into_boxed_str(),
    )
}

fn oversized_follow_up_code(marker: &str) -> String {
    format!(
        "big <- paste(rep('u', {OVER_HARD_SPILL_TEXT_LEN}), collapse = ''); \
         cat('{marker}_START\\n'); \
         cat(big); \
         cat('\\n{marker}_END\\n'); \
         flush.console()"
    )
}

fn test_delay_ms(default_ms: u64, windows_ms: u64) -> std::time::Duration {
    std::time::Duration::from_millis(if cfg!(windows) {
        windows_ms
    } else {
        default_ms
    })
}

fn latest_debug_events(debug_dir: &Path) -> TestResult<Vec<Value>> {
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
async fn sandbox_state_meta_capability_advertised_with_inherit() -> TestResult<()> {
    let _guard = test_guard();
    let session =
        common::spawn_server_with_args(vec!["--sandbox".to_string(), "inherit".to_string()])
            .await?;
    let info = session.server_info().ok_or_else(|| {
        Box::<dyn std::error::Error + Send + Sync>::from(
            "missing server info from initialize".to_string(),
        )
    })?;
    let experimental = info.capabilities.experimental.as_ref().ok_or_else(|| {
        Box::<dyn std::error::Error + Send + Sync>::from(
            "missing experimental capabilities".to_string(),
        )
    })?;
    assert!(
        experimental.contains_key(SANDBOX_STATE_META_CAPABILITY),
        "expected sandbox state meta capability in experimental: {experimental:?}"
    );
    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_state_meta_capability_hidden_without_inherit() -> TestResult<()> {
    let _guard = test_guard();
    let session = common::spawn_server().await?;
    let info = session.server_info().ok_or_else(|| {
        Box::<dyn std::error::Error + Send + Sync>::from(
            "missing server info from initialize".to_string(),
        )
    })?;
    let advertised = info
        .capabilities
        .experimental
        .as_ref()
        .is_some_and(|experimental| experimental.contains_key(SANDBOX_STATE_META_CAPABILITY));
    assert!(
        !advertised,
        "did not expect sandbox state meta capability without `--sandbox inherit`: {info:?}"
    );
    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_state_meta_capability_hidden_after_later_workspace_write_override()
-> TestResult<()> {
    let _guard = test_guard();
    let scratch = repo_scratch_dir("sandbox-inherit-override-workspace-write")?;
    let session = spawn_inherit_then_workspace_write_server(scratch.path()).await?;
    let info = session.server_info().ok_or_else(|| {
        Box::<dyn std::error::Error + Send + Sync>::from(
            "missing server info from initialize".to_string(),
        )
    })?;
    let advertised = info
        .capabilities
        .experimental
        .as_ref()
        .is_some_and(|experimental| experimental.contains_key(SANDBOX_STATE_META_CAPABILITY));
    assert!(
        !advertised,
        "did not expect sandbox state meta capability after later workspace-write override: {info:?}"
    );

    let target = scratch.path().join("override-write.txt");
    let result = session
        .write_stdin_raw_with(write_file_code(&target)?, Some(10.0))
        .await?;
    let text = collect_text(&result);
    if backend_unavailable(&text) {
        eprintln!("sandbox_state_updates backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("WRITE_OK"),
        "expected later workspace-write override to avoid inherit metadata requirements, got: {text}"
    );
    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_without_state_meta_fails_on_first_tool_call() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let session = spawn_inherit_server(temp.path()).await?;
    let result = session.write_stdin_raw_with("1+1", Some(2.0)).await?;
    let text = collect_text(&result);
    if backend_unavailable(&text) {
        eprintln!("sandbox_state_updates backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("--sandbox inherit requested but no client sandbox state was provided"),
        "expected missing sandbox-state-meta error, got: {text}"
    );
    assert!(
        !text.contains("2"),
        "did not expect successful evaluation, got: {text}"
    );
    assert_eq!(
        result.is_error,
        Some(true),
        "expected missing metadata on the first worker interaction to set isError, got: {:?}",
        result.is_error
    );
    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_with_malformed_state_meta_fails_on_first_tool_call() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let session = spawn_inherit_server(temp.path()).await?;
    let meta = Some(json!({
        SANDBOX_STATE_META_CAPABILITY: "invalid",
    }));
    let result = session
        .write_stdin_raw_with_meta("1+1", Some(2.0), meta)
        .await?;
    let text = collect_text(&result);
    if backend_unavailable(&text) {
        eprintln!("sandbox_state_updates backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("failed to parse Codex sandbox state metadata"),
        "expected malformed sandbox-state-meta error, got: {text}"
    );
    assert!(
        !text.contains("2"),
        "did not expect successful evaluation, got: {text}"
    );
    assert_eq!(
        result.is_error,
        Some(true),
        "expected malformed metadata on the first worker interaction to set isError, got: {:?}",
        result.is_error
    );
    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_empty_repl_uses_state_meta_when_spawn_needed() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let session = spawn_inherit_server(temp.path()).await?;
    let result = session
        .write_stdin_raw_with_meta("", Some(2.0), Some(workspace_write_meta(temp.path())))
        .await?;
    let text = collect_text(&result);
    if backend_unavailable(&text) {
        eprintln!("sandbox_state_updates backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("<<repl status: idle>>"),
        "expected empty inherit repl call with metadata to return idle status, got: {text}"
    );
    assert!(
        !text.contains("--sandbox inherit requested but no client sandbox state was provided"),
        "did not expect empty inherit repl call with metadata to fail closed, got: {text}"
    );
    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_empty_repl_without_state_meta_sets_is_error() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let session = spawn_inherit_server(temp.path()).await?;
    let result = session.write_stdin_raw_with("", Some(2.0)).await?;
    let text = collect_text(&result);
    if backend_unavailable(&text) {
        eprintln!("sandbox_state_updates backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains(MISSING_INHERITED_STATE_MESSAGE),
        "expected empty inherit repl call without metadata to fail closed, got: {text}"
    );
    assert_eq!(
        result.is_error,
        Some(true),
        "expected empty inherit repl preflight failure to set isError, got: {:?}",
        result.is_error
    );
    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_interrupt_follow_up_ignores_local_meta_errors() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let session = spawn_inherit_server(temp.path()).await?;
    let initial = session
        .write_stdin_raw_with_meta("1+1", Some(2.0), Some(workspace_write_meta(temp.path())))
        .await?;
    let initial_text = common::result_text(&initial);
    if backend_unavailable(&initial_text) {
        eprintln!("sandbox_state_updates backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let interrupt = session
        .write_stdin_raw_with_meta(
            "\u{3}",
            Some(2.0),
            Some(json!({ SANDBOX_STATE_META_CAPABILITY: "invalid" })),
        )
        .await?;
    let interrupt_text = common::result_text(&interrupt);
    assert!(
        !interrupt_text.contains("failed to parse Codex sandbox state metadata"),
        "expected local interrupt follow-up to ignore malformed metadata, got: {interrupt_text}"
    );
    assert!(
        !interrupt_text.contains(MISSING_INHERITED_STATE_MESSAGE),
        "expected local interrupt follow-up to ignore missing inherited metadata checks, got: {interrupt_text}"
    );
    assert!(
        interrupt_text.contains(">")
            || interrupt_text.contains("<<repl status: busy")
            || interrupt_text.contains("<<repl status: idle>>"),
        "expected interrupt follow-up to return local recovery output, got: {interrupt_text}"
    );
    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_metadata_error_preserves_hidden_timeout_bundle() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let session = spawn_inherit_files_server(temp.path(), Vec::new()).await?;
    let first = session
        .write_stdin_raw_with_meta(
            timeout_then_large_completion_code(),
            Some(0.05),
            Some(workspace_write_meta(temp.path())),
        )
        .await?;
    let first_text = common::result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("sandbox_state_updates backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        bundle_transcript_path(&first_text).is_none(),
        "did not expect the first under-threshold timeout reply to disclose a bundle path, got: {first_text:?}"
    );

    tokio::time::sleep(test_delay_ms(600, 900)).await;

    let metadata_error = session
        .write_stdin_raw_with_meta(
            "1+1",
            Some(2.0),
            Some(json!({ SANDBOX_STATE_META_CAPABILITY: "invalid" })),
        )
        .await?;
    let metadata_error_text = common::result_text(&metadata_error);
    assert!(
        metadata_error_text.contains("failed to parse Codex sandbox state metadata"),
        "expected malformed metadata error, got: {metadata_error_text}"
    );

    let mut final_text = String::new();
    for _ in 0..10 {
        let final_poll = session
            .write_stdin_raw_with_meta("", Some(2.0), Some(workspace_write_meta(temp.path())))
            .await?;
        final_text = common::result_text(&final_poll);
        if !final_text.contains("<<repl status: busy") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let transcript_path = bundle_transcript_path(&final_text).unwrap_or_else(|| {
        panic!(
            "expected preserved timeout state to disclose a transcript path on the later poll, got: {final_text:?}"
        )
    });
    let transcript = fs::read_to_string(&transcript_path)?;

    session.cancel().await?;

    assert!(
        transcript.contains("FIRST_START") && transcript.contains("FIRST_END"),
        "expected the preserved timeout bundle to backfill the first timed-out chunk, got: {transcript:?}"
    );
    assert!(
        transcript.contains("SECOND_START") && transcript.contains("SECOND_END"),
        "expected the preserved timeout bundle to include the later completion chunk, got: {transcript:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_active_pager_command_ignores_missing_state_meta() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let session = spawn_inherit_pager_server(temp.path(), 120).await?;
    let initial = session
        .write_stdin_raw_with_meta(
            "line <- paste(rep(\"foo\", 80), collapse = \" \"); for (i in 1:300) cat(sprintf(\"line%04d %s\\n\", i, line))",
            Some(30.0),
            Some(workspace_write_meta(temp.path())),
        )
        .await?;
    let initial_text = common::result_text(&initial);
    if backend_unavailable(&initial_text) {
        eprintln!("sandbox_state_updates backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        initial_text.contains("--More--"),
        "expected pager to activate before local pager command test, got: {initial_text:?}"
    );

    let quit = session.write_stdin_raw_with(":q", Some(30.0)).await?;
    let quit_text = common::result_text(&quit);
    assert!(
        !quit_text.contains(MISSING_INHERITED_STATE_MESSAGE),
        "expected active pager :q to ignore missing inherited metadata, got: {quit_text}"
    );
    assert!(
        !quit_text.contains("failed to parse Codex sandbox state metadata"),
        "expected active pager :q to skip sandbox metadata parsing, got: {quit_text}"
    );
    assert!(
        !quit_text.contains("unexpected ':'"),
        "expected :q to be handled by pager after inherit warm-up, got: {quit_text}"
    );
    assert!(
        quit_text.contains(">"),
        "expected prompt after :q, got: {quit_text}"
    );
    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_active_pager_command_ignores_state_meta_changes() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let session = spawn_inherit_pager_server(temp.path(), 120).await?;
    let initial = session
        .write_stdin_raw_with_meta(
            "line <- paste(rep(\"foo\", 80), collapse = \" \"); for (i in 1:300) cat(sprintf(\"line%04d %s\\n\", i, line))",
            Some(30.0),
            Some(workspace_write_meta(temp.path())),
        )
        .await?;
    let initial_text = common::result_text(&initial);
    if backend_unavailable(&initial_text) {
        eprintln!("sandbox_state_updates backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        initial_text.contains("--More--"),
        "expected pager to activate before local pager command test, got: {initial_text:?}"
    );

    let quit = session
        .write_stdin_raw_with_meta(":q", Some(30.0), Some(full_access_meta(temp.path())))
        .await?;
    let quit_text = common::result_text(&quit);
    assert!(
        !quit_text.contains("sandbox policy changed; new session started"),
        "did not expect active pager command to restart the worker, got: {quit_text}"
    );
    assert!(
        !quit_text.contains("new sandbox policy"),
        "did not expect active pager command to apply the new sandbox policy immediately, got: {quit_text}"
    );
    assert!(
        !quit_text.contains("unexpected ':'"),
        "expected pager quit to stay pager-local, got: {quit_text}"
    );
    assert!(
        quit_text.contains(">"),
        "expected prompt after pager quit, got: {quit_text}"
    );
    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_active_pager_empty_input_ignores_missing_state_meta() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let session = spawn_inherit_pager_server(temp.path(), 120).await?;
    let initial = session
        .write_stdin_raw_with_meta(
            "line <- paste(rep(\"foo\", 80), collapse = \" \"); for (i in 1:300) cat(sprintf(\"line%04d %s\\n\", i, line))",
            Some(30.0),
            Some(workspace_write_meta(temp.path())),
        )
        .await?;
    let initial_text = common::result_text(&initial);
    if backend_unavailable(&initial_text) {
        eprintln!("sandbox_state_updates backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        initial_text.contains("--More--"),
        "expected pager to activate before empty pager command test, got: {initial_text:?}"
    );

    let page_advance = session.write_stdin_raw_with("", Some(30.0)).await?;
    let page_advance_text = common::result_text(&page_advance);
    session.cancel().await?;

    assert!(
        !page_advance_text.contains(MISSING_INHERITED_STATE_MESSAGE),
        "expected active pager empty input to ignore missing inherited metadata, got: {page_advance_text}"
    );
    assert!(
        page_advance_text.contains("--More--") || page_advance_text.contains("(END"),
        "expected active pager empty input to stay in pager mode, got: {page_advance_text}"
    );
    assert_ne!(
        page_advance.is_error,
        Some(true),
        "did not expect active pager empty input to set isError, got: {:?}",
        page_advance.is_error
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_pending_interrupt_tail_with_bad_meta_fails_closed() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let session = spawn_inherit_files_server(temp.path(), Vec::new()).await?;
    let first = session
        .write_stdin_raw_with_meta(
            timeout_then_tail_code(),
            Some(0.05),
            Some(workspace_write_meta(temp.path())),
        )
        .await?;
    let first_text = common::result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("sandbox_state_updates backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    tokio::time::sleep(test_delay_ms(260, 700)).await;

    let interrupt_error = session
        .write_stdin_raw_with_meta(
            "\u{3}cat('AFTER_INTERRUPT\\n')",
            Some(10.0),
            Some(json!({ SANDBOX_STATE_META_CAPABILITY: "invalid" })),
        )
        .await?;
    let interrupt_error_text = common::result_text(&interrupt_error);
    assert!(
        interrupt_error_text.contains("failed to parse Codex sandbox state metadata"),
        "expected malformed metadata error on rejected interrupt follow-up, got: {interrupt_error_text}"
    );
    assert!(
        interrupt_error.is_error == Some(true),
        "expected rejected interrupt follow-up to set isError, got: {:?}",
        interrupt_error.is_error
    );
    assert!(
        !interrupt_error_text.contains("new session started"),
        "did not expect rejected interrupt follow-up to mutate session state, got: {interrupt_error_text}"
    );

    let busy_follow_up = session
        .write_stdin_raw_with_meta("1+1", Some(0.1), Some(workspace_write_meta(temp.path())))
        .await?;
    let busy_follow_up_text = common::result_text(&busy_follow_up);
    assert!(
        busy_follow_up_text.contains("[repl] input discarded while worker busy")
            || busy_follow_up_text.contains("<<repl status: busy"),
        "expected rejected interrupt follow-up to leave the old request running, got: {busy_follow_up_text}"
    );

    let mut final_poll_text = String::new();
    for _ in 0..20 {
        let final_poll = session
            .write_stdin_raw_with_meta("", Some(0.2), Some(workspace_write_meta(temp.path())))
            .await?;
        final_poll_text = common::result_text(&final_poll);
        if !final_poll_text.contains("<<repl status: busy") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    session.cancel().await?;

    assert!(
        final_poll_text.contains("TAIL"),
        "expected the original timed-out request to keep running after rejected interrupt metadata, got: {final_poll_text}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_pending_restart_tail_with_bad_meta_fails_closed() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let session = spawn_inherit_files_server(temp.path(), Vec::new()).await?;
    let first = session
        .write_stdin_raw_with_meta(
            format!("x <- 1\n{}", timeout_then_tail_code()),
            Some(0.05),
            Some(workspace_write_meta(temp.path())),
        )
        .await?;
    let first_text = common::result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("sandbox_state_updates backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    tokio::time::sleep(std::time::Duration::from_millis(260)).await;

    let restart_error = session
        .write_stdin_raw_with_meta(
            "\u{4}cat('AFTER_RESTART\\n')",
            Some(0.1),
            Some(json!({ SANDBOX_STATE_META_CAPABILITY: "invalid" })),
        )
        .await?;
    let restart_error_text = common::result_text(&restart_error);
    assert!(
        restart_error_text.contains("failed to parse Codex sandbox state metadata"),
        "expected malformed metadata error on rejected restart follow-up, got: {restart_error_text}"
    );
    assert!(
        restart_error.is_error == Some(true),
        "expected rejected restart follow-up to set isError, got: {:?}",
        restart_error.is_error
    );
    assert!(
        !restart_error_text.contains("new session started"),
        "did not expect rejected restart follow-up to restart the worker, got: {restart_error_text}"
    );

    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

    let recovery = session
        .write_stdin_raw_with_meta(
            variable_probe_code(),
            Some(1.0),
            Some(workspace_write_meta(temp.path())),
        )
        .await?;
    let recovery_text = common::result_text(&recovery);
    session.cancel().await?;

    assert!(
        recovery_text.contains("X_EXISTS:TRUE"),
        "expected rejected restart follow-up to preserve the running session, got: {recovery_text}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_pending_interrupt_tail_restarts_on_state_change() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let session = spawn_inherit_files_server(temp.path(), Vec::new()).await?;
    let first = session
        .write_stdin_raw_with_meta(
            format!("x <- 1\n{}", timeout_then_tail_code()),
            Some(0.05),
            Some(workspace_write_meta(temp.path())),
        )
        .await?;
    let first_text = common::result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("sandbox_state_updates backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    tokio::time::sleep(test_delay_ms(260, 700)).await;

    let follow_up = session
        .write_stdin_raw_with_meta(
            format!("\u{3}{}", variable_probe_code()),
            Some(1.0),
            Some(full_access_meta(temp.path())),
        )
        .await?;
    let follow_up_text = common::result_text(&follow_up);
    session.cancel().await?;

    assert!(
        follow_up_text.contains("sandbox policy changed; new session started"),
        "expected interrupt tail with changed metadata to restart the worker, got: {follow_up_text}"
    );
    assert!(
        follow_up_text.contains("new sandbox policy"),
        "expected restart notice to include the new sandbox policy, got: {follow_up_text}"
    );
    assert!(
        follow_up_text.contains("X_EXISTS:FALSE"),
        "expected interrupt tail to run in the restarted session, got: {follow_up_text}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_pending_follow_up_restarts_on_new_state_meta() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let session = spawn_inherit_files_server(temp.path(), Vec::new()).await?;
    let first = session
        .write_stdin_raw_with_meta(
            format!("x <- 1\n{}", timeout_then_tail_code()),
            Some(0.05),
            Some(workspace_write_meta(temp.path())),
        )
        .await?;
    let first_text = collect_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("sandbox_state_updates backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    tokio::time::sleep(std::time::Duration::from_millis(260)).await;

    let second = session
        .write_stdin_raw_with_meta(
            variable_probe_code(),
            Some(1.0),
            Some(full_access_meta(temp.path())),
        )
        .await?;
    let second_text = collect_text(&second);
    assert!(
        second_text.contains("sandbox policy changed; new session started"),
        "expected changed metadata to restart the worker instead of preserving the pending request, got: {second_text}"
    );
    assert!(
        second_text.contains("X_EXISTS:FALSE"),
        "expected changed metadata to reset the worker session before running the follow-up, got: {second_text}"
    );
    assert!(
        !second_text.contains("[repl] input discarded while worker busy"),
        "did not expect changed metadata to keep the old busy session alive, got: {second_text}"
    );
    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_pending_empty_poll_ignores_new_state_meta() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let session = spawn_inherit_files_server(temp.path(), Vec::new()).await?;
    let first = session
        .write_stdin_raw_with_meta(
            timeout_then_tail_code(),
            Some(0.05),
            Some(workspace_write_meta(temp.path())),
        )
        .await?;
    let first_text = collect_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("sandbox_state_updates backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    tokio::time::sleep(std::time::Duration::from_millis(260)).await;

    let poll = session
        .write_stdin_raw_with_meta("", Some(2.0), Some(full_access_meta(temp.path())))
        .await?;
    let poll_text = collect_text(&poll);
    assert!(
        poll_text.contains("TAIL"),
        "expected empty poll to continue draining the original request, got: {poll_text}"
    );
    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_pending_empty_poll_ignores_missing_state_meta() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let session = spawn_inherit_files_server(temp.path(), Vec::new()).await?;
    let first = session
        .write_stdin_raw_with_meta(
            timeout_then_tail_code(),
            Some(0.05),
            Some(workspace_write_meta(temp.path())),
        )
        .await?;
    let first_text = collect_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("sandbox_state_updates backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    tokio::time::sleep(std::time::Duration::from_millis(260)).await;

    let poll = session.write_stdin_raw_with("", Some(2.0)).await?;
    let poll_text = collect_text(&poll);
    assert!(
        poll_text.contains("TAIL"),
        "expected empty poll without metadata to continue draining the original request, got: {poll_text}"
    );
    assert!(
        !poll_text.contains(MISSING_INHERITED_STATE_MESSAGE),
        "did not expect empty draining poll without metadata to fail closed, got: {poll_text}"
    );
    assert_ne!(
        poll.is_error,
        Some(true),
        "did not expect empty draining poll without metadata to set isError, got: {:?}",
        poll.is_error
    );
    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_empty_poll_session_end_respawn_uses_current_state_meta() -> TestResult<()>
{
    let _guard = test_guard();
    let scratch = repo_scratch_dir("sandbox-empty-poll-session-end-respawn")?;
    let home_dir = tempdir()?;
    let startup_target = home_dir.path().join("startup-spawn.txt");
    let encoded_target = encode_path(&startup_target)?;
    fs::write(
        home_dir.path().join(".Rprofile"),
        format!(
            "invisible(suppressWarnings(tryCatch({{ writeLines(\"startup\", {encoded_target}) }}, error = function(e) NULL)))\n"
        ),
    )?;

    let session =
        spawn_inherit_files_server(scratch.path(), home_env_vars(home_dir.path())).await?;
    let first = session
        .write_stdin_raw_with_meta(
            "Sys.sleep(0.2)\nquit(\"no\")",
            Some(0.05),
            Some(full_access_meta(scratch.path())),
        )
        .await?;
    let first_text = common::result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("sandbox_state_updates backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let _ = fs::remove_file(&startup_target);
    tokio::time::sleep(std::time::Duration::from_millis(260)).await;

    let drained = session
        .write_stdin_raw_with_meta("", Some(2.0), Some(read_only_meta(scratch.path())))
        .await?;
    let drained_text = common::result_text(&drained);
    assert!(
        drained_text.contains("session ended")
            || drained_text.contains("ipc disconnected while waiting for request completion"),
        "expected timed-out quit request to end the session on the draining poll, got: {drained_text}"
    );

    let prompt = session
        .write_stdin_raw_with_meta("", Some(2.0), Some(read_only_meta(scratch.path())))
        .await?;
    let prompt_text = common::result_text(&prompt);
    session.cancel().await?;

    assert!(
        prompt_text.contains("<<repl status: idle>>"),
        "expected a replacement idle session after draining the ended request, got: {prompt_text}"
    );
    assert!(
        !startup_target.exists(),
        "expected drained-session respawn to honor the current empty-poll read-only metadata"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_empty_poll_respawn_retires_disclosed_timeout_bundle() -> TestResult<()> {
    let _guard = test_guard();
    let scratch = repo_scratch_dir("sandbox-empty-poll-retires-timeout-bundle")?;
    let session = spawn_inherit_files_server(scratch.path(), Vec::new()).await?;
    let first = session
        .write_stdin_raw_with_meta(
            timeout_then_large_completion_and_quit_code(),
            Some(0.05),
            Some(full_access_meta(scratch.path())),
        )
        .await?;
    let first_text = common::result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("sandbox_state_updates backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let mut drained = None;
    for _ in 0..20 {
        let poll = session
            .write_stdin_raw_with_meta("", Some(2.0), Some(full_access_meta(scratch.path())))
            .await?;
        let poll_text = common::result_text(&poll);
        if bundle_transcript_path(&poll_text).is_some()
            && (poll_text.contains("session ended")
                || poll_text.contains("ipc disconnected while waiting for request completion"))
        {
            drained = Some(poll);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let drained = drained.unwrap_or_else(|| {
        panic!("expected draining poll to disclose the settled timeout transcript before respawn")
    });
    let drained_text = common::result_text(&drained);
    let first_transcript_path = bundle_transcript_path(&drained_text).unwrap_or_else(|| {
        panic!(
            "expected draining poll to disclose the settled timeout transcript, got: {drained_text:?}"
        )
    });
    let first_transcript_before = fs::read_to_string(&first_transcript_path)?;
    assert!(
        first_transcript_before.contains("SECOND_START")
            && first_transcript_before.contains("SECOND_END"),
        "expected the disclosed timeout transcript to contain the settled completion chunk, got: {first_transcript_before:?}"
    );

    let respawned = session
        .write_stdin_raw_with_meta("", Some(2.0), Some(read_only_meta(scratch.path())))
        .await?;
    let respawned_text = common::result_text(&respawned);
    assert!(
        respawned_text.contains("<<repl status: idle>>"),
        "expected the empty poll to respawn the ended session before the fresh follow-up, got: {respawned_text:?}"
    );

    let follow_up = session
        .write_stdin_raw_with_meta(
            oversized_follow_up_code("FOLLOW_UP"),
            Some(10.0),
            Some(read_only_meta(scratch.path())),
        )
        .await?;
    let follow_up_text = common::result_text(&follow_up);
    let follow_up_transcript_path = bundle_transcript_path(&follow_up_text);
    let first_transcript_after = fs::read_to_string(&first_transcript_path)?;
    let follow_up_transcript = follow_up_transcript_path
        .as_ref()
        .map(fs::read_to_string)
        .transpose()?
        .unwrap_or_default();

    session.cancel().await?;

    if let Some(follow_up_transcript_path) = follow_up_transcript_path {
        assert_ne!(
            first_transcript_path, follow_up_transcript_path,
            "expected the empty-poll respawn to stop reusing the old disclosed timeout bundle"
        );
    }
    assert!(
        !first_transcript_after.contains("FOLLOW_UP_START"),
        "did not expect the fresh post-respawn output in the prior disclosed timeout transcript: {first_transcript_after:?}"
    );
    assert!(
        follow_up_text.contains("FOLLOW_UP_START")
            || follow_up_transcript.contains("FOLLOW_UP_START"),
        "expected the fresh post-respawn output to stay with the new turn, got reply {follow_up_text:?} and transcript {follow_up_transcript:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_empty_poll_session_end_without_state_meta_does_not_respawn_stale_worker()
-> TestResult<()> {
    let _guard = test_guard();
    let scratch = repo_scratch_dir("sandbox-empty-poll-session-end-missing-meta")?;
    let home_dir = tempdir()?;
    let startup_target = home_dir.path().join("startup-spawn.txt");
    let encoded_target = encode_path(&startup_target)?;
    fs::write(
        home_dir.path().join(".Rprofile"),
        format!(
            "invisible(suppressWarnings(tryCatch({{ writeLines(\"startup\", {encoded_target}) }}, error = function(e) NULL)))\n"
        ),
    )?;

    let session =
        spawn_inherit_files_server(scratch.path(), home_env_vars(home_dir.path())).await?;
    let first = session
        .write_stdin_raw_with_meta(
            "Sys.sleep(0.2)\nquit(\"no\")",
            Some(0.05),
            Some(full_access_meta(scratch.path())),
        )
        .await?;
    let first_text = common::result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("sandbox_state_updates backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let _ = fs::remove_file(&startup_target);
    tokio::time::sleep(std::time::Duration::from_millis(260)).await;

    let drained = session.write_stdin_raw_with("", Some(2.0)).await?;
    let drained_text = common::result_text(&drained);
    assert!(
        drained_text.contains("session ended")
            || drained_text.contains("ipc disconnected while waiting for request completion"),
        "expected timed-out quit request to end the session on the draining poll, got: {drained_text}"
    );
    assert!(
        !startup_target.exists(),
        "did not expect a draining poll without metadata to respawn a stale worker"
    );

    let prompt = session.write_stdin_raw_with("", Some(2.0)).await?;
    let prompt_text = common::result_text(&prompt);
    session.cancel().await?;

    assert!(
        prompt_text.contains(MISSING_INHERITED_STATE_MESSAGE),
        "expected the next empty poll to fail closed once a new worker spawn was required, got: {prompt_text}"
    );
    assert_eq!(
        prompt.is_error,
        Some(true),
        "expected the spawn-needed follow-up poll without metadata to set isError, got: {:?}",
        prompt.is_error
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_bare_interrupt_after_session_end_uses_current_state_meta() -> TestResult<()>
{
    let _guard = test_guard();
    let scratch = repo_scratch_dir("sandbox-bare-interrupt-session-end-meta")?;
    let home_dir = tempdir()?;
    let startup_target = home_dir.path().join("startup-spawn.txt");
    let encoded_target = encode_path(&startup_target)?;
    fs::write(
        home_dir.path().join(".Rprofile"),
        format!(
            "invisible(suppressWarnings(tryCatch({{ writeLines(\"startup\", {encoded_target}) }}, error = function(e) NULL)))\n"
        ),
    )?;

    let session =
        spawn_inherit_files_server(scratch.path(), home_env_vars(home_dir.path())).await?;
    let first = session
        .write_stdin_raw_with_meta(
            "Sys.sleep(0.2)\nquit(\"no\")",
            Some(0.05),
            Some(full_access_meta(scratch.path())),
        )
        .await?;
    let first_text = common::result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("sandbox_state_updates backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let _ = fs::remove_file(&startup_target);
    tokio::time::sleep(std::time::Duration::from_millis(260)).await;

    let interrupt = session
        .write_stdin_raw_with_meta("\u{3}", Some(2.0), Some(read_only_meta(scratch.path())))
        .await?;
    let interrupt_text = common::result_text(&interrupt);
    assert!(
        !interrupt_text.contains(MISSING_INHERITED_STATE_MESSAGE),
        "did not expect bare interrupt with current metadata to fail closed, got: {interrupt_text}"
    );

    let prompt = session.write_stdin_raw_with("", Some(2.0)).await?;
    let prompt_text = common::result_text(&prompt);
    session.cancel().await?;

    assert!(
        prompt_text.contains("<<repl status: idle>>"),
        "expected bare interrupt to let the session respawn under the current metadata, got: {prompt_text}"
    );
    assert!(
        !startup_target.exists(),
        "expected bare interrupt respawn to honor the current read-only metadata"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_bare_interrupt_after_session_end_without_state_meta_does_not_respawn_stale_worker()
-> TestResult<()> {
    let _guard = test_guard();
    let scratch = repo_scratch_dir("sandbox-bare-interrupt-session-end-missing-meta")?;
    let home_dir = tempdir()?;
    let startup_target = home_dir.path().join("startup-spawn.txt");
    let encoded_target = encode_path(&startup_target)?;
    fs::write(
        home_dir.path().join(".Rprofile"),
        format!(
            "invisible(suppressWarnings(tryCatch({{ writeLines(\"startup\", {encoded_target}) }}, error = function(e) NULL)))\n"
        ),
    )?;

    let session =
        spawn_inherit_files_server(scratch.path(), home_env_vars(home_dir.path())).await?;
    let first = session
        .write_stdin_raw_with_meta(
            "Sys.sleep(0.2)\nquit(\"no\")",
            Some(0.05),
            Some(full_access_meta(scratch.path())),
        )
        .await?;
    let first_text = common::result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("sandbox_state_updates backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let _ = fs::remove_file(&startup_target);
    tokio::time::sleep(std::time::Duration::from_millis(260)).await;

    let interrupt = session.write_stdin_raw_with("\u{3}", Some(2.0)).await?;
    let interrupt_text = common::result_text(&interrupt);
    assert!(
        !interrupt_text.contains(MISSING_INHERITED_STATE_MESSAGE),
        "did not expect bare interrupt without metadata to fail closed, got: {interrupt_text}"
    );
    assert!(
        !startup_target.exists(),
        "did not expect bare interrupt without metadata to respawn a stale worker"
    );

    let prompt = session.write_stdin_raw_with("", Some(2.0)).await?;
    let prompt_text = common::result_text(&prompt);
    session.cancel().await?;

    assert!(
        prompt_text.contains(MISSING_INHERITED_STATE_MESSAGE),
        "expected the next empty poll to fail closed once a new worker spawn was required, got: {prompt_text}"
    );
    assert_eq!(
        prompt.is_error,
        Some(true),
        "expected the spawn-needed poll after bare interrupt without metadata to set isError, got: {:?}",
        prompt.is_error
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_applies_new_state_meta_after_timed_out_request_settles() -> TestResult<()>
{
    let _guard = test_guard();
    let scratch = repo_scratch_dir("sandbox-timeout-settle-fresh-call")?;
    let target = scratch.path().join("fresh-call-write.txt");
    let session = spawn_inherit_files_server(scratch.path(), Vec::new()).await?;
    let first = session
        .write_stdin_raw_with_meta(
            timeout_then_done_code(),
            Some(0.05),
            Some(read_only_meta(scratch.path())),
        )
        .await?;
    let first_text = collect_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("sandbox_state_updates backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    tokio::time::sleep(std::time::Duration::from_millis(260)).await;

    let second = session
        .write_stdin_raw_with_meta(
            write_file_code(&target)?,
            Some(10.0),
            Some(workspace_write_meta(scratch.path())),
        )
        .await?;
    let second_text = collect_text(&second);
    assert!(
        second_text.contains("WRITE_OK"),
        "expected fresh follow-up call to apply current sandbox metadata, got: {second_text}"
    );
    assert!(
        !second_text.contains("WRITE_ERROR:"),
        "did not expect stale settled timeout state to keep the old sandbox, got: {second_text}"
    );
    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_metadata_change_keeps_settled_timeout_output() -> TestResult<()> {
    let _guard = test_guard();
    let scratch = repo_scratch_dir("sandbox-timeout-tail-across-state-change")?;
    let session = spawn_inherit_files_server(scratch.path(), Vec::new()).await?;
    let first = session
        .write_stdin_raw_with_meta(
            timeout_then_tail_code(),
            Some(0.05),
            Some(read_only_meta(scratch.path())),
        )
        .await?;
    let first_text = collect_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("sandbox_state_updates backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        !first_text.contains("TAIL"),
        "expected the late completion chunk to remain detached from the timeout reply, got: {first_text}"
    );
    tokio::time::sleep(test_delay_ms(1400, 1800)).await;

    let second = session
        .write_stdin_raw_with_meta(
            "1+1",
            Some(10.0),
            Some(workspace_write_meta(scratch.path())),
        )
        .await?;
    let second_text = collect_text(&second);
    assert!(
        second_text.contains("TAIL"),
        "expected settled timeout output to survive sandbox respawn, got: {second_text}"
    );
    assert!(
        second_text.contains("[1] 2"),
        "expected the fresh call to still execute after the preserved timeout tail, got: {second_text}"
    );
    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_metadata_change_keeps_timeout_bundle_output() -> TestResult<()> {
    let _guard = test_guard();
    let scratch = repo_scratch_dir("sandbox-timeout-bundle-across-state-change")?;
    let session = spawn_inherit_files_server(scratch.path(), Vec::new()).await?;
    let first = session
        .write_stdin_raw_with_meta(
            timeout_then_large_completion_code(),
            Some(0.05),
            Some(read_only_meta(scratch.path())),
        )
        .await?;
    let first_text = common::result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("sandbox_state_updates backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        bundle_transcript_path(&first_text).is_none(),
        "did not expect the initial timeout reply to disclose a transcript path, got: {first_text:?}"
    );

    tokio::time::sleep(test_delay_ms(900, 1200)).await;

    let second = session
        .write_stdin_raw_with_meta(
            "1+1",
            Some(10.0),
            Some(workspace_write_meta(scratch.path())),
        )
        .await?;
    let second_text = common::result_text(&second);
    let transcript_path = bundle_transcript_path(&second_text).unwrap_or_else(|| {
        panic!(
            "expected the metadata-changing follow-up to preserve and disclose the timeout transcript, got: {second_text:?}"
        )
    });
    let transcript = fs::read_to_string(&transcript_path)?;

    session.cancel().await?;

    assert!(
        transcript.contains("FIRST_START") && transcript.contains("FIRST_END"),
        "expected the preserved timeout transcript to include the first timed-out chunk, got: {transcript:?}"
    );
    assert!(
        transcript.contains("SECOND_START") && transcript.contains("SECOND_END"),
        "expected the preserved timeout transcript to include the settled completion chunk, got: {transcript:?}"
    );
    assert!(
        second_text.contains("[1] 2") || transcript.contains("[1] 2"),
        "expected the fresh follow-up result to execute after preserving the timeout transcript, got reply {second_text:?} and transcript {transcript:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_restart_tail_after_sandbox_respawn_keeps_timeout_bundle_output()
-> TestResult<()> {
    let _guard = test_guard();
    let scratch = repo_scratch_dir("sandbox-timeout-bundle-across-restart-tail-respawn")?;
    let session = spawn_inherit_files_server(scratch.path(), Vec::new()).await?;
    let first = session
        .write_stdin_raw_with_meta(
            timeout_then_large_completion_code(),
            Some(0.05),
            Some(read_only_meta(scratch.path())),
        )
        .await?;
    let first_text = common::result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("sandbox_state_updates backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        bundle_transcript_path(&first_text).is_none(),
        "did not expect the initial timeout reply to disclose a transcript path, got: {first_text:?}"
    );

    tokio::time::sleep(test_delay_ms(900, 1200)).await;

    let second = session
        .write_stdin_raw_with_meta(
            "\u{4}1+1",
            Some(10.0),
            Some(workspace_write_meta(scratch.path())),
        )
        .await?;
    let second_text = common::result_text(&second);
    let transcript_path = bundle_transcript_path(&second_text).unwrap_or_else(|| {
        panic!(
            "expected the sandbox-respawned restart tail to preserve and disclose the timeout transcript, got: {second_text:?}"
        )
    });
    let transcript = fs::read_to_string(&transcript_path)?;

    session.cancel().await?;

    assert!(
        transcript.contains("FIRST_START") && transcript.contains("FIRST_END"),
        "expected the preserved timeout transcript to include the first timed-out chunk, got: {transcript:?}"
    );
    assert!(
        transcript.contains("SECOND_START") && transcript.contains("SECOND_END"),
        "expected the preserved timeout transcript to include the settled completion chunk, got: {transcript:?}"
    );
    assert!(
        second_text.contains("[1] 2") || transcript.contains("[1] 2"),
        "expected the restart tail to execute after preserving the timeout transcript, got reply {second_text:?} and transcript {transcript:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_disclosed_timeout_bundle_is_retired_on_state_change() -> TestResult<()> {
    let _guard = test_guard();
    let scratch = repo_scratch_dir("sandbox-disclosed-timeout-bundle-respawn")?;
    let session = spawn_inherit_files_server(scratch.path(), Vec::new()).await?;
    let first = session
        .write_stdin_raw_with_meta(
            timeout_then_large_completion_code(),
            Some(0.05),
            Some(read_only_meta(scratch.path())),
        )
        .await?;
    let first_text = common::result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("sandbox_state_updates backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let first_transcript_path = loop {
        let poll = session
            .write_stdin_raw_with_meta("", Some(2.0), Some(read_only_meta(scratch.path())))
            .await?;
        let first_poll_text = common::result_text(&poll);
        if let Some(path) = bundle_transcript_path(&first_poll_text) {
            break path;
        }
        if !first_poll_text.contains("<<repl status: busy") {
            panic!(
                "expected the first timeout flow to disclose a transcript path, got: {first_poll_text:?}"
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    let first_transcript_before = fs::read_to_string(&first_transcript_path)?;
    assert!(
        first_transcript_before.contains("SECOND_START")
            && first_transcript_before.contains("SECOND_END"),
        "expected the first disclosed timeout transcript to contain the late completion tail, got: {first_transcript_before:?}"
    );

    let second = session
        .write_stdin_raw_with_meta(
            oversized_follow_up_code("FOLLOW_UP"),
            Some(10.0),
            Some(workspace_write_meta(scratch.path())),
        )
        .await?;
    let second_text = common::result_text(&second);
    let first_transcript_after = fs::read_to_string(&first_transcript_path)?;
    let second_transcript_path = bundle_transcript_path(&second_text);
    let second_transcript = second_transcript_path
        .as_ref()
        .map(fs::read_to_string)
        .transpose()?
        .unwrap_or_default();

    session.cancel().await?;

    if let Some(second_transcript_path) = second_transcript_path {
        assert_ne!(
            first_transcript_path, second_transcript_path,
            "expected the respawned follow-up turn to get a fresh transcript path"
        );
    }
    assert!(
        !first_transcript_after.contains("FOLLOW_UP_START"),
        "did not expect respawned follow-up output in the earlier disclosed timeout transcript: {first_transcript_after:?}"
    );
    assert!(
        second_text.contains("FOLLOW_UP_START")
            || (second_transcript.contains("FOLLOW_UP_START")
                && second_transcript.contains("FOLLOW_UP_END")),
        "expected the respawned follow-up output to stay with the fresh turn, got reply {second_text:?} and transcript {second_transcript:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_busy_follow_up_never_executes_under_stale_sandbox() -> TestResult<()> {
    let _guard = test_guard();
    for delay_ms in [90_u64, 100, 110, 120, 130, 140, 150, 160] {
        let scratch = repo_scratch_dir(&format!("sandbox-busy-recheck-{delay_ms}"))?;
        let target = scratch
            .path()
            .join(format!("stale-follow-up-{delay_ms}.txt"));
        let session = spawn_inherit_files_server(scratch.path(), Vec::new()).await?;
        let first = session
            .write_stdin_raw_with_meta(
                timeout_then_done_code_after(0.22),
                Some(0.05),
                Some(workspace_write_meta(scratch.path())),
            )
            .await?;
        let first_text = collect_text(&first);
        if backend_unavailable(&first_text) {
            eprintln!("sandbox_state_updates backend unavailable in this environment; skipping");
            session.cancel().await?;
            return Ok(());
        }

        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;

        let second = session
            .write_stdin_raw_with_meta(
                write_file_code(&target)?,
                Some(10.0),
                Some(read_only_meta(scratch.path())),
            )
            .await?;
        let second_text = collect_text(&second);
        assert!(
            !second_text.contains("WRITE_OK"),
            "did not expect stale sandbox execution after busy follow-up at delay {delay_ms}ms, got: {second_text}"
        );
        assert!(
            !target.exists(),
            "did not expect follow-up to create {} at delay {delay_ms}ms",
            target.display()
        );
        session.cancel().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_restart_follow_up_applies_current_state_meta() -> TestResult<()> {
    let _guard = test_guard();
    let scratch = repo_scratch_dir("sandbox-restart-follow-up-state-meta")?;
    let target = scratch.path().join("restart-follow-up-write.txt");
    let session = spawn_inherit_files_server(scratch.path(), Vec::new()).await?;
    let first = session
        .write_stdin_raw_with_meta(
            timeout_then_tail_code(),
            Some(0.05),
            Some(workspace_write_meta(scratch.path())),
        )
        .await?;
    let first_text = collect_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("sandbox_state_updates backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    tokio::time::sleep(std::time::Duration::from_millis(260)).await;

    let second = session
        .write_stdin_raw_with_meta(
            format!("\u{4}{}", write_file_code(&target)?),
            Some(10.0),
            Some(read_only_meta(scratch.path())),
        )
        .await?;
    let second_text = collect_text(&second);
    assert!(
        second_text.contains("new session started"),
        "expected restart follow-up reply to include restart notice, got: {second_text}"
    );
    assert!(
        !second_text.contains("WRITE_OK"),
        "did not expect restart follow-up to run under stale workspace-write metadata, got: {second_text}"
    );
    assert!(
        !target.exists(),
        "did not expect restart follow-up to create {}",
        target.display()
    );
    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_bare_restart_stays_restart_after_sandbox_respawn() -> TestResult<()> {
    let _guard = test_guard();
    let scratch = repo_scratch_dir("sandbox-bare-restart-after-respawn")?;
    let session = spawn_inherit_files_server(scratch.path(), Vec::new()).await?;
    let first = session
        .write_stdin_raw_with_meta(
            timeout_then_tail_code(),
            Some(0.05),
            Some(workspace_write_meta(scratch.path())),
        )
        .await?;
    let first_text = common::result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("sandbox_state_updates backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    tokio::time::sleep(test_delay_ms(260, 700)).await;

    let restart = session
        .write_stdin_raw_with_meta("\u{4}", Some(1.0), Some(read_only_meta(scratch.path())))
        .await?;
    let restart_text = common::result_text(&restart);
    session.cancel().await?;

    assert!(
        restart_text.contains("new session started"),
        "expected bare Ctrl-D after sandbox respawn to remain an explicit restart, got: {restart_text}"
    );
    assert!(
        !restart_text.contains("MID") && !restart_text.contains("TAIL"),
        "did not expect bare Ctrl-D after sandbox respawn to drain preserved timeout output, got: {restart_text}"
    );
    assert!(
        !restart_text.contains("<<repl status: idle>>"),
        "did not expect bare Ctrl-D after sandbox respawn to degrade into an empty poll, got: {restart_text}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_workspace_write_meta_allows_write_in_cwd() -> TestResult<()> {
    let _guard = test_guard();
    let scratch = repo_scratch_dir("sandbox-workspace-write")?;
    let target = scratch.path().join("allowed.txt");
    let session = spawn_inherit_server(scratch.path()).await?;
    let result = session
        .write_stdin_raw_with_meta(
            write_file_code(&target)?,
            Some(10.0),
            Some(workspace_write_meta(scratch.path())),
        )
        .await?;
    let text = collect_text(&result);
    if backend_unavailable(&text) {
        eprintln!("sandbox_state_updates backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("WRITE_OK"),
        "expected write in cwd to succeed, got: {text}"
    );
    assert!(
        !text.contains("WRITE_ERROR:"),
        "workspace-write unexpectedly blocked write in cwd: {text}"
    );
    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_read_only_meta_blocks_write_in_cwd() -> TestResult<()> {
    let _guard = test_guard();
    let scratch = repo_scratch_dir("sandbox-read-only")?;
    let target = scratch.path().join("blocked.txt");
    let session = spawn_inherit_server(scratch.path()).await?;
    let result = session
        .write_stdin_raw_with_meta(
            write_file_code(&target)?,
            Some(10.0),
            Some(read_only_meta(scratch.path())),
        )
        .await?;
    let text = collect_text(&result);
    if backend_unavailable(&text) {
        eprintln!("sandbox_state_updates backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("WRITE_ERROR:"),
        "expected read-only metadata to block write in cwd, got: {text}"
    );
    assert!(
        !text.contains("WRITE_OK"),
        "did not expect read-only metadata to allow write in cwd, got: {text}"
    );
    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_full_access_meta_allows_write_outside_cwd() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let target = outside_workspace_target("full-access")?;
    let _ = std::fs::remove_file(&target);
    let session = spawn_inherit_server(temp.path()).await?;
    let result = session
        .write_stdin_raw_with_meta(
            write_file_code(&target)?,
            Some(10.0),
            Some(full_access_meta(temp.path())),
        )
        .await?;
    let text = collect_text(&result);
    if backend_unavailable(&text) {
        eprintln!("sandbox_state_updates backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("WRITE_OK"),
        "expected full access to allow write outside cwd, got: {text}"
    );
    assert!(
        !text.contains("WRITE_ERROR:"),
        "full access unexpectedly blocked outside write: {text}"
    );
    let _ = std::fs::remove_file(&target);
    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_workspace_write_mode_ignores_codex_sandbox_state_meta() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let target = outside_workspace_target("ignored-meta")?;
    let _ = std::fs::remove_file(&target);
    let session = common::spawn_server_with_args_env_and_cwd(
        vec!["--sandbox".to_string(), "workspace-write".to_string()],
        Vec::new(),
        Some(temp.path().to_path_buf()),
    )
    .await?;
    let result = session
        .write_stdin_raw_with_meta(
            write_file_code(&target)?,
            Some(10.0),
            Some(full_access_meta(temp.path())),
        )
        .await?;
    let text = collect_text(&result);
    if backend_unavailable(&text) {
        eprintln!("sandbox_state_updates backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("WRITE_ERROR:"),
        "expected explicit workspace-write mode to ignore full-access metadata, got: {text}"
    );
    assert!(
        !text.contains("WRITE_OK"),
        "did not expect explicit workspace-write mode to allow outside write, got: {text}"
    );
    let _ = std::fs::remove_file(&target);
    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_restarts_worker_when_state_meta_changes() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let session = spawn_inherit_server(temp.path()).await?;
    let first = session
        .write_stdin_raw_with_meta(
            r#"x <- 42; cat("SET_OK\n")"#,
            Some(10.0),
            Some(workspace_write_meta(temp.path())),
        )
        .await?;
    let first_text = collect_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("sandbox_state_updates backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        first_text.contains("SET_OK"),
        "expected setup write, got: {first_text}"
    );

    let second = session
        .write_stdin_raw_with_meta(
            variable_probe_code(),
            Some(10.0),
            Some(full_access_meta(temp.path())),
        )
        .await?;
    let second_text = collect_text(&second);
    assert!(
        second_text.contains("X_EXISTS:FALSE"),
        "expected sandbox state change to restart the worker session, got: {second_text}"
    );
    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_without_state_meta_fails_on_repl_reset() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let session = spawn_inherit_server(temp.path()).await?;
    let result = session.call_tool_raw("repl_reset", json!({})).await?;
    let text = collect_text(&result);
    if backend_unavailable(&text) {
        eprintln!("sandbox_state_updates backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("--sandbox inherit requested but no client sandbox state was provided"),
        "expected missing sandbox-state-meta error, got: {text}"
    );
    assert_eq!(
        result.is_error,
        Some(true),
        "expected repl_reset without required metadata to set isError, got: {:?}",
        result.is_error
    );
    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_repl_reset_uses_state_meta() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let session = spawn_inherit_server(temp.path()).await?;
    let result = session
        .call_tool_raw_with_meta(
            "repl_reset",
            json!({}),
            Some(workspace_write_meta(temp.path())),
        )
        .await?;
    let text = collect_text(&result);
    if backend_unavailable(&text) {
        eprintln!("sandbox_state_updates backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("new session started"),
        "expected repl_reset with sandbox metadata to succeed, got: {text}"
    );
    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_repl_reset_does_not_spawn_worker_just_to_stage_state() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let debug_dir = temp.path().join("debug");
    let session = spawn_inherit_server_with_env(
        temp.path(),
        vec![(
            "MCP_REPL_DEBUG_DIR".to_string(),
            debug_dir.to_string_lossy().to_string(),
        )],
    )
    .await?;
    let result = session
        .call_tool_raw_with_meta(
            "repl_reset",
            json!({}),
            Some(workspace_write_meta(temp.path())),
        )
        .await?;
    let text = collect_text(&result);
    assert!(
        text.contains("new session started"),
        "expected repl_reset with sandbox metadata to succeed, got: {text}"
    );
    session.cancel().await?;

    let events = latest_debug_events(&debug_dir)?;
    let saw_restart = events
        .iter()
        .any(|entry| entry["event"] == "worker_restart_begin");
    assert!(saw_restart, "expected repl_reset to emit a restart event");
    let saw_spawn = events
        .iter()
        .any(|entry| entry["event"] == "worker_spawn_begin");
    assert!(
        !saw_spawn,
        "did not expect repl_reset to spawn a worker just to stage sandbox metadata"
    );
    Ok(())
}
