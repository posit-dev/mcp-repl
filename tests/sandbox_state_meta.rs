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

fn empty_or_blank_stderr(text: &str) -> bool {
    text.is_empty()
        || text
            .strip_prefix("stderr:")
            .is_some_and(|stderr| stderr.trim().is_empty())
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

fn sandbox_cwd_uri(sandbox_cwd: &Path) -> String {
    url::Url::from_file_path(sandbox_cwd)
        .map(|url| url.to_string())
        .unwrap_or_else(|_| panic!("failed to convert {} to file URI", sandbox_cwd.display()))
}

fn codex_sandbox_state_meta(
    permission_profile: Value,
    sandbox_cwd: &Path,
    use_legacy_landlock: bool,
) -> Value {
    let sandbox_cwd = sandbox_cwd_uri(sandbox_cwd);
    json!({
        SANDBOX_STATE_META_CAPABILITY: {
            "permissionProfile": permission_profile,
            "sandboxCwd": sandbox_cwd,
            "useLegacyLandlock": use_legacy_landlock,
            "codexLinuxSandboxExe": linux_sandbox_exe_value(use_legacy_landlock),
        }
    })
}

fn root_read_entry() -> Value {
    json!({
        "path": {
            "type": "special",
            "value": { "kind": "root" }
        },
        "access": "read"
    })
}

fn special_entry(kind: &str, access: &str) -> Value {
    json!({
        "path": {
            "type": "special",
            "value": { "kind": kind }
        },
        "access": access
    })
}

fn protected_project_entry(subpath: &str) -> Value {
    json!({
        "path": {
            "type": "special",
            "value": {
                "kind": "project_roots",
                "subpath": subpath
            }
        },
        "access": "read"
    })
}

fn managed_profile(entries: Vec<Value>, network: &str) -> Value {
    json!({
        "type": "managed",
        "file_system": {
            "type": "restricted",
            "entries": entries,
        },
        "network": network,
    })
}

fn managed_unrestricted_profile(network: &str) -> Value {
    json!({
        "type": "managed",
        "file_system": {
            "type": "unrestricted",
        },
        "network": network,
    })
}

fn workspace_write_profile() -> Value {
    managed_profile(
        vec![
            root_read_entry(),
            special_entry("project_roots", "write"),
            special_entry("tmpdir", "write"),
            special_entry("slash_tmp", "write"),
            protected_project_entry(".git"),
            protected_project_entry(".agents"),
            protected_project_entry(".codex"),
        ],
        "restricted",
    )
}

fn workspace_write_meta(sandbox_cwd: &Path) -> Value {
    codex_sandbox_state_meta(
        workspace_write_profile(),
        sandbox_cwd,
        /*use_legacy_landlock*/ false,
    )
}

fn workspace_write_with_glob_deny_meta(sandbox_cwd: &Path, pattern: &str) -> Value {
    let mut entries = vec![
        root_read_entry(),
        special_entry("project_roots", "write"),
        special_entry("tmpdir", "write"),
        special_entry("slash_tmp", "write"),
        protected_project_entry(".git"),
        protected_project_entry(".agents"),
        protected_project_entry(".codex"),
    ];
    entries.push(json!({
        "path": {
            "type": "glob_pattern",
            "pattern": pattern
        },
        "access": "deny"
    }));
    codex_sandbox_state_meta(managed_profile(entries, "restricted"), sandbox_cwd, false)
}

fn workspace_write_with_path_deny_meta(sandbox_cwd: &Path, denied_path: &Path) -> Value {
    let mut entries = vec![
        root_read_entry(),
        special_entry("project_roots", "write"),
        special_entry("tmpdir", "write"),
        special_entry("slash_tmp", "write"),
        protected_project_entry(".git"),
        protected_project_entry(".agents"),
        protected_project_entry(".codex"),
    ];
    entries.push(json!({
        "path": {
            "type": "path",
            "path": denied_path
        },
        "access": "deny"
    }));
    codex_sandbox_state_meta(managed_profile(entries, "restricted"), sandbox_cwd, false)
}

fn workspace_write_with_path_deny_and_child_write_meta(
    sandbox_cwd: &Path,
    denied_path: &Path,
    writable_child: &Path,
) -> Value {
    let mut entries = vec![
        root_read_entry(),
        special_entry("project_roots", "write"),
        special_entry("tmpdir", "write"),
        special_entry("slash_tmp", "write"),
        protected_project_entry(".git"),
        protected_project_entry(".agents"),
        protected_project_entry(".codex"),
    ];
    entries.push(json!({
        "path": {
            "type": "path",
            "path": denied_path
        },
        "access": "deny"
    }));
    entries.push(json!({
        "path": {
            "type": "path",
            "path": writable_child
        },
        "access": "write"
    }));
    codex_sandbox_state_meta(managed_profile(entries, "restricted"), sandbox_cwd, false)
}

fn explicit_path_write_meta(sandbox_cwd: &Path, writable_root: &Path) -> Value {
    codex_sandbox_state_meta(
        managed_profile(
            vec![
                root_read_entry(),
                special_entry("tmpdir", "write"),
                special_entry("slash_tmp", "write"),
                json!({
                    "path": {
                        "type": "path",
                        "path": writable_root
                    },
                    "access": "write"
                }),
            ],
            "restricted",
        ),
        sandbox_cwd,
        false,
    )
}

fn workspace_write_restricted_read_meta(sandbox_cwd: &Path) -> Value {
    codex_sandbox_state_meta(
        managed_profile(
            vec![
                root_read_entry(),
                special_entry("project_roots", "write"),
                special_entry("tmpdir", "write"),
                special_entry("slash_tmp", "write"),
                json!({
                    "path": {
                        "type": "special",
                        "value": {
                            "kind": "project_roots",
                            "subpath": "restricted"
                        }
                    },
                    "access": "read"
                }),
            ],
            "restricted",
        ),
        sandbox_cwd,
        /*use_legacy_landlock*/ false,
    )
}

fn read_only_meta(sandbox_cwd: &Path) -> Value {
    codex_sandbox_state_meta(
        managed_profile(vec![root_read_entry()], "restricted"),
        sandbox_cwd,
        false,
    )
}

fn read_only_with_unknown_special_meta(sandbox_cwd: &Path) -> Value {
    codex_sandbox_state_meta(
        managed_profile(
            vec![
                root_read_entry(),
                json!({
                    "path": {
                        "type": "special",
                        "value": {
                            "kind": "unknown",
                            "path": ":future_special_path",
                            "subpath": null
                        }
                    },
                    "access": "write"
                }),
            ],
            "restricted",
        ),
        sandbox_cwd,
        false,
    )
}

fn minimal_read_meta(sandbox_cwd: &Path) -> Value {
    codex_sandbox_state_meta(
        managed_profile(
            vec![
                special_entry("minimal", "read"),
                special_entry("project_roots", "read"),
                special_entry("tmpdir", "write"),
            ],
            "restricted",
        ),
        sandbox_cwd,
        false,
    )
}

fn minimal_read_with_path_deny_meta(sandbox_cwd: &Path, denied_path: &Path) -> Value {
    codex_sandbox_state_meta(
        managed_profile(
            vec![
                special_entry("minimal", "read"),
                special_entry("project_roots", "read"),
                special_entry("tmpdir", "write"),
                json!({
                    "path": {
                        "type": "path",
                        "path": denied_path
                    },
                    "access": "deny"
                }),
            ],
            "restricted",
        ),
        sandbox_cwd,
        false,
    )
}

fn read_only_restricted_access_meta(sandbox_cwd: &Path) -> Value {
    codex_sandbox_state_meta(
        managed_profile(Vec::new(), "restricted"),
        sandbox_cwd,
        false,
    )
}

fn read_only_network_access_meta(sandbox_cwd: &Path) -> Value {
    codex_sandbox_state_meta(
        managed_profile(vec![root_read_entry()], "enabled"),
        sandbox_cwd,
        false,
    )
}

fn full_write_network_restricted_meta(sandbox_cwd: &Path) -> Value {
    codex_sandbox_state_meta(
        managed_unrestricted_profile("restricted"),
        sandbox_cwd,
        false,
    )
}

fn root_write_network_restricted_meta(sandbox_cwd: &Path) -> Value {
    codex_sandbox_state_meta(
        managed_profile(
            vec![root_read_entry(), special_entry("root", "write")],
            "restricted",
        ),
        sandbox_cwd,
        false,
    )
}

fn full_access_meta(sandbox_cwd: &Path) -> Value {
    codex_sandbox_state_meta(json!({"type": "disabled"}), sandbox_cwd, false)
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

fn home_scratch_dir(label: &str) -> TestResult<TempDir> {
    let base = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
        .ok_or_else(|| "missing HOME/USERPROFILE for sandbox test home".to_string())?;
    Ok(Builder::new()
        .prefix(&format!(".tmp-{label}-"))
        .tempdir_in(base)?)
}

fn repo_scratch_dir(label: &str) -> TestResult<TempDir> {
    Ok(Builder::new()
        .prefix(&format!(".tmp-{label}-"))
        .tempdir_in(common::checkout_test_temp_parent("sandbox-state-meta")?)?)
}

#[test]
fn repo_scratch_dir_uses_non_temp_checkout_scratch_parent() -> TestResult<()> {
    let scratch = repo_scratch_dir("parent")?;
    assert_ne!(
        scratch.path().parent(),
        Some(Path::new(env!("CARGO_MANIFEST_DIR"))),
        "test scratch dirs must not be created directly in the repo root"
    );
    assert!(
        scratch
            .path()
            .starts_with(common::checkout_test_temp_parent("sandbox-state-meta")?),
        "sandbox cwd scratch dirs should stay under target/test-scratch"
    );
    assert!(
        !scratch.path().starts_with(std::env::temp_dir()),
        "sandbox cwd scratch dirs must stay outside OS temp so read-only sandbox tests can observe blocked cwd writes"
    );
    Ok(())
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

async fn spawn_python_inherit_server(cwd: &Path) -> TestResult<McpTestSession> {
    common::spawn_server_with_args_env_and_cwd(
        vec![
            "--interpreter".to_string(),
            "python".to_string(),
            "--sandbox".to_string(),
            "inherit".to_string(),
        ],
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

#[cfg(unix)]
async fn spawn_inherit_pager_server_with_env(
    cwd: &Path,
    page_chars: u64,
    env: Vec<(String, String)>,
) -> TestResult<McpTestSession> {
    common::spawn_server_with_args_env_and_cwd_and_pager_page_chars(
        vec!["--sandbox".to_string(), "inherit".to_string()],
        env,
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

fn timeout_then_tail_code_after(wait_secs: f64) -> String {
    format!(
        r#"
Sys.sleep(0.2)
cat("MID\n")
flush.console()
Sys.sleep({wait_secs:.3})
cat("TAIL\n")
flush.console()
"#
    )
}

fn timeout_then_paged_tail_code() -> &'static str {
    r#"
line <- paste(rep("foo", 80), collapse = " ")
for (i in 1:300) cat(sprintf("line%04d %s\n", i, line))
flush.console()
Sys.sleep(1.0)
cat("TAIL\n")
flush.console()
"#
}

#[cfg(unix)]
fn timeout_then_paged_exit_code() -> &'static str {
    r#"
line <- paste(rep("foo", 80), collapse = " ")
for (i in 1:300) cat(sprintf("line%04d %s\n", i, line))
flush.console()
Sys.sleep(1.0)
q("no", status = 0, runLast = FALSE)
"#
}

fn timeout_then_done_code() -> &'static str {
    r#"
Sys.sleep(0.2)
cat("DONE\n")
flush.console()
"#
}

#[cfg(unix)]
fn timeout_then_exit_code() -> &'static str {
    r#"
cat("BEFORE_EXIT\n")
flush.console()
Sys.sleep(0.2)
q("no", status = 0, runLast = FALSE)
"#
}

#[cfg(unix)]
fn timeout_then_tail_exit_code() -> &'static str {
    r#"
Sys.sleep(0.2)
cat("MID\n")
flush.console()
Sys.sleep(1.0)
q("no", status = 0, runLast = FALSE)
"#
}

#[cfg(unix)]
fn interrupt_then_exit_code() -> &'static str {
    r#"
tryCatch({
  Sys.sleep(30)
}, interrupt = function(e) {
  cat("INTERRUPT_EXIT\n")
  flush.console()
  q("no", status = 0, runLast = FALSE)
})
"#
}

#[cfg(unix)]
fn interrupt_then_prompt_code() -> &'static str {
    r#"
tryCatch({
  Sys.sleep(30)
}, interrupt = function(e) {
  cat("INTERRUPT_PROMPT\n")
  flush.console()
})
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
             Sys.sleep(1.0); \
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
             Sys.sleep(1.0); \
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

#[cfg(unix)]
fn worker_spawn_policy_types(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .filter(|entry| entry["event"] == "worker_spawn_begin")
        .filter_map(|entry| entry["payload"]["sandbox_policy"]["type"].as_str())
        .map(str::to_string)
        .collect()
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
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
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
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
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
        "expected missing sandbox-state-meta to be reported as an MCP tool error"
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
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("failed to parse Codex sandbox state metadata"),
        "expected malformed sandbox-state-meta error, got: {text}"
    );
    assert_eq!(
        result.is_error,
        Some(true),
        "expected malformed sandbox-state-meta to be reported as an MCP tool error"
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
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
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
async fn sandbox_inherit_empty_repl_after_reset_uses_staged_state_meta() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let session = spawn_inherit_server(temp.path()).await?;
    let reset = session
        .call_tool_raw_with_meta(
            "repl_reset",
            json!({}),
            Some(workspace_write_meta(temp.path())),
        )
        .await?;
    let reset_text = collect_text(&reset);
    assert!(
        reset_text.contains("new session started"),
        "expected repl_reset with sandbox metadata to succeed, got: {reset_text}"
    );

    let result = session
        .write_stdin_raw_with_meta("", Some(2.0), Some(workspace_write_meta(temp.path())))
        .await?;
    let text = collect_text(&result);
    session.cancel().await?;

    if backend_unavailable(&text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        return Ok(());
    }
    assert!(
        text.contains("<<repl status: idle>>"),
        "expected empty inherit repl call after reset to return idle status, got: {text}"
    );
    assert_ne!(
        result.is_error,
        Some(true),
        "did not expect empty inherit repl call after reset to fail closed"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_empty_poll_with_existing_worker_ignores_bad_state_meta() -> TestResult<()>
{
    let _guard = test_guard();
    let temp = tempdir()?;
    let session = spawn_inherit_server(temp.path()).await?;
    let initial = session
        .write_stdin_raw_with_meta("1+1", Some(2.0), Some(workspace_write_meta(temp.path())))
        .await?;
    let initial_text = collect_text(&initial);
    if backend_unavailable(&initial_text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let poll = session
        .write_stdin_raw_with_meta(
            "",
            Some(2.0),
            Some(json!({ SANDBOX_STATE_META_CAPABILITY: "invalid" })),
        )
        .await?;
    let poll_text = collect_text(&poll);
    session.cancel().await?;

    assert_ne!(
        poll.is_error,
        Some(true),
        "did not expect empty poll with existing worker to fail on malformed metadata"
    );
    assert!(
        !poll_text.contains("failed to parse Codex sandbox state metadata"),
        "expected empty poll with existing worker to ignore malformed metadata, got: {poll_text}"
    );
    assert!(
        poll_text.contains("<<repl status: idle>>") || poll_text.contains(">"),
        "expected empty poll with existing worker to return local status, got: {poll_text}"
    );
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
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
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
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let interrupt = session
        .write_stdin_raw_unterminated_with_meta(
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
        empty_or_blank_stderr(&interrupt_text)
            || interrupt_text.contains(">")
            || interrupt_text.contains("<<repl status: busy")
            || interrupt_text.contains("<<repl status: idle>>"),
        "expected interrupt follow-up to return local recovery output or an empty clean reply, got: {interrupt_text}"
    );
    session.cancel().await?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_pending_bare_interrupt_ignores_missing_state_meta() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let session = spawn_inherit_files_server(temp.path(), Vec::new()).await?;
    let timeout = session
        .write_stdin_raw_with_meta(
            interrupt_then_prompt_code(),
            Some(0.05),
            Some(workspace_write_meta(temp.path())),
        )
        .await?;
    let timeout_text = collect_text(&timeout);
    if backend_unavailable(&timeout_text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        timeout_text.contains("<<repl status: busy"),
        "expected interrupt setup request to time out, got: {timeout_text}"
    );

    let interrupt = session
        .write_stdin_raw_unterminated_with("\u{3}", Some(10.0))
        .await?;
    let interrupt_text = collect_text(&interrupt);
    session.cancel().await?;

    assert!(
        !interrupt_text.contains(MISSING_INHERITED_STATE_MESSAGE),
        "expected pending bare Ctrl-C to ignore missing metadata, got: {interrupt_text}"
    );
    assert_ne!(
        interrupt.is_error,
        Some(true),
        "did not expect pending bare Ctrl-C without metadata to set isError, got: {:?}",
        interrupt.is_error
    );
    assert!(
        interrupt_text.contains("INTERRUPT_PROMPT"),
        "expected pending bare Ctrl-C to interrupt the worker, got: {interrupt_text}"
    );
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
            Some(0.5),
            Some(workspace_write_meta(temp.path())),
        )
        .await?;
    let first_text = common::result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        bundle_transcript_path(&first_text).is_none(),
        "did not expect the first under-threshold timeout reply to disclose a bundle path, got: {first_text:?}"
    );

    tokio::time::sleep(test_delay_ms(1100, 1500)).await;

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
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
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
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
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

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_session_ended_pager_command_ignores_state_meta_changes() -> TestResult<()>
{
    let _guard = test_guard();
    let scratch = repo_scratch_dir("sandbox-ended-pager-local-state-meta")?;
    let debug_dir = scratch.path().join("debug");
    let session = spawn_inherit_pager_server_with_env(
        scratch.path(),
        120,
        vec![(
            "MCP_REPL_DEBUG_DIR".to_string(),
            debug_dir.to_string_lossy().to_string(),
        )],
    )
    .await?;
    let timed_out = session
        .write_stdin_raw_with_meta(
            timeout_then_paged_exit_code(),
            Some(0.5),
            Some(workspace_write_meta(scratch.path())),
        )
        .await?;
    let timed_out_text = common::result_text(&timed_out);
    if backend_unavailable(&timed_out_text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        timed_out_text.contains("--More--"),
        "expected timed-out request to leave pager active, got: {timed_out_text}"
    );
    tokio::time::sleep(test_delay_ms(1100, 1500)).await;

    let quit = session
        .write_stdin_raw_with_meta(":q", Some(5.0), Some(read_only_meta(scratch.path())))
        .await?;
    let quit_text = common::result_text(&quit);
    if quit_text.contains("<<repl status: busy") {
        eprintln!("timed-out pager request did not observe session end before timeout; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        !quit_text.contains("unexpected ':'"),
        "expected :q to remain pager-local after session end, got: {quit_text}"
    );
    session.cancel().await?;

    let policy_types = worker_spawn_policy_types(&latest_debug_events(&debug_dir)?);
    assert_eq!(
        policy_types,
        vec!["workspace-write".to_string()],
        "did not expect pager-local :q to respawn under changed metadata, got policy sequence: {policy_types:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_pending_pager_command_ignores_missing_state_meta() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let session = spawn_inherit_pager_server(temp.path(), 120).await?;
    let timed_out = session
        .write_stdin_raw_with_meta(
            timeout_then_paged_tail_code(),
            Some(0.5),
            Some(workspace_write_meta(temp.path())),
        )
        .await?;
    let timed_out_text = common::result_text(&timed_out);
    if backend_unavailable(&timed_out_text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        timed_out_text.contains("--More--"),
        "expected timed-out request to leave pager active, got: {timed_out_text}"
    );

    let quit = session.write_stdin_raw_with(":q", Some(1.0)).await?;
    let quit_text = common::result_text(&quit);
    assert!(
        !quit_text.contains(MISSING_INHERITED_STATE_MESSAGE),
        "expected pending pager command to ignore missing inherited metadata, got: {quit_text}"
    );
    assert!(
        !quit_text.contains("sandbox policy changed; new session started"),
        "did not expect pending pager command to restart the worker, got: {quit_text}"
    );
    assert!(
        !quit_text.contains("unexpected ':'"),
        "expected :q to remain pager-local while a request is pending, got: {quit_text}"
    );

    tokio::time::sleep(test_delay_ms(1100, 1500)).await;
    let poll = session
        .write_stdin_raw_with_meta("", Some(2.0), Some(workspace_write_meta(temp.path())))
        .await?;
    let poll_text = common::result_text(&poll);
    session.cancel().await?;

    assert!(
        poll_text.contains("TAIL"),
        "expected pending request to keep running after pager-local :q, got: {poll_text}"
    );
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
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
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
            timeout_then_tail_code_after(3.0),
            Some(0.05),
            Some(workspace_write_meta(temp.path())),
        )
        .await?;
    let first_text = common::result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
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
    assert_eq!(
        interrupt_error.is_error,
        Some(true),
        "expected malformed metadata follow-up to be reported as an MCP tool error"
    );
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

    let mut final_poll_text = busy_follow_up_text.clone();
    for _ in 0..20 {
        let final_poll = session
            .write_stdin_raw_with_meta("", Some(0.2), Some(workspace_write_meta(temp.path())))
            .await?;
        let poll_text = common::result_text(&final_poll);
        final_poll_text.push_str(&poll_text);
        if !poll_text.contains("<<repl status: busy") && final_poll_text.contains("TAIL") {
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
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
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
    assert_eq!(
        restart_error.is_error,
        Some(true),
        "expected malformed metadata restart follow-up to be reported as an MCP tool error"
    );
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
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
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
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
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

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_busy_follow_up_stages_current_meta_before_session_end_reset()
-> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let debug_dir = temp.path().join("debug");
    let session = spawn_inherit_files_server(
        temp.path(),
        vec![(
            "MCP_REPL_DEBUG_DIR".to_string(),
            debug_dir.to_string_lossy().to_string(),
        )],
    )
    .await?;

    let timeout = session
        .write_stdin_raw_with_meta(
            timeout_then_tail_exit_code(),
            Some(0.05),
            Some(workspace_write_meta(temp.path())),
        )
        .await?;
    let timeout_text = collect_text(&timeout);
    if backend_unavailable(&timeout_text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        timeout_text.contains("<<repl status: busy"),
        "expected exit setup request to time out, got: {timeout_text}"
    );
    tokio::time::sleep(test_delay_ms(260, 700)).await;

    let follow_up = session
        .write_stdin_raw_with_meta(
            "cat(\"BUSY_FOLLOWUP\\n\")",
            Some(5.0),
            Some(read_only_meta(temp.path())),
        )
        .await?;
    let follow_up_text = collect_text(&follow_up);
    session.cancel().await?;

    if follow_up_text.contains("<<repl status: busy") {
        eprintln!("busy follow-up did not observe session end before timeout; skipping");
        return Ok(());
    }
    let policy_types = worker_spawn_policy_types(&latest_debug_events(&debug_dir)?);
    assert_eq!(
        policy_types,
        vec!["workspace-write".to_string(), "read-only".to_string()],
        "expected busy-follow-up reset to use current read-only metadata, got policy sequence: {policy_types:?}"
    );
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
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
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
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
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
    let home_dir = home_scratch_dir("sandbox-empty-poll-session-end-respawn-home")?;
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
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
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
            Some(0.5),
            Some(full_access_meta(scratch.path())),
        )
        .await?;
    let first_text = common::result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
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
    let home_dir = home_scratch_dir("sandbox-empty-poll-session-end-missing-meta-home")?;
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
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
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
        !drained_text.contains(MISSING_INHERITED_STATE_MESSAGE),
        "did not expect draining poll without metadata to replace local output with a metadata error, got: {drained_text}"
    );
    assert_ne!(
        drained.is_error,
        Some(true),
        "did not expect draining poll without metadata to set isError, got: {:?}",
        drained.is_error
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
    let home_dir = home_scratch_dir("sandbox-bare-interrupt-session-end-meta-home")?;
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
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let _ = fs::remove_file(&startup_target);
    tokio::time::sleep(std::time::Duration::from_millis(260)).await;

    let interrupt = session
        .write_stdin_raw_unterminated_with_meta(
            "\u{3}",
            Some(2.0),
            Some(read_only_meta(scratch.path())),
        )
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
    let home_dir = home_scratch_dir("sandbox-bare-interrupt-session-end-missing-meta-home")?;
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
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let _ = fs::remove_file(&startup_target);
    tokio::time::sleep(std::time::Duration::from_millis(260)).await;

    let interrupt = session
        .write_stdin_raw_unterminated_with("\u{3}", Some(2.0))
        .await?;
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
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
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
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
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
            Some(0.5),
            Some(read_only_meta(scratch.path())),
        )
        .await?;
    let first_text = common::result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        bundle_transcript_path(&first_text).is_none(),
        "did not expect the initial timeout reply to disclose a transcript path, got: {first_text:?}"
    );

    tokio::time::sleep(test_delay_ms(1100, 1500)).await;

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
            Some(0.5),
            Some(read_only_meta(scratch.path())),
        )
        .await?;
    let first_text = common::result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        bundle_transcript_path(&first_text).is_none(),
        "did not expect the initial timeout reply to disclose a transcript path, got: {first_text:?}"
    );

    tokio::time::sleep(test_delay_ms(1100, 1500)).await;

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
            Some(0.5),
            Some(read_only_meta(scratch.path())),
        )
        .await?;
    let first_text = common::result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
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
        "expected the respawned follow-up output to stay with the fresh input batch, got reply {second_text:?} and transcript {second_transcript:?}"
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
            eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
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
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
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
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    tokio::time::sleep(test_delay_ms(260, 700)).await;

    let restart = session
        .write_stdin_raw_unterminated_with_meta(
            "\u{4}",
            Some(1.0),
            Some(read_only_meta(scratch.path())),
        )
        .await?;
    let restart_text = common::result_text(&restart);
    assert!(
        restart_text.contains("new session started"),
        "expected bare Ctrl-D after sandbox respawn to remain an explicit restart, got: {restart_text}"
    );
    assert!(
        restart_text.contains("sandbox policy changed; new session started"),
        "expected bare Ctrl-D after sandbox respawn to flush the sandbox-change notice, got: {restart_text}"
    );
    assert!(
        !restart_text.contains("MID") && !restart_text.contains("TAIL"),
        "did not expect bare Ctrl-D after sandbox respawn to drain preserved timeout output, got: {restart_text}"
    );
    assert!(
        !restart_text.contains("<<repl status: idle>>"),
        "did not expect bare Ctrl-D after sandbox respawn to degrade into an empty poll, got: {restart_text}"
    );

    let follow_up = session
        .write_stdin_raw_with_meta("1+1", Some(1.0), Some(read_only_meta(scratch.path())))
        .await?;
    let follow_up_text = common::result_text(&follow_up);
    session.cancel().await?;

    assert!(
        !follow_up_text.contains("sandbox policy changed; new session started"),
        "did not expect the sandbox-change notice to leak into the next unrelated reply, got: {follow_up_text}"
    );
    assert!(
        !follow_up_text.contains("MID") && !follow_up_text.contains("TAIL"),
        "did not expect preserved timeout output to leak into the next unrelated reply, got: {follow_up_text}"
    );
    assert!(
        follow_up_text.contains("[1] 2"),
        "expected the post-restart follow-up to run normally, got: {follow_up_text}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_active_pager_bare_restart_stays_restart_after_sandbox_respawn()
-> TestResult<()> {
    let _guard = test_guard();
    let scratch = repo_scratch_dir("sandbox-pager-bare-restart-after-respawn")?;
    let session = spawn_inherit_pager_server(scratch.path(), 120).await?;
    let initial = session
        .write_stdin_raw_with_meta(
            "line <- paste(rep(\"foo\", 80), collapse = \" \"); for (i in 1:300) cat(sprintf(\"line%04d %s\\n\", i, line))",
            Some(30.0),
            Some(workspace_write_meta(scratch.path())),
        )
        .await?;
    let initial_text = common::result_text(&initial);
    if backend_unavailable(&initial_text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        initial_text.contains("--More--"),
        "expected pager to activate before bare Ctrl-D restart test, got: {initial_text:?}"
    );

    let restart = session
        .write_stdin_raw_unterminated_with_meta(
            "\u{4}",
            Some(10.0),
            Some(read_only_meta(scratch.path())),
        )
        .await?;
    let restart_text = common::result_text(&restart);
    session.cancel().await?;

    assert!(
        restart_text.contains("new session started"),
        "expected active-pager bare Ctrl-D after sandbox respawn to remain an explicit restart, got: {restart_text}"
    );
    assert!(
        restart_text.contains("[repl] new session started"),
        "expected active-pager bare Ctrl-D to emit the explicit restart reply, got: {restart_text}"
    );
    assert!(
        restart_text.contains("sandbox policy changed; new session started"),
        "expected active-pager bare Ctrl-D to flush the sandbox-change notice, got: {restart_text}"
    );
    assert!(
        !restart_text.contains("--More--"),
        "did not expect active-pager bare Ctrl-D to degrade into pager navigation, got: {restart_text}"
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
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
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

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_explicit_path_write_meta_blocks_missing_protected_metadata()
-> TestResult<()> {
    let _guard = test_guard();
    let scratch = repo_scratch_dir("sandbox-explicit-path-write-protected")?;
    let writable_root = scratch.path().join("explicit-root");
    fs::create_dir(&writable_root)?;
    for protected_name in [".git", ".agents", ".codex"] {
        assert!(
            !writable_root.join(protected_name).exists(),
            "test requires missing protected metadata path: {}",
            writable_root.join(protected_name).display()
        );
    }
    let encoded_writable_root = encode_path(&writable_root)?;
    let session = spawn_inherit_server(scratch.path()).await?;
    let result = session
        .write_stdin_raw_with_meta(
            format!(
                r#"
writable_root <- {encoded_writable_root}
allowed_target <- file.path(writable_root, "allowed.txt")
tryCatch({{
  writeLines("allowed", allowed_target)
  cat("ALLOWED_WRITE_OK\n")
}}, error = function(e) {{
  message("ALLOWED_WRITE_ERROR:", conditionMessage(e))
}})
for (protected_name in c(".git", ".agents", ".codex")) {{
  protected_dir <- file.path(writable_root, protected_name)
  protected_target <- file.path(protected_dir, "blocked.txt")
  tryCatch({{
    dir.create(protected_dir)
    writeLines("blocked", protected_target)
    cat("PROTECTED_WRITE_OK:", protected_name, "\n", sep = "")
  }}, error = function(e) {{
    message("PROTECTED_WRITE_ERROR:", protected_name, ":", conditionMessage(e))
  }})
}}
"#
            ),
            Some(10.0),
            Some(explicit_path_write_meta(scratch.path(), &writable_root)),
        )
        .await?;
    let text = collect_text(&result);
    if backend_unavailable(&text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("ALLOWED_WRITE_OK"),
        "expected explicit path write root to allow ordinary writes, got: {text}"
    );
    assert!(
        !text.contains("ALLOWED_WRITE_ERROR:"),
        "explicit path write root unexpectedly blocked ordinary write: {text}"
    );
    for protected_name in [".git", ".agents", ".codex"] {
        assert!(
            text.contains(&format!("PROTECTED_WRITE_ERROR:{protected_name}:")),
            "expected explicit path write root to block missing protected metadata {protected_name}, got: {text}"
        );
        assert!(
            !text.contains(&format!("PROTECTED_WRITE_OK:{protected_name}")),
            "explicit path write root unexpectedly allowed protected metadata write {protected_name}: {text}"
        );
    }
    session.cancel().await?;
    assert!(
        writable_root.join("allowed.txt").exists(),
        "ordinary write under explicit root should create the target file"
    );
    for protected_name in [".git", ".agents", ".codex"] {
        assert!(
            !writable_root.join(protected_name).exists(),
            "protected metadata write should not create {}",
            writable_root.join(protected_name).display()
        );
    }
    Ok(())
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_workspace_write_meta_blocks_missing_protected_metadata_alias()
-> TestResult<()> {
    let _guard = test_guard();
    let scratch = Builder::new()
        .prefix(".tmp-sandbox-protected-alias-")
        .tempdir_in("/tmp")?;
    let canonical_scratch = scratch.path().canonicalize()?;
    if canonical_scratch == scratch.path() {
        eprintln!(
            "{} does not have a distinct canonical path; skipping",
            scratch.path().display()
        );
        return Ok(());
    }
    let protected_dir = canonical_scratch.join(".git");
    assert!(
        !protected_dir.exists(),
        "test requires missing protected metadata path: {}",
        protected_dir.display()
    );
    let encoded_canonical_scratch = encode_path(&canonical_scratch)?;
    let encoded_protected_dir = encode_path(&protected_dir)?;
    let session = spawn_inherit_server(scratch.path()).await?;
    let result = session
        .write_stdin_raw_with_meta(
            format!(
                r#"
canonical_scratch <- {encoded_canonical_scratch}
protected_dir <- {encoded_protected_dir}
allowed_target <- file.path(canonical_scratch, "allowed.txt")
tryCatch({{
  writeLines("allowed", allowed_target)
  cat("ALIAS_ALLOWED_WRITE_OK\n")
}}, error = function(e) {{
  message("ALIAS_ALLOWED_WRITE_ERROR:", conditionMessage(e))
}})
protected_target <- file.path(protected_dir, "blocked.txt")
tryCatch({{
  suppressWarnings(dir.create(protected_dir))
  writeLines("blocked", protected_target)
  cat("ALIAS_PROTECTED_WRITE_OK\n")
}}, error = function(e) {{
  message("ALIAS_PROTECTED_WRITE_ERROR:", conditionMessage(e))
}})
"#
            ),
            Some(10.0),
            Some(workspace_write_meta(scratch.path())),
        )
        .await?;
    let text = collect_text(&result);
    session.cancel().await?;
    if backend_unavailable(&text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        return Ok(());
    }
    assert!(
        text.contains("ALIAS_ALLOWED_WRITE_OK"),
        "expected canonical writable-root alias to allow ordinary writes, got: {text}"
    );
    assert!(
        !text.contains("ALIAS_ALLOWED_WRITE_ERROR:"),
        "canonical writable-root alias unexpectedly blocked ordinary write: {text}"
    );
    assert!(
        text.contains("ALIAS_PROTECTED_WRITE_ERROR:"),
        "expected canonical writable-root alias to block missing protected metadata, got: {text}"
    );
    assert!(
        !text.contains("ALIAS_PROTECTED_WRITE_OK"),
        "canonical writable-root alias unexpectedly allowed protected metadata write: {text}"
    );
    assert!(
        canonical_scratch.join("allowed.txt").exists(),
        "ordinary write through canonical writable-root alias should create the target file"
    );
    assert!(
        !protected_dir.exists(),
        "protected metadata write should not create {}",
        protected_dir.display()
    );
    Ok(())
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_glob_deny_meta_allows_write_but_blocks_read_and_unlink_in_cwd()
-> TestResult<()> {
    let _guard = test_guard();
    let scratch = repo_scratch_dir("sandbox-glob-deny-write")?;
    let target = scratch.path().join("secret.env");
    fs::write(&target, "original\n")?;
    let encoded_target = encode_path(&target)?;
    let session = spawn_inherit_server(scratch.path()).await?;
    let result = session
        .write_stdin_raw_with_meta(
            format!(
                r#"
target <- {encoded_target}
tryCatch({{
  writeLines("allowed", target)
  cat("WRITE_OK\n")
}}, error = function(e) {{
  message("WRITE_ERROR:", conditionMessage(e))
}})
tryCatch({{
  readLines(target)
  cat("READ_OK\n")
}}, error = function(e) {{
  message("READ_ERROR:", conditionMessage(e))
}})
status <- suppressWarnings(unlink(target))
cat("UNLINK_STATUS:", status, "\n", sep = "")
"#
            ),
            Some(10.0),
            Some(workspace_write_with_glob_deny_meta(
                scratch.path(),
                "**/*.env",
            )),
        )
        .await?;
    let text = collect_text(&result);
    if backend_unavailable(&text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("WRITE_OK"),
        "expected glob-denied file write in cwd to succeed, got: {text}"
    );
    assert!(
        !text.contains("WRITE_ERROR:"),
        "glob-denied file write in cwd unexpectedly failed: {text}"
    );
    assert!(
        text.contains("READ_ERROR:"),
        "expected glob-denied file read in cwd to fail, got: {text}"
    );
    assert!(
        !text.contains("READ_OK"),
        "glob-denied file read in cwd unexpectedly succeeded: {text}"
    );
    session.cancel().await?;
    assert_eq!(
        fs::read_to_string(&target)?,
        "allowed\n",
        "glob-denied file write should update contents while unlink remains denied"
    );
    Ok(())
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_glob_deny_meta_blocks_canonical_tmp_read_and_unlink() -> TestResult<()> {
    let _guard = test_guard();
    let scratch = Builder::new()
        .prefix(".tmp-sandbox-glob-deny-canonical-")
        .tempdir_in("/tmp")?;
    let target = scratch.path().join("secret.env");
    fs::write(&target, "original\n")?;
    let canonical_target = target.canonicalize()?;
    if canonical_target == target {
        eprintln!(
            "{} does not have a distinct canonical path; skipping",
            target.display()
        );
        return Ok(());
    }
    let encoded_canonical_target = encode_path(&canonical_target)?;
    let session = spawn_inherit_server(scratch.path()).await?;
    let result = session
        .write_stdin_raw_with_meta(
            format!(
                r#"
target <- {encoded_canonical_target}
tryCatch({{
  readLines(target)
  cat("READ_OK\n")
}}, error = function(e) {{
  message("READ_ERROR:", conditionMessage(e))
}})
status <- suppressWarnings(unlink(target))
cat("UNLINK_STATUS:", status, "\n", sep = "")
"#
            ),
            Some(10.0),
            Some(workspace_write_with_glob_deny_meta(
                scratch.path(),
                "**/*.env",
            )),
        )
        .await?;
    let text = collect_text(&result);
    if backend_unavailable(&text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("READ_ERROR:"),
        "expected canonical glob-denied file read to fail, got: {text}"
    );
    assert!(
        !text.contains("READ_OK"),
        "canonical glob-denied file read unexpectedly succeeded: {text}"
    );
    session.cancel().await?;
    assert!(
        target.exists(),
        "canonical glob-denied unlink should leave the target file in place"
    );
    Ok(())
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_path_deny_meta_blocks_write_read_and_unlink_in_cwd() -> TestResult<()> {
    let _guard = test_guard();
    let scratch = repo_scratch_dir("sandbox-path-deny-write")?;
    let target = scratch.path().join("secret.txt");
    fs::write(&target, "original\n")?;
    let encoded_target = encode_path(&target)?;
    let session = spawn_inherit_server(scratch.path()).await?;
    let result = session
        .write_stdin_raw_with_meta(
            format!(
                r#"
target <- {encoded_target}
tryCatch({{
  writeLines("allowed", target)
  cat("WRITE_OK\n")
}}, error = function(e) {{
  message("WRITE_ERROR:", conditionMessage(e))
}})
tryCatch({{
  readLines(target)
  cat("READ_OK\n")
}}, error = function(e) {{
  message("READ_ERROR:", conditionMessage(e))
}})
status <- suppressWarnings(unlink(target))
cat("UNLINK_STATUS:", status, "\n", sep = "")
"#
            ),
            Some(10.0),
            Some(workspace_write_with_path_deny_meta(scratch.path(), &target)),
        )
        .await?;
    let text = collect_text(&result);
    if backend_unavailable(&text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("WRITE_ERROR:"),
        "expected path-denied file write in cwd to fail, got: {text}"
    );
    assert!(
        !text.contains("WRITE_OK"),
        "path-denied file write in cwd unexpectedly succeeded: {text}"
    );
    assert!(
        text.contains("READ_ERROR:"),
        "expected path-denied file read in cwd to fail, got: {text}"
    );
    assert!(
        !text.contains("READ_OK"),
        "path-denied file read in cwd unexpectedly succeeded: {text}"
    );
    session.cancel().await?;
    assert_eq!(
        fs::read_to_string(&target)?,
        "original\n",
        "path-denied file write should leave contents unchanged while unlink remains denied"
    );
    Ok(())
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_path_deny_meta_blocks_missing_alias_path_creation() -> TestResult<()> {
    let _guard = test_guard();
    let scratch = Builder::new()
        .prefix(".tmp-sandbox-path-deny-alias-")
        .tempdir_in("/tmp")?;
    let canonical_scratch = scratch.path().canonicalize()?;
    if canonical_scratch == scratch.path() {
        eprintln!(
            "{} does not have a distinct canonical path; skipping",
            scratch.path().display()
        );
        return Ok(());
    }
    let denied_dir = scratch.path().join("denied");
    let canonical_denied_dir = canonical_scratch.join("denied");
    assert!(
        !denied_dir.exists(),
        "test requires missing denied path: {}",
        denied_dir.display()
    );
    let encoded_canonical_scratch = encode_path(&canonical_scratch)?;
    let encoded_canonical_denied_dir = encode_path(&canonical_denied_dir)?;
    let session = spawn_inherit_server(scratch.path()).await?;
    let result = session
        .write_stdin_raw_with_meta(
            format!(
                r#"
canonical_scratch <- {encoded_canonical_scratch}
denied_dir <- {encoded_canonical_denied_dir}
allowed_target <- file.path(canonical_scratch, "allowed.txt")
tryCatch({{
  writeLines("allowed", allowed_target)
  cat("PATH_DENY_ALIAS_ALLOWED_WRITE_OK\n")
}}, error = function(e) {{
  message("PATH_DENY_ALIAS_ALLOWED_WRITE_ERROR:", conditionMessage(e))
}})
denied_target <- file.path(denied_dir, "blocked.txt")
tryCatch({{
  suppressWarnings(dir.create(denied_dir))
  writeLines("blocked", denied_target)
  cat("PATH_DENY_ALIAS_WRITE_OK\n")
}}, error = function(e) {{
  message("PATH_DENY_ALIAS_WRITE_ERROR:", conditionMessage(e))
}})
"#
            ),
            Some(10.0),
            Some(workspace_write_with_path_deny_meta(
                scratch.path(),
                &canonical_denied_dir,
            )),
        )
        .await?;
    let text = collect_text(&result);
    session.cancel().await?;
    if backend_unavailable(&text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        return Ok(());
    }
    assert!(
        text.contains("PATH_DENY_ALIAS_ALLOWED_WRITE_OK"),
        "expected canonical writable-root alias to allow ordinary writes, got: {text}"
    );
    assert!(
        !text.contains("PATH_DENY_ALIAS_ALLOWED_WRITE_ERROR:"),
        "canonical writable-root alias unexpectedly blocked ordinary write: {text}"
    );
    assert!(
        text.contains("PATH_DENY_ALIAS_WRITE_ERROR:"),
        "expected canonical path-deny alias to block missing denied path creation, got: {text}"
    );
    assert!(
        !text.contains("PATH_DENY_ALIAS_WRITE_OK"),
        "canonical path-deny alias unexpectedly allowed denied path creation: {text}"
    );
    assert!(
        canonical_scratch.join("allowed.txt").exists(),
        "ordinary write through canonical writable-root alias should create the target file"
    );
    assert!(
        !canonical_denied_dir.exists(),
        "path-deny write should not create {}",
        canonical_denied_dir.display()
    );
    Ok(())
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_path_deny_meta_preserves_more_specific_child_write() -> TestResult<()> {
    let _guard = test_guard();
    let scratch = repo_scratch_dir("sandbox-path-deny-child-write")?;
    let denied_dir = scratch.path().join("private");
    let allowed_dir = denied_dir.join("allowed");
    fs::create_dir_all(&allowed_dir)?;
    let allowed_target = allowed_dir.join("allowed.txt");
    let blocked_target = denied_dir.join("blocked.txt");
    let encoded_allowed_target = encode_path(&allowed_target)?;
    let encoded_blocked_target = encode_path(&blocked_target)?;
    let session = spawn_inherit_server(scratch.path()).await?;
    let result = session
        .write_stdin_raw_with_meta(
            format!(
                r#"
allowed_target <- {encoded_allowed_target}
blocked_target <- {encoded_blocked_target}
tryCatch({{
  writeLines("allowed", allowed_target)
  cat("CHILD_WRITE_OK\n")
}}, error = function(e) {{
  message("CHILD_WRITE_ERROR:", conditionMessage(e))
}})
tryCatch({{
  readLines(allowed_target)
  cat("CHILD_READ_OK\n")
}}, error = function(e) {{
  message("CHILD_READ_ERROR:", conditionMessage(e))
}})
tryCatch({{
  writeLines("blocked", blocked_target)
  cat("PARENT_WRITE_OK\n")
}}, error = function(e) {{
  message("PARENT_WRITE_ERROR:", conditionMessage(e))
}})
"#
            ),
            Some(10.0),
            Some(workspace_write_with_path_deny_and_child_write_meta(
                scratch.path(),
                &denied_dir,
                &allowed_dir,
            )),
        )
        .await?;
    let text = collect_text(&result);
    session.cancel().await?;
    if backend_unavailable(&text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        return Ok(());
    }
    assert!(
        text.contains("CHILD_WRITE_OK"),
        "expected re-allowed child write under denied parent to succeed, got: {text}"
    );
    assert!(
        !text.contains("CHILD_WRITE_ERROR:"),
        "re-allowed child write unexpectedly failed: {text}"
    );
    assert!(
        text.contains("CHILD_READ_OK"),
        "expected re-allowed child read under denied parent to succeed, got: {text}"
    );
    assert!(
        !text.contains("CHILD_READ_ERROR:"),
        "re-allowed child read unexpectedly failed: {text}"
    );
    assert!(
        text.contains("PARENT_WRITE_ERROR:"),
        "expected sibling under denied parent to remain blocked, got: {text}"
    );
    assert!(
        !text.contains("PARENT_WRITE_OK"),
        "denied parent unexpectedly allowed sibling write: {text}"
    );
    assert_eq!(
        fs::read_to_string(&allowed_target)?,
        "allowed\n",
        "re-allowed child write should persist"
    );
    assert!(
        !blocked_target.exists(),
        "denied parent write should not create {}",
        blocked_target.display()
    );
    Ok(())
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_path_deny_meta_preserves_alias_child_write() -> TestResult<()> {
    let _guard = test_guard();
    let scratch = Builder::new()
        .prefix(".tmp-sandbox-path-deny-child-alias-")
        .tempdir_in("/tmp")?;
    let canonical_scratch = scratch.path().canonicalize()?;
    if canonical_scratch == scratch.path() {
        eprintln!(
            "{} does not have a distinct canonical path; skipping",
            scratch.path().display()
        );
        return Ok(());
    }
    let denied_dir = canonical_scratch.join("private");
    let allowed_dir = scratch.path().join("private").join("allowed");
    fs::create_dir_all(&allowed_dir)?;
    let allowed_target = canonical_scratch
        .join("private")
        .join("allowed")
        .join("allowed.txt");
    let blocked_target = canonical_scratch.join("private").join("blocked.txt");
    let encoded_allowed_target = encode_path(&allowed_target)?;
    let encoded_blocked_target = encode_path(&blocked_target)?;
    let session = spawn_inherit_server(scratch.path()).await?;
    let result = session
        .write_stdin_raw_with_meta(
            format!(
                r#"
allowed_target <- {encoded_allowed_target}
blocked_target <- {encoded_blocked_target}
tryCatch({{
  writeLines("allowed", allowed_target)
  cat("ALIAS_CHILD_WRITE_OK\n")
}}, error = function(e) {{
  message("ALIAS_CHILD_WRITE_ERROR:", conditionMessage(e))
}})
tryCatch({{
  readLines(allowed_target)
  cat("ALIAS_CHILD_READ_OK\n")
}}, error = function(e) {{
  message("ALIAS_CHILD_READ_ERROR:", conditionMessage(e))
}})
tryCatch({{
  writeLines("blocked", blocked_target)
  cat("ALIAS_PARENT_WRITE_OK\n")
}}, error = function(e) {{
  message("ALIAS_PARENT_WRITE_ERROR:", conditionMessage(e))
}})
"#
            ),
            Some(10.0),
            Some(workspace_write_with_path_deny_and_child_write_meta(
                scratch.path(),
                &denied_dir,
                &allowed_dir,
            )),
        )
        .await?;
    let text = collect_text(&result);
    session.cancel().await?;
    if backend_unavailable(&text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        return Ok(());
    }
    assert!(
        text.contains("ALIAS_CHILD_WRITE_OK"),
        "expected re-allowed alias child write under denied parent to succeed, got: {text}"
    );
    assert!(
        !text.contains("ALIAS_CHILD_WRITE_ERROR:"),
        "re-allowed alias child write unexpectedly failed: {text}"
    );
    assert!(
        text.contains("ALIAS_CHILD_READ_OK"),
        "expected re-allowed alias child read under denied parent to succeed, got: {text}"
    );
    assert!(
        !text.contains("ALIAS_CHILD_READ_ERROR:"),
        "re-allowed alias child read unexpectedly failed: {text}"
    );
    assert!(
        text.contains("ALIAS_PARENT_WRITE_ERROR:"),
        "expected sibling under denied alias parent to remain blocked, got: {text}"
    );
    assert!(
        !text.contains("ALIAS_PARENT_WRITE_OK"),
        "denied alias parent unexpectedly allowed sibling write: {text}"
    );
    assert_eq!(
        fs::read_to_string(&allowed_target)?,
        "allowed\n",
        "re-allowed alias child write should persist"
    );
    assert!(
        !blocked_target.exists(),
        "denied alias parent write should not create {}",
        blocked_target.display()
    );
    Ok(())
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_accepts_restricted_read_workspace_write_meta() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let session = spawn_inherit_server(temp.path()).await?;
    let result = session
        .write_stdin_raw_with_meta(
            "1+1",
            Some(2.0),
            Some(workspace_write_restricted_read_meta(temp.path())),
        )
        .await?;
    let text = collect_text(&result);
    session.cancel().await?;

    if backend_unavailable(&text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        return Ok(());
    }
    assert!(
        result.is_error != Some(true),
        "expected restricted read metadata to be accepted, got: {text}"
    );
    assert!(
        text.contains("[1] 2"),
        "expected input to run after restricted read metadata, got: {text}"
    );
    Ok(())
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_minimal_meta_blocks_slash_tmp_write_without_slash_tmp_entry()
-> TestResult<()> {
    let _guard = test_guard();
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let slash_tmp_target = Path::new("/tmp").join(format!("mcp-repl-no-slash-tmp-{nanos}.txt"));
    let encoded_slash_tmp_target = encode_path(&slash_tmp_target)?;
    let temp = tempdir()?;
    let session = spawn_inherit_server(temp.path()).await?;
    let result = session
        .write_stdin_raw_with_meta(
            format!(
                r#"
slash_tmp_target <- {encoded_slash_tmp_target}
tmpdir_target <- file.path(Sys.getenv("TMPDIR"), "mcp-repl-tmpdir-write-ok.txt")
tryCatch({{
  writeLines("tmpdir", tmpdir_target)
  cat("TMPDIR_WRITE_OK\n")
}}, error = function(e) {{
  message("TMPDIR_WRITE_ERROR:", conditionMessage(e))
}})
tryCatch({{
  writeLines("slash_tmp", slash_tmp_target)
  cat("SLASH_TMP_WRITE_OK\n")
}}, error = function(e) {{
  message("SLASH_TMP_WRITE_ERROR:", conditionMessage(e))
}})
if (file.exists(slash_tmp_target)) unlink(slash_tmp_target)
"#
            ),
            Some(10.0),
            Some(minimal_read_meta(temp.path())),
        )
        .await?;
    let text = collect_text(&result);
    let _ = fs::remove_file(&slash_tmp_target);
    session.cancel().await?;

    if backend_unavailable(&text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        return Ok(());
    }
    assert!(
        text.contains("TMPDIR_WRITE_OK"),
        "expected minimal metadata to allow TMPDIR writes, got: {text}"
    );
    assert!(
        text.contains("SLASH_TMP_WRITE_ERROR:"),
        "expected minimal metadata without slash_tmp to block /tmp writes, got: {text}"
    );
    assert!(
        !text.contains("SLASH_TMP_WRITE_OK"),
        "minimal metadata without slash_tmp unexpectedly allowed /tmp writes: {text}"
    );
    Ok(())
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_workspace_write_meta_allows_explicit_slash_tmp_write() -> TestResult<()> {
    let _guard = test_guard();
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let slash_tmp_target = Path::new("/tmp").join(format!("mcp-repl-slash-tmp-{nanos}.txt"));
    let encoded_slash_tmp_target = encode_path(&slash_tmp_target)?;
    let scratch = repo_scratch_dir("sandbox-slash-tmp-write")?;
    let session = spawn_inherit_server(scratch.path()).await?;
    let result = session
        .write_stdin_raw_with_meta(
            format!(
                r#"
slash_tmp_target <- {encoded_slash_tmp_target}
tryCatch({{
  writeLines("slash_tmp", slash_tmp_target)
  cat("SLASH_TMP_WRITE_OK\n")
}}, error = function(e) {{
  message("SLASH_TMP_WRITE_ERROR:", conditionMessage(e))
}})
if (file.exists(slash_tmp_target)) unlink(slash_tmp_target)
"#
            ),
            Some(10.0),
            Some(workspace_write_meta(scratch.path())),
        )
        .await?;
    let text = collect_text(&result);
    let _ = fs::remove_file(&slash_tmp_target);
    session.cancel().await?;

    if backend_unavailable(&text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        return Ok(());
    }
    assert!(
        text.contains("SLASH_TMP_WRITE_OK"),
        "expected workspace-write metadata with slash_tmp to allow /tmp writes, got: {text}"
    );
    assert!(
        !text.contains("SLASH_TMP_WRITE_ERROR:"),
        "workspace-write metadata with slash_tmp unexpectedly blocked /tmp write: {text}"
    );
    Ok(())
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_minimal_path_deny_blocks_platform_default_read() -> TestResult<()> {
    let _guard = test_guard();
    let denied_root = Path::new("/Library/Preferences");
    let Some(denied_child) = fs::read_dir(denied_root)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.is_file())
    else {
        eprintln!("no direct file under {}; skipping", denied_root.display());
        return Ok(());
    };
    let encoded_denied_child = encode_path(&denied_child)?;
    let temp = tempdir()?;
    let session = spawn_inherit_server(temp.path()).await?;
    let result = session
        .write_stdin_raw_with_meta(
            format!(
                r#"
target <- {encoded_denied_child}
tryCatch({{
  readBin(target, "raw", n = 1)
  cat("PLATFORM_DEFAULT_READ_OK\n")
}}, error = function(e) {{
  message("PLATFORM_DEFAULT_READ_ERROR:", conditionMessage(e))
}})
"#
            ),
            Some(10.0),
            Some(minimal_read_with_path_deny_meta(temp.path(), denied_root)),
        )
        .await?;
    let text = collect_text(&result);
    session.cancel().await?;

    if backend_unavailable(&text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        return Ok(());
    }
    assert!(
        text.contains("PLATFORM_DEFAULT_READ_ERROR:"),
        "expected path deny to block platform-default read, got: {text}"
    );
    assert!(
        !text.contains("PLATFORM_DEFAULT_READ_OK"),
        "path deny unexpectedly allowed platform-default read: {text}"
    );
    Ok(())
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_minimal_meta_allows_python_libomp_shm() -> TestResult<()> {
    let _guard = test_guard();
    if !common::python_available() {
        eprintln!("python not available; skipping");
        return Ok(());
    }
    let temp = tempdir()?;
    let session = spawn_python_inherit_server(temp.path()).await?;
    let result = session
        .write_stdin_raw_with_meta(
            r#"
import ctypes
import os

libc = ctypes.CDLL(None, use_errno=True)
name = f"/__KMP_REGISTERED_LIB_{os.getpid()}".encode()
fd = libc.shm_open(name, os.O_CREAT | os.O_EXCL | os.O_RDWR, 0o600)
if fd == -1:
    err = ctypes.get_errno()
    print(f"SHM_ERROR:{err}:{os.strerror(err)}")
else:
    os.close(fd)
    libc.shm_unlink(name)
    print("SHM_OK")
"#,
            Some(10.0),
            Some(minimal_read_meta(temp.path())),
        )
        .await?;
    let text = collect_text(&result);
    session.cancel().await?;

    if backend_unavailable(&text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        return Ok(());
    }
    assert!(
        text.contains("SHM_OK"),
        "expected libomp-style shared memory registration under minimal metadata, got: {text}"
    );
    assert!(
        !text.contains("SHM_ERROR:"),
        "minimal metadata blocked libomp-style shared memory registration: {text}"
    );
    Ok(())
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_minimal_meta_allows_r_startup_and_practical_probes() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let session = spawn_inherit_server(temp.path()).await?;
    let result = session
        .write_stdin_raw_with_meta(
            r#"
brand <- suppressWarnings(system("/usr/sbin/sysctl machdep.cpu.brand_string", intern = TRUE))
suppressWarnings({
  logical <- parallel::detectCores(logical = TRUE)
  physical <- parallel::detectCores(logical = FALSE)
  cat("SYSCTL_BRAND_OK=", length(brand) > 0 && any(grepl("Intel|Apple", brand)), "\n", sep = "")
  cat("DETECT_CORES_OK=", is.numeric(logical) && is.numeric(physical) && logical >= physical && physical >= 1, "\n", sep = "")
})
"#,
            Some(10.0),
            Some(minimal_read_meta(temp.path())),
        )
        .await?;
    let text = collect_text(&result);
    session.cancel().await?;

    if backend_unavailable(&text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        return Ok(());
    }
    assert!(
        text.contains("SYSCTL_BRAND_OK=TRUE"),
        "expected Quarto-style sysctl probe under minimal metadata, got: {text}"
    );
    assert!(
        text.contains("DETECT_CORES_OK=TRUE"),
        "expected R parallel::detectCores under minimal metadata, got: {text}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_rejects_restricted_read_only_meta() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let session = spawn_inherit_server(temp.path()).await?;
    let result = session
        .write_stdin_raw_with_meta(
            "1+1",
            Some(2.0),
            Some(read_only_restricted_access_meta(temp.path())),
        )
        .await?;
    let text = collect_text(&result);
    session.cancel().await?;

    if backend_unavailable(&text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        return Ok(());
    }
    assert_eq!(
        result.is_error,
        Some(true),
        "expected restricted read-only metadata to be reported as an MCP tool error"
    );
    assert!(
        text.contains("requires at least one readable entry"),
        "expected restricted read-only metadata rejection, got: {text}"
    );
    assert!(
        !text.contains("[1] 2"),
        "did not expect input to run after unsupported restricted read-only metadata, got: {text}"
    );
    Ok(())
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_accepts_full_write_network_restricted_meta() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let session = spawn_inherit_server(temp.path()).await?;
    let result = session
        .write_stdin_raw_with_meta(
            "1+1",
            Some(2.0),
            Some(full_write_network_restricted_meta(temp.path())),
        )
        .await?;
    let text = collect_text(&result);
    session.cancel().await?;

    assert!(
        result.is_error != Some(true),
        "expected full-write restricted-network metadata to be accepted, got: {text}"
    );
    assert!(
        text.contains("[1] 2"),
        "expected input to run after full-write restricted-network metadata, got: {text}"
    );
    Ok(())
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_accepts_root_write_network_restricted_meta() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let session = spawn_inherit_server(temp.path()).await?;
    let result = session
        .write_stdin_raw_with_meta(
            "1+1",
            Some(2.0),
            Some(root_write_network_restricted_meta(temp.path())),
        )
        .await?;
    let text = collect_text(&result);
    session.cancel().await?;

    assert!(
        result.is_error != Some(true),
        "expected root-write restricted-network metadata to be accepted, got: {text}"
    );
    assert!(
        text.contains("[1] 2"),
        "expected input to run after root-write restricted-network metadata, got: {text}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_accepts_read_only_network_access_meta() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let session = spawn_inherit_server(temp.path()).await?;
    let result = session
        .write_stdin_raw_with_meta(
            "1+1",
            Some(2.0),
            Some(read_only_network_access_meta(temp.path())),
        )
        .await?;
    let text = collect_text(&result);
    session.cancel().await?;

    if backend_unavailable(&text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        return Ok(());
    }
    assert_eq!(
        result.is_error,
        Some(false),
        "expected read-only network metadata to be accepted"
    );
    assert!(
        text.contains("[1] 2"),
        "expected input to run after read-only network metadata, got: {text}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_ignores_unknown_special_path_entries() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let session = spawn_inherit_server(temp.path()).await?;
    let result = session
        .write_stdin_raw_with_meta(
            "1+1",
            Some(2.0),
            Some(read_only_with_unknown_special_meta(temp.path())),
        )
        .await?;
    let text = collect_text(&result);
    session.cancel().await?;

    if backend_unavailable(&text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        return Ok(());
    }
    assert!(
        result.is_error != Some(true),
        "expected unknown special-path metadata to be accepted, got: {text}"
    );
    assert!(
        !text.contains("failed to parse Codex sandbox state metadata"),
        "unknown special-path metadata should not fail deserialization: {text}"
    );
    assert!(
        text.contains("[1] 2"),
        "expected input to run after unknown special-path metadata, got: {text}"
    );
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
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
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
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
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
async fn sandbox_inherit_pending_ctrl_c_tail_applies_new_meta_before_running_tail_files()
-> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let target = outside_workspace_target("ctrl-c-tail-files")?;
    let _ = std::fs::remove_file(&target);
    let session = spawn_inherit_files_server(temp.path(), Vec::new()).await?;
    let first = session
        .write_stdin_raw_with_meta("1+1", Some(2.0), Some(workspace_write_meta(temp.path())))
        .await?;
    let first_text = collect_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let timed_out = session
        .write_stdin_raw_with_meta(
            timeout_then_tail_code(),
            Some(0.05),
            Some(workspace_write_meta(temp.path())),
        )
        .await?;
    let timed_out_text = collect_text(&timed_out);
    if backend_unavailable(&timed_out_text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    tokio::time::sleep(test_delay_ms(260, 700)).await;

    let mut text = collect_text(
        &session
            .write_stdin_raw_with_meta(
                format!("\u{3}{}", write_file_code(&target)?),
                Some(10.0),
                Some(full_access_meta(temp.path())),
            )
            .await?,
    );
    for _ in 0..20 {
        if !text.contains("[repl] input discarded while worker busy")
            && !text.contains("<<repl status: busy")
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        text = collect_text(&session.write_stdin_raw_with("", Some(0.5)).await?);
    }
    let file_text = std::fs::read_to_string(&target).ok();
    let _ = std::fs::remove_file(&target);
    session.cancel().await?;

    assert!(
        text.contains("WRITE_OK"),
        "expected ctrl-c tail to execute under the updated full-access sandbox, got: {text}"
    );
    assert!(
        !text.contains("WRITE_ERROR:"),
        "did not expect ctrl-c tail to keep the previous sandbox permissions, got: {text}"
    );
    assert_eq!(
        file_text.as_deref().map(str::trim_end),
        Some("allowed"),
        "expected ctrl-c tail to write outside the workspace under the new sandbox"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_pending_ctrl_c_tail_applies_new_meta_before_running_tail_pager()
-> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let target = outside_workspace_target("ctrl-c-tail-pager")?;
    let _ = std::fs::remove_file(&target);
    let session = spawn_inherit_pager_server(temp.path(), 120).await?;
    let first = session
        .write_stdin_raw_with_meta("1+1", Some(2.0), Some(workspace_write_meta(temp.path())))
        .await?;
    let first_text = collect_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let timed_out = session
        .write_stdin_raw_with_meta(
            timeout_then_tail_code(),
            Some(0.05),
            Some(workspace_write_meta(temp.path())),
        )
        .await?;
    let timed_out_text = collect_text(&timed_out);
    if backend_unavailable(&timed_out_text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    tokio::time::sleep(test_delay_ms(260, 700)).await;

    let mut text = collect_text(
        &session
            .write_stdin_raw_with_meta(
                format!("\u{3}{}", write_file_code(&target)?),
                Some(10.0),
                Some(full_access_meta(temp.path())),
            )
            .await?,
    );
    for _ in 0..20 {
        if !text.contains("[repl] input discarded while worker busy")
            && !text.contains("<<repl status: busy")
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        text = collect_text(&session.write_stdin_raw_with("", Some(0.5)).await?);
    }
    let file_text = std::fs::read_to_string(&target).ok();
    let _ = std::fs::remove_file(&target);
    session.cancel().await?;

    assert!(
        text.contains("WRITE_OK"),
        "expected pager ctrl-c tail to execute under the updated full-access sandbox, got: {text}"
    );
    assert!(
        !text.contains("WRITE_ERROR:"),
        "did not expect pager ctrl-c tail to keep the previous sandbox permissions, got: {text}"
    );
    assert_eq!(
        file_text.as_deref().map(str::trim_end),
        Some("allowed"),
        "expected pager ctrl-c tail to write outside the workspace under the new sandbox"
    );
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
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
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
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
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
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
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
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
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

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_ctrl_d_does_not_spawn_worker_just_to_stage_state() -> TestResult<()> {
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
        .write_stdin_raw_unterminated_with_meta(
            "\u{4}",
            Some(2.0),
            Some(workspace_write_meta(temp.path())),
        )
        .await?;
    let text = collect_text(&result);
    assert!(
        text.contains("new session started"),
        "expected bare Ctrl-D with sandbox metadata to restart the session, got: {text}"
    );
    session.cancel().await?;

    let events = latest_debug_events(&debug_dir)?;
    let saw_restart = events
        .iter()
        .any(|entry| entry["event"] == "worker_restart_begin");
    assert!(saw_restart, "expected bare Ctrl-D to emit a restart event");
    let saw_spawn = events
        .iter()
        .any(|entry| entry["event"] == "worker_spawn_begin");
    assert!(
        !saw_spawn,
        "did not expect bare Ctrl-D to spawn a worker just to stage sandbox metadata"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_first_ctrl_d_tail_stages_current_meta_before_restart() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let session = spawn_inherit_server(temp.path()).await?;
    let result = session
        .write_stdin_raw_with_meta(
            "\u{4}1+1",
            Some(2.0),
            Some(workspace_write_meta(temp.path())),
        )
        .await?;
    let text = collect_text(&result);
    session.cancel().await?;

    if backend_unavailable(&text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        return Ok(());
    }
    assert!(
        text.contains("new session started"),
        "expected Ctrl-D tail to restart before running tail input, got: {text}"
    );
    assert!(
        text.contains("[1] 2"),
        "expected Ctrl-D tail to run with current sandbox metadata, got: {text}"
    );
    assert!(
        !text.contains(MISSING_INHERITED_STATE_MESSAGE),
        "did not expect valid current metadata to fail closed, got: {text}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_pending_ctrl_c_tail_stages_current_meta_before_session_end_reset()
-> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let debug_dir = temp.path().join("debug");
    let session = spawn_inherit_files_server(
        temp.path(),
        vec![(
            "MCP_REPL_DEBUG_DIR".to_string(),
            debug_dir.to_string_lossy().to_string(),
        )],
    )
    .await?;

    let timeout = session
        .write_stdin_raw_with_meta(
            interrupt_then_exit_code(),
            Some(0.2),
            Some(read_only_meta(temp.path())),
        )
        .await?;
    let timeout_text = collect_text(&timeout);
    if backend_unavailable(&timeout_text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        timeout_text.contains("<<repl status: busy"),
        "expected interrupt setup request to time out, got: {timeout_text}"
    );

    let tail = session
        .write_stdin_raw_with_meta(
            "\u{3}1+1",
            Some(10.0),
            Some(workspace_write_meta(temp.path())),
        )
        .await?;
    let tail_text = collect_text(&tail);
    session.cancel().await?;

    if backend_unavailable(&tail_text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        return Ok(());
    }
    if tail_text.contains("<<repl status: busy") {
        eprintln!("interrupt tail did not complete before timeout; skipping");
        return Ok(());
    }
    assert!(
        tail_text.contains("[1] 2"),
        "expected Ctrl-C tail to run after the exiting interrupt handler, got: {tail_text}"
    );

    let policy_types = worker_spawn_policy_types(&latest_debug_events(&debug_dir)?);
    let read_only_spawns = policy_types
        .iter()
        .filter(|policy_type| policy_type.as_str() == "read-only")
        .count();
    assert_eq!(
        read_only_spawns, 1,
        "expected only the initial worker to spawn with read-only metadata, got policy sequence: {policy_types:?}"
    );
    assert!(
        policy_types
            .iter()
            .any(|policy_type| policy_type == "workspace-write"),
        "expected the interrupt tail replacement worker to use workspace-write metadata, got policy sequence: {policy_types:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_pending_bare_ctrl_c_stages_current_meta_before_session_end_reset()
-> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let debug_dir = temp.path().join("debug");
    let session = spawn_inherit_files_server(
        temp.path(),
        vec![(
            "MCP_REPL_DEBUG_DIR".to_string(),
            debug_dir.to_string_lossy().to_string(),
        )],
    )
    .await?;

    let timeout = session
        .write_stdin_raw_with_meta(
            interrupt_then_exit_code(),
            Some(0.2),
            Some(workspace_write_meta(temp.path())),
        )
        .await?;
    let timeout_text = collect_text(&timeout);
    if backend_unavailable(&timeout_text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        timeout_text.contains("<<repl status: busy"),
        "expected interrupt setup request to time out, got: {timeout_text}"
    );

    let interrupt = session
        .write_stdin_raw_unterminated_with_meta(
            "\u{3}",
            Some(10.0),
            Some(read_only_meta(temp.path())),
        )
        .await?;
    let interrupt_text = collect_text(&interrupt);
    session.cancel().await?;

    if interrupt_text.contains("<<repl status: busy") {
        eprintln!("bare interrupt did not complete before timeout; skipping");
        return Ok(());
    }

    let policy_types = worker_spawn_policy_types(&latest_debug_events(&debug_dir)?);
    assert_eq!(
        policy_types,
        vec!["workspace-write".to_string(), "read-only".to_string()],
        "expected bare Ctrl-C reset to use the current read-only metadata, got policy sequence: {policy_types:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_pending_bare_ctrl_c_keeps_old_meta_when_worker_survives() -> TestResult<()>
{
    let _guard = test_guard();
    let temp = tempdir()?;
    let debug_dir = temp.path().join("debug");
    let session = spawn_inherit_files_server(
        temp.path(),
        vec![(
            "MCP_REPL_DEBUG_DIR".to_string(),
            debug_dir.to_string_lossy().to_string(),
        )],
    )
    .await?;

    let timeout = session
        .write_stdin_raw_with_meta(
            interrupt_then_prompt_code(),
            Some(0.2),
            Some(workspace_write_meta(temp.path())),
        )
        .await?;
    let timeout_text = collect_text(&timeout);
    if backend_unavailable(&timeout_text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        timeout_text.contains("<<repl status: busy"),
        "expected interrupt setup request to time out, got: {timeout_text}"
    );

    let interrupt = session
        .write_stdin_raw_unterminated_with_meta(
            "\u{3}",
            Some(10.0),
            Some(read_only_meta(temp.path())),
        )
        .await?;
    let interrupt_text = collect_text(&interrupt);
    if interrupt_text.contains("<<repl status: busy") {
        eprintln!("bare interrupt did not complete before timeout; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        interrupt_text.contains("INTERRUPT_PROMPT"),
        "expected interrupt handler to return to the existing worker prompt, got: {interrupt_text}"
    );

    let follow_up = session
        .write_stdin_raw_with_meta(
            "cat(\"AFTER_INTERRUPT\\n\")",
            Some(5.0),
            Some(read_only_meta(temp.path())),
        )
        .await?;
    let follow_up_text = collect_text(&follow_up);
    session.cancel().await?;
    assert!(
        follow_up_text.contains("AFTER_INTERRUPT"),
        "expected follow-up to run after the bare interrupt, got: {follow_up_text}"
    );

    let policy_types = worker_spawn_policy_types(&latest_debug_events(&debug_dir)?);
    assert_eq!(
        policy_types,
        vec!["workspace-write".to_string(), "read-only".to_string()],
        "expected same-metadata follow-up to respawn under read-only after a surviving Ctrl-C, got policy sequence: {policy_types:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_empty_poll_stages_current_meta_before_session_end_reset() -> TestResult<()>
{
    let _guard = test_guard();
    let temp = tempdir()?;
    let debug_dir = temp.path().join("debug");
    let session = spawn_inherit_files_server(
        temp.path(),
        vec![(
            "MCP_REPL_DEBUG_DIR".to_string(),
            debug_dir.to_string_lossy().to_string(),
        )],
    )
    .await?;

    let timeout = session
        .write_stdin_raw_with_meta(
            timeout_then_exit_code(),
            Some(0.05),
            Some(workspace_write_meta(temp.path())),
        )
        .await?;
    let timeout_text = collect_text(&timeout);
    if backend_unavailable(&timeout_text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        timeout_text.contains("<<repl status: busy"),
        "expected exit setup request to time out, got: {timeout_text}"
    );
    tokio::time::sleep(test_delay_ms(350, 700)).await;

    let poll = session
        .write_stdin_raw_with_meta("", Some(5.0), Some(read_only_meta(temp.path())))
        .await?;
    let poll_text = collect_text(&poll);
    session.cancel().await?;

    if poll_text.contains("<<repl status: busy") {
        eprintln!("empty poll did not observe session end before timeout; skipping");
        return Ok(());
    }

    let policy_types = worker_spawn_policy_types(&latest_debug_events(&debug_dir)?);
    assert_eq!(
        policy_types,
        vec!["workspace-write".to_string(), "read-only".to_string()],
        "expected empty-poll reset to use the current read-only metadata, got policy sequence: {policy_types:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_empty_poll_without_meta_defers_session_end_respawn() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let session = spawn_inherit_files_server(temp.path(), Vec::new()).await?;

    let timeout = session
        .write_stdin_raw_with_meta(
            timeout_then_exit_code(),
            Some(0.05),
            Some(workspace_write_meta(temp.path())),
        )
        .await?;
    let timeout_text = collect_text(&timeout);
    if backend_unavailable(&timeout_text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        timeout_text.contains("<<repl status: busy"),
        "expected exit setup request to time out, got: {timeout_text}"
    );
    tokio::time::sleep(test_delay_ms(350, 700)).await;

    let poll = session.write_stdin_raw_with("", Some(5.0)).await?;
    let poll_text = collect_text(&poll);
    assert_ne!(
        poll.is_error,
        Some(true),
        "did not expect omitted metadata empty poll to fail while draining local output, got: {poll_text}"
    );
    assert!(
        poll_text.contains("session ended")
            || poll_text.contains("ipc disconnected while waiting for request completion"),
        "expected empty poll without metadata to report the ended session locally, got: {poll_text}"
    );
    assert!(
        !poll_text.contains(MISSING_INHERITED_STATE_MESSAGE),
        "did not expect empty poll without metadata to respawn while draining local output, got: {poll_text}"
    );

    let follow_up = session.write_stdin_raw_with("", Some(5.0)).await?;
    let follow_up_text = collect_text(&follow_up);
    session.cancel().await?;

    assert_eq!(
        follow_up.is_error,
        Some(true),
        "expected later spawn-needed empty poll without metadata to fail, got: {follow_up_text}"
    );
    assert!(
        follow_up_text.contains(MISSING_INHERITED_STATE_MESSAGE),
        "expected missing sandbox metadata error once a fresh worker was needed, got: {follow_up_text}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_idle_ctrl_d_without_state_meta_does_not_restart() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let session = spawn_inherit_server(temp.path()).await?;
    let first = session
        .write_stdin_raw_with_meta("x <- 1", Some(2.0), Some(workspace_write_meta(temp.path())))
        .await?;
    let first_text = collect_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let restart_error = session
        .write_stdin_raw_unterminated_with("\u{4}", Some(2.0))
        .await?;
    assert_eq!(
        restart_error.is_error,
        Some(true),
        "expected missing metadata Ctrl-D to be reported as an MCP tool error"
    );
    let restart_error_text = collect_text(&restart_error);
    assert!(
        restart_error_text.contains(MISSING_INHERITED_STATE_MESSAGE),
        "expected missing metadata error after bare Ctrl-D, got: {restart_error_text}"
    );
    assert!(
        !restart_error_text.contains("new session started"),
        "did not expect missing metadata Ctrl-D to reset under stale state, got: {restart_error_text}"
    );

    let probe = session
        .write_stdin_raw_with_meta(
            variable_probe_code(),
            Some(2.0),
            Some(workspace_write_meta(temp.path())),
        )
        .await?;
    let probe_text = collect_text(&probe);
    session.cancel().await?;

    assert!(
        probe_text.contains("X_EXISTS:TRUE"),
        "expected missing metadata Ctrl-D to preserve the existing session, got: {probe_text}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_idle_ctrl_d_with_bad_meta_does_not_restart() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let session = spawn_inherit_server(temp.path()).await?;
    let first = session
        .write_stdin_raw_with_meta("x <- 1", Some(2.0), Some(workspace_write_meta(temp.path())))
        .await?;
    let first_text = collect_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let restart_error = session
        .write_stdin_raw_unterminated_with_meta(
            "\u{4}",
            Some(2.0),
            Some(json!({ SANDBOX_STATE_META_CAPABILITY: "invalid" })),
        )
        .await?;
    assert_eq!(
        restart_error.is_error,
        Some(true),
        "expected malformed metadata Ctrl-D to be reported as an MCP tool error"
    );
    let restart_error_text = collect_text(&restart_error);
    assert!(
        restart_error_text.contains("failed to parse Codex sandbox state metadata"),
        "expected malformed metadata error after bare Ctrl-D, got: {restart_error_text}"
    );
    assert!(
        !restart_error_text.contains("new session started"),
        "did not expect malformed metadata Ctrl-D to reset under stale state, got: {restart_error_text}"
    );

    let probe = session
        .write_stdin_raw_with_meta(
            variable_probe_code(),
            Some(2.0),
            Some(workspace_write_meta(temp.path())),
        )
        .await?;
    let probe_text = collect_text(&probe);
    session.cancel().await?;

    assert!(
        probe_text.contains("X_EXISTS:TRUE"),
        "expected malformed metadata Ctrl-D to preserve the existing session, got: {probe_text}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_inherit_idle_ctrl_d_tail_with_bad_meta_does_not_run_tail() -> TestResult<()> {
    let _guard = test_guard();
    let temp = tempdir()?;
    let session = spawn_inherit_server(temp.path()).await?;
    let first = session
        .write_stdin_raw_with_meta("1+1", Some(2.0), Some(workspace_write_meta(temp.path())))
        .await?;
    let first_text = collect_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("sandbox_state_meta backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let restart_error = session
        .write_stdin_raw_with_meta(
            "\u{4}x <- 2",
            Some(2.0),
            Some(json!({ SANDBOX_STATE_META_CAPABILITY: "invalid" })),
        )
        .await?;
    assert_eq!(
        restart_error.is_error,
        Some(true),
        "expected malformed metadata Ctrl-D tail to be reported as an MCP tool error"
    );
    let restart_error_text = collect_text(&restart_error);
    assert!(
        restart_error_text.contains("failed to parse Codex sandbox state metadata"),
        "expected malformed metadata error after bare Ctrl-D tail, got: {restart_error_text}"
    );
    assert!(
        !restart_error_text.contains("new session started"),
        "did not expect malformed metadata Ctrl-D tail to reset under stale state, got: {restart_error_text}"
    );

    let probe = session
        .write_stdin_raw_with_meta(
            variable_probe_code(),
            Some(2.0),
            Some(workspace_write_meta(temp.path())),
        )
        .await?;
    let probe_text = collect_text(&probe);
    session.cancel().await?;

    assert!(
        probe_text.contains("X_EXISTS:FALSE"),
        "expected malformed metadata Ctrl-D tail to avoid running fresh tail input, got: {probe_text}"
    );
    Ok(())
}
