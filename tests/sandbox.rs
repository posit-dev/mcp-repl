mod common;

#[cfg(target_os = "windows")]
use std::path::{Path, PathBuf};
#[cfg(target_os = "windows")]
use std::process::Command;
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
use std::time::{SystemTime, UNIX_EPOCH};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::{
    path::{Path, PathBuf},
    process::Command,
};

use common::TestResult;
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
use rmcp::model::CallToolResult;
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
use rmcp::model::RawContent;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use tokio::io::AsyncWriteExt;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use tokio::net::TcpListener;

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[derive(Debug)]
struct TempDirInfo {
    mcp_tmpdir: String,
    tmpdir: String,
    r_tmpdir: String,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[derive(Debug)]
struct TempDirStatus {
    info: TempDirInfo,
    marker_exists: bool,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
const SESSION_MARKER_FILE: &str = "mcp-repl-session-marker.txt";
const SANDBOX_PAGER_PAGE_CHARS: u64 = 2048;

#[cfg(target_os = "macos")]
fn sandbox_available() -> bool {
    common::sandbox_exec_available()
}

#[cfg(target_os = "linux")]
fn sandbox_available() -> bool {
    true
}

#[cfg(target_os = "linux")]
fn linux_bwrap_available() -> bool {
    let absolute = PathBuf::from("/usr/bin/bwrap");
    if absolute.is_file() {
        return true;
    }

    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path)
        .map(|dir| dir.join("bwrap"))
        .any(|candidate| candidate.is_file())
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
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
            !(trimmed.starts_with("> ")
                || trimmed.starts_with("+ ")
                || trimmed == ">"
                || trimmed.starts_with("[repl] input:")
                || trimmed.starts_with("[repl] echoed input"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn sandbox_state_read_only() -> String {
    serde_json::json!({
        "sandboxPolicy": {
            "type": "read-only",
        }
    })
    .to_string()
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
fn sandbox_state_workspace_write(network_access: bool) -> String {
    let mut policy = serde_json::Map::new();
    policy.insert(
        "type".to_string(),
        serde_json::Value::String("workspace-write".to_string()),
    );
    if network_access {
        policy.insert("network_access".to_string(), serde_json::Value::Bool(true));
    }
    serde_json::json!({
        "sandboxPolicy": serde_json::Value::Object(policy),
    })
    .to_string()
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
fn sandbox_state_workspace_write_with_roots(
    network_access: bool,
    writable_roots: Vec<PathBuf>,
) -> String {
    let roots = writable_roots
        .into_iter()
        .map(|root| root.to_string_lossy().to_string())
        .collect::<Vec<_>>();
    serde_json::json!({
        "sandboxPolicy": {
            "type": "workspace-write",
            "network_access": network_access,
            "writable_roots": roots,
        }
    })
    .to_string()
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn sandbox_state_full_access() -> String {
    serde_json::json!({
        "sandboxPolicy": {
            "type": "danger-full-access",
        }
    })
    .to_string()
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn extract_prefixed_value(text: &str, prefix: &str) -> Option<String> {
    for line in text.lines() {
        let trimmed = line.trim_start();
        if let Some(value) = trimmed.strip_prefix(prefix) {
            return Some(value.trim().to_string());
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn bwrap_loopback_unavailable(text: &str) -> bool {
    text.contains("Failed RTM_NEWADDR")
        || (text.contains("loopback") && text.contains("Operation not permitted"))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn sandbox_backend_unavailable(text: &str) -> bool {
    text.contains("Fatal error: cannot create 'R_TempDir'")
        || text.contains("failed to start R session")
        || text.contains("worker exited with signal")
        || text.contains("worker exited with status")
        || text.contains("worker io error: Broken pipe")
        || text.contains("unable to initialize the JIT")
        || text.contains("libR.so: cannot open shared object file")
        || text.contains("options(\"defaultPackages\") was not found")
        || text.contains("worker protocol error: ipc disconnected while waiting for backend info")
        || text.contains(
            "worker protocol error: ipc disconnected while waiting for request completion",
        )
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn skip_backend_unavailable(test_name: &str, text: &str) -> bool {
    if sandbox_backend_unavailable(text) {
        eprintln!("{test_name} backend unavailable in this environment; skipping");
        return true;
    }
    false
}

#[cfg(target_os = "linux")]
fn bwrap_worker_unavailable(text: &str) -> bool {
    bwrap_loopback_unavailable(text) || sandbox_backend_unavailable(text)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn extract_prefixed_value_does_not_match_substrings() {
    let text = "R_SESSION_TMPDIR=/tmp/session\nTMPDIR=/tmp/tmpdir\n";
    assert_eq!(
        extract_prefixed_value(text, "TMPDIR=").as_deref(),
        Some("/tmp/tmpdir")
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn assert_tempdir_layout(info: &TempDirInfo, context: &str) {
    assert!(
        !info.mcp_tmpdir.is_empty(),
        "missing MCP_TMPDIR output: {context}"
    );
    assert!(!info.tmpdir.is_empty(), "missing TMPDIR output: {context}");
    assert!(
        !info.r_tmpdir.is_empty(),
        "missing R_TMPDIR output: {context}"
    );
    assert!(
        std::path::Path::new(&info.mcp_tmpdir).is_absolute(),
        "expected MCP_TMPDIR to be absolute, got: {}",
        info.mcp_tmpdir
    );
    assert!(
        std::path::Path::new(&info.r_tmpdir).is_absolute(),
        "expected R_TMPDIR to be absolute, got: {}",
        info.r_tmpdir
    );
    assert_eq!(
        info.mcp_tmpdir, info.tmpdir,
        "expected TMPDIR to match MCP_TMPDIR, got: {}",
        info.tmpdir
    );
    assert!(
        std::path::Path::new(&info.r_tmpdir).starts_with(&info.mcp_tmpdir),
        "expected R_TMPDIR to be under MCP_TMPDIR, got: {} (mcp: {})",
        info.r_tmpdir,
        info.mcp_tmpdir
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
async fn fetch_tempdir_info(
    session: &mut common::McpTestSession,
) -> TestResult<Option<TempDirInfo>> {
    let code = r#"
cat("MCP_TMPDIR=", Sys.getenv("MCP_REPL_R_SESSION_TMPDIR"), "\n", sep = "")
cat("TMPDIR=", Sys.getenv("TMPDIR"), "\n", sep = "")
cat("R_TMPDIR=", tempdir(), "\n", sep = "")
marker <- file.path(Sys.getenv("TMPDIR"), "mcp-repl-session-marker.txt")
tryCatch({
  writeLines("marker", marker)
  cat("MARKER_OK\n")
}, error = function(e) {
  message("MARKER_ERROR:", conditionMessage(e))
})
tf <- tempfile()
tryCatch({
  writeLines("ok", tf)
  cat("TEMPFILE_OK\n")
}, error = function(e) {
  message("TEMPFILE_ERROR:", conditionMessage(e))
})
"#;
    let result = session.write_stdin_raw_with(code, Some(10.0)).await?;
    let text = collect_text(&result);
    if sandbox_backend_unavailable(&text) {
        return Ok(None);
    }
    assert!(
        text.contains("MARKER_OK"),
        "expected marker write to succeed, got: {text}"
    );
    assert!(
        !text.contains("MARKER_ERROR:"),
        "marker write unexpectedly failed: {text}"
    );
    assert!(
        text.contains("TEMPFILE_OK"),
        "expected temp file write to succeed, got: {text}"
    );
    assert!(
        !text.contains("TEMPFILE_ERROR:"),
        "temp file write unexpectedly failed: {text}"
    );

    let mcp_tmpdir = extract_prefixed_value(&text, "MCP_TMPDIR=").unwrap_or_default();
    let tmpdir = extract_prefixed_value(&text, "TMPDIR=").unwrap_or_default();
    let r_tmpdir = extract_prefixed_value(&text, "R_TMPDIR=").unwrap_or_default();

    let info = TempDirInfo {
        mcp_tmpdir,
        tmpdir,
        r_tmpdir,
    };
    assert_tempdir_layout(&info, &text);
    Ok(Some(info))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
async fn fetch_tempdir_status(
    session: &mut common::McpTestSession,
    marker_path: &str,
) -> TestResult<Option<TempDirStatus>> {
    let marker = r_string(marker_path);
    let code = format!(
        r#"
cat("MCP_TMPDIR=", Sys.getenv("MCP_REPL_R_SESSION_TMPDIR"), "\n", sep = "")
cat("TMPDIR=", Sys.getenv("TMPDIR"), "\n", sep = "")
cat("R_TMPDIR=", tempdir(), "\n", sep = "")
cat("MARKER_EXISTS=", file.exists({marker}), "\n", sep = "")
"#
    );
    let result = session.write_stdin_raw_with(code, Some(10.0)).await?;
    let text = collect_text(&result);
    if sandbox_backend_unavailable(&text) {
        return Ok(None);
    }
    let marker_exists = text.contains("MARKER_EXISTS=TRUE");
    let info = TempDirInfo {
        mcp_tmpdir: extract_prefixed_value(&text, "MCP_TMPDIR=").unwrap_or_default(),
        tmpdir: extract_prefixed_value(&text, "TMPDIR=").unwrap_or_default(),
        r_tmpdir: extract_prefixed_value(&text, "R_TMPDIR=").unwrap_or_default(),
    };
    assert_tempdir_layout(&info, &text);
    Ok(Some(TempDirStatus {
        info,
        marker_exists,
    }))
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
async fn spawn_server_with_sandbox_state(state: String) -> TestResult<common::McpTestSession> {
    let args = sandbox_args_from_state(&state)?;
    common::spawn_server_with_args_env_and_pager_page_chars(
        args,
        Vec::new(),
        SANDBOX_PAGER_PAGE_CHARS,
    )
    .await
}

#[cfg(target_os = "windows")]
async fn spawn_server_with_sandbox_state_in_cwd(
    state: String,
    cwd: &Path,
) -> TestResult<common::McpTestSession> {
    let args = sandbox_args_from_state(&state)?;
    common::spawn_server_with_args_env_and_cwd_and_pager_page_chars(
        args,
        Vec::new(),
        Some(cwd.to_path_buf()),
        SANDBOX_PAGER_PAGE_CHARS,
    )
    .await
}

#[cfg(target_os = "windows")]
fn temp_workspace_root() -> TestResult<tempfile::TempDir> {
    Ok(tempfile::tempdir()?)
}

#[cfg(target_os = "windows")]
async fn spawn_server_with_sandbox_state_in_temp_cwd(
    state: String,
) -> TestResult<(common::McpTestSession, tempfile::TempDir, PathBuf)> {
    let workspace = temp_workspace_root()?;
    let cwd = workspace.path().join("workspace");
    std::fs::create_dir_all(&cwd)?;
    let session = spawn_server_with_sandbox_state_in_cwd(state, &cwd).await?;
    Ok((session, workspace, cwd))
}

#[cfg(target_os = "windows")]
fn temp_workspace_root_with_git_dir() -> TestResult<tempfile::TempDir> {
    let workspace = temp_workspace_root()?;
    std::fs::create_dir_all(workspace.path().join(".git"))?;
    Ok(workspace)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
async fn spawn_server_with_sandbox_state_and_env(
    state: String,
    env: Vec<(String, String)>,
) -> TestResult<common::McpTestSession> {
    let args = sandbox_args_from_state(&state)?;
    common::spawn_server_with_args_env_and_pager_page_chars(args, env, SANDBOX_PAGER_PAGE_CHARS)
        .await
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
fn sandbox_args_from_state(state: &str) -> TestResult<Vec<String>> {
    let parsed: serde_json::Value = serde_json::from_str(state)?;
    let policy = parsed
        .get("sandboxPolicy")
        .and_then(serde_json::Value::as_object)
        .ok_or("missing sandboxPolicy object in test fixture")?;
    let policy_type = policy
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or("missing sandboxPolicy.type in test fixture")?;

    let mut args = vec!["--sandbox".to_string(), policy_type.to_string()];
    match policy_type {
        "read-only" | "danger-full-access" => {}
        "workspace-write" => {
            if let Some(network_access) = policy
                .get("network_access")
                .and_then(serde_json::Value::as_bool)
            {
                args.push("--config".to_string());
                args.push(format!(
                    "sandbox_workspace_write.network_access={network_access}"
                ));
            }

            if let Some(roots) = policy
                .get("writable_roots")
                .and_then(serde_json::Value::as_array)
            {
                for root in roots {
                    let root = root.as_str().ok_or(
                        "sandboxPolicy.writable_roots must be an array of strings in test fixture",
                    )?;
                    args.push("--add-writable-root".to_string());
                    args.push(root.to_string());
                }
            }
        }
        other => {
            return Err(format!("unsupported sandboxPolicy.type in test fixture: {other}").into());
        }
    }
    Ok(args)
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
fn r_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| format!("\"{value}\""))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn unique_path(root: &Path, label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    root.join(format!("mcp-repl-sandbox-{label}-{nanos}.txt"))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn write_test_code(target: &Path) -> String {
    let target = r_string(&target.to_string_lossy());
    format!(
        r#"
target <- {target}
tryCatch({{
  writeLines("ok", target)
  cat("WRITE_OK\n")
}}, error = function(e) {{
  message("WRITE_ERROR:", conditionMessage(e))
}})
"#
    )
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
async fn start_loopback_server() -> TestResult<std::net::SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        if let Ok((mut socket, _)) = listener.accept().await {
            let _ = socket.write_all(b"ok").await;
        }
    });
    Ok(addr)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
async fn start_loopback_server_if_available() -> TestResult<Option<std::net::SocketAddr>> {
    match start_loopback_server().await {
        Ok(addr) => Ok(Some(addr)),
        Err(err) => {
            let message = err.to_string();
            if message.contains("Operation not permitted") || message.contains("Permission denied")
            {
                eprintln!("loopback unavailable in this environment; skipping");
                Ok(None)
            } else {
                Err(err)
            }
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn network_test_code(addr: std::net::SocketAddr) -> String {
    let host = r_string(&addr.ip().to_string());
    let port = addr.port();
    format!(
        r#"
tryCatch({{
  con <- socketConnection({host}, {port}, blocking = TRUE, open = "r+", timeout = 1)
  on.exit(close(con))
  cat("NETWORK_OK\n")
}}, error = function(e) {{
  message("NETWORK_ERROR:", conditionMessage(e))
}})
"#
    )
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn reticulate_cache_dir() -> Option<PathBuf> {
    let output = Command::new("Rscript")
        .args(["-e", "cat(reticulate:::reticulate_cache_dir())"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let path = stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    Some(PathBuf::from(path))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_read_only_blocks_workspace_writes() -> TestResult<()> {
    if !sandbox_available() {
        eprintln!("sandbox-exec unavailable; skipping");
        return Ok(());
    }
    let repo_root = std::env::current_dir()?;
    let target = unique_path(&repo_root, "read-only");
    let mut session = spawn_server_with_sandbox_state(sandbox_state_read_only()).await?;
    let result = session
        .write_stdin_raw_with(write_test_code(&target), Some(10.0))
        .await?;
    let text = collect_text(&result);
    let _ = std::fs::remove_file(&target);
    if skip_backend_unavailable("sandbox_read_only_blocks_workspace_writes", &text) {
        session.cancel().await?;
        return Ok(());
    }

    assert!(
        text.contains("WRITE_ERROR:"),
        "expected write to be blocked, got: {text}"
    );
    assert!(
        !text.contains("WRITE_OK"),
        "write unexpectedly succeeded: {text}"
    );
    session.cancel().await?;
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_workspace_write_allows_workspace_writes() -> TestResult<()> {
    if !sandbox_available() {
        eprintln!("sandbox-exec unavailable; skipping");
        return Ok(());
    }
    let repo_root = std::env::current_dir()?;
    let target = unique_path(&repo_root, "workspace-write");
    let mut session = spawn_server_with_sandbox_state(sandbox_state_workspace_write(false)).await?;
    let result = session
        .write_stdin_raw_with(write_test_code(&target), Some(10.0))
        .await?;
    let text = collect_text(&result);
    let _ = std::fs::remove_file(&target);
    if skip_backend_unavailable("sandbox_workspace_write_allows_workspace_writes", &text) {
        session.cancel().await?;
        return Ok(());
    }

    assert!(
        text.contains("WRITE_OK"),
        "expected write to succeed, got: {text}"
    );
    session.cancel().await?;
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_workspace_write_allows_r_package_cache_root_from_config() -> TestResult<()> {
    if !sandbox_available() {
        eprintln!("sandbox-exec unavailable; skipping");
        return Ok(());
    }
    let Some(home) = std::env::var_os("HOME") else {
        return Ok(());
    };
    let home = PathBuf::from(home);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let root = home.join(format!(".mcp-repl-sandbox-r-cache-probe-{nanos}"));

    let xdg_cache_home = root.join("xdg-cache");
    let r_package_cache_root = xdg_cache_home.join("R");
    let reticulate_uv_cache_root = r_package_cache_root.join("reticulate").join("uv");
    let another_pkg_cache_root = r_package_cache_root.join("otherpkg");
    for path in [&reticulate_uv_cache_root, &another_pkg_cache_root] {
        std::fs::create_dir_all(path)?;
    }

    let mut session = spawn_server_with_sandbox_state(sandbox_state_workspace_write_with_roots(
        false,
        vec![r_package_cache_root.clone()],
    ))
    .await?;

    let targets = vec![
        unique_path(&reticulate_uv_cache_root, "reticulate-uv-cache-root"),
        unique_path(&another_pkg_cache_root, "other-package-cache-root"),
    ];
    for target in &targets {
        let result = session
            .write_stdin_raw_with(write_test_code(target), Some(10.0))
            .await?;
        let text = collect_text(&result);
        if skip_backend_unavailable(
            "sandbox_workspace_write_allows_r_package_cache_root_from_config",
            &text,
        ) {
            session.cancel().await?;
            let _ = std::fs::remove_file(target);
            let _ = std::fs::remove_dir_all(&root);
            return Ok(());
        }
        assert!(
            text.contains("WRITE_OK"),
            "expected write to succeed for {} got: {text}",
            target.display()
        );
        let _ = std::fs::remove_file(target);
    }

    session.cancel().await?;
    let _ = std::fs::remove_dir_all(root);
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_read_only_blocks_r_package_cache_root_writes() -> TestResult<()> {
    if !sandbox_available() {
        eprintln!("sandbox-exec unavailable; skipping");
        return Ok(());
    }
    let Some(home) = std::env::var_os("HOME") else {
        return Ok(());
    };
    let home = PathBuf::from(home);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let root = home.join(format!(".mcp-repl-sandbox-r-cache-probe-read-only-{nanos}"));

    let xdg_cache_home = root.join("xdg-cache");
    let r_package_cache_root = xdg_cache_home.join("R");
    let reticulate_uv_cache_root = r_package_cache_root.join("reticulate").join("uv");
    std::fs::create_dir_all(&reticulate_uv_cache_root)?;

    let env = vec![(
        "R_USER_CACHE_DIR".to_string(),
        xdg_cache_home.to_string_lossy().to_string(),
    )];
    let mut session =
        spawn_server_with_sandbox_state_and_env(sandbox_state_read_only(), env).await?;

    let target = unique_path(
        &reticulate_uv_cache_root,
        "reticulate-uv-cache-root-read-only",
    );
    let result = session
        .write_stdin_raw_with(write_test_code(&target), Some(10.0))
        .await?;
    let text = collect_text(&result);
    if skip_backend_unavailable(
        "sandbox_read_only_blocks_r_package_cache_root_writes",
        &text,
    ) {
        session.cancel().await?;
        let _ = std::fs::remove_file(&target);
        let _ = std::fs::remove_dir_all(&root);
        return Ok(());
    }

    assert!(
        text.contains("WRITE_ERROR:"),
        "expected read-only mode to block write to {} got: {text}",
        target.display()
    );
    assert!(
        !text.contains("WRITE_OK"),
        "write unexpectedly succeeded in read-only mode for {}: {text}",
        target.display()
    );

    session.cancel().await?;
    let _ = std::fs::remove_file(&target);
    let _ = std::fs::remove_dir_all(root);
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_full_access_allows_writes_outside_workspace() -> TestResult<()> {
    if !sandbox_available() {
        eprintln!("sandbox-exec unavailable; skipping");
        return Ok(());
    }
    let target = unique_path(&std::env::temp_dir(), "full-access");
    let mut session = spawn_server_with_sandbox_state(sandbox_state_full_access()).await?;
    let result = session
        .write_stdin_raw_with(write_test_code(&target), Some(10.0))
        .await?;
    let text = collect_text(&result);
    let _ = std::fs::remove_file(&target);
    if skip_backend_unavailable("sandbox_full_access_allows_writes_outside_workspace", &text) {
        session.cancel().await?;
        return Ok(());
    }

    assert!(
        text.contains("WRITE_OK"),
        "expected write to succeed, got: {text}"
    );
    session.cancel().await?;
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_read_only_blocks_network_access() -> TestResult<()> {
    if !sandbox_available() {
        eprintln!("sandbox-exec unavailable; skipping");
        return Ok(());
    }
    let Some(addr) = start_loopback_server_if_available().await? else {
        return Ok(());
    };
    let mut session = spawn_server_with_sandbox_state(sandbox_state_read_only()).await?;
    let result = session
        .write_stdin_raw_with(network_test_code(addr), Some(10.0))
        .await?;
    let text = collect_text(&result);
    if skip_backend_unavailable("sandbox_read_only_blocks_network_access", &text) {
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("NETWORK_ERROR:"),
        "expected network to be blocked, got: {text}"
    );
    assert!(
        !text.contains("NETWORK_OK"),
        "network request unexpectedly succeeded: {text}"
    );
    session.cancel().await?;
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_reticulate_keras_backend() -> TestResult<()> {
    if !sandbox_available() {
        eprintln!("sandbox-exec unavailable; skipping");
        return Ok(());
    }
    let mut writable_roots = Vec::new();
    if let Some(root) = reticulate_cache_dir() {
        writable_roots.push(root);
    }
    let mut session = spawn_server_with_sandbox_state(sandbox_state_workspace_write_with_roots(
        false,
        writable_roots,
    ))
    .await?;

    let code = r#"
if (!requireNamespace("reticulate", quietly = TRUE)) {
  cat("[repl] reticulate not installed\n")
} else if (!requireNamespace("keras3", quietly = TRUE)) {
  cat("[repl] keras3 not installed\n")
} else {
  library(reticulate)
  library(keras3)
  ok <- TRUE
  msg <- NULL
  tryCatch({
    use_backend("jax")
    import("sys")
    cat("[repl] keras-reticulate-ok\n")
  }, error = function(e) {
    ok <<- FALSE
    msg <<- conditionMessage(e)
  })
  if (!ok) {
    cat("[repl] keras-reticulate-error:", msg, "\n", sep = "")
  }
}
"#;

    let result = session.write_stdin_raw_with(code, Some(180.0)).await?;
    let text = collect_text(&result);
    if skip_backend_unavailable("sandbox_reticulate_keras_backend", &text) {
        session.cancel().await?;
        return Ok(());
    }

    if text.contains("[repl] reticulate not installed")
        || text.contains("[repl] keras3 not installed")
        || text.contains("[repl] keras-reticulate-error:Python specified in RETICULATE_PYTHON")
    {
        session.cancel().await?;
        return Ok(());
    }

    assert!(
        !text.contains("[repl] keras-reticulate-error:"),
        "reticulate/keras sandbox run failed: {text}"
    );
    assert!(
        text.contains("[repl] keras-reticulate-ok"),
        "expected keras/reticulate success marker, got: {text}"
    );

    session.cancel().await?;
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_workspace_write_blocks_network_access() -> TestResult<()> {
    if !sandbox_available() {
        eprintln!("sandbox-exec unavailable; skipping");
        return Ok(());
    }
    let Some(addr) = start_loopback_server_if_available().await? else {
        return Ok(());
    };
    let mut session = spawn_server_with_sandbox_state(sandbox_state_workspace_write(false)).await?;
    let result = session
        .write_stdin_raw_with(network_test_code(addr), Some(10.0))
        .await?;
    let text = collect_text(&result);
    if skip_backend_unavailable("sandbox_workspace_write_blocks_network_access", &text) {
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("NETWORK_ERROR:"),
        "expected network to be blocked, got: {text}"
    );
    assert!(
        !text.contains("NETWORK_OK"),
        "network request unexpectedly succeeded: {text}"
    );
    session.cancel().await?;
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_tempdir_stable_across_restart() -> TestResult<()> {
    if !sandbox_available() {
        eprintln!("sandbox-exec unavailable; skipping");
        return Ok(());
    }
    let mut session = spawn_server_with_sandbox_state(sandbox_state_read_only()).await?;
    let Some(first) = fetch_tempdir_info(&mut session).await? else {
        eprintln!(
            "sandbox_tempdir_stable_across_restart backend unavailable in this environment; skipping"
        );
        session.cancel().await?;
        return Ok(());
    };
    let marker_path = PathBuf::from(&first.tmpdir).join(SESSION_MARKER_FILE);
    let marker_path = marker_path.to_string_lossy().to_string();

    session.write_stdin("\u{4}").await;
    let Some(after_restart) = fetch_tempdir_status(&mut session, &marker_path).await? else {
        eprintln!(
            "sandbox_tempdir_stable_across_restart backend unavailable in this environment; skipping"
        );
        session.cancel().await?;
        return Ok(());
    };
    assert_eq!(
        first.mcp_tmpdir, after_restart.info.mcp_tmpdir,
        "expected MCP_TMPDIR to stay stable after restart"
    );
    assert_eq!(
        first.tmpdir, after_restart.info.tmpdir,
        "expected TMPDIR to stay stable after restart"
    );
    assert!(
        !after_restart.marker_exists,
        "expected session marker to be cleared after restart"
    );
    session.cancel().await?;
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_ignores_preexisting_r_session_tmpdir() -> TestResult<()> {
    if !sandbox_available() {
        eprintln!("sandbox-exec unavailable; skipping");
        return Ok(());
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sentinel = format!("/tmp/mcp-repl-preexisting-{nanos}");
    let mut session = common::spawn_server_with_args_env_and_pager_page_chars(
        Vec::new(),
        vec![("R_SESSION_TMPDIR".to_string(), sentinel.clone())],
        SANDBOX_PAGER_PAGE_CHARS,
    )
    .await?;

    let code = r#"
cat("R_SESSION_TMPDIR=", Sys.getenv("R_SESSION_TMPDIR"), "\n", sep = "")
cat("TMPDIR=", Sys.getenv("TMPDIR"), "\n", sep = "")
cat("MCP_TMPDIR=", Sys.getenv("MCP_REPL_R_SESSION_TMPDIR"), "\n", sep = "")
"#;
    let result = session.write_stdin_raw_with(code, Some(10.0)).await?;
    let text = collect_text(&result);
    if skip_backend_unavailable("sandbox_ignores_preexisting_r_session_tmpdir", &text) {
        session.cancel().await?;
        return Ok(());
    }
    let r_session = extract_prefixed_value(&text, "R_SESSION_TMPDIR=").unwrap_or_default();
    let tmpdir = extract_prefixed_value(&text, "TMPDIR=").unwrap_or_default();
    let mcp_tmpdir = extract_prefixed_value(&text, "MCP_TMPDIR=").unwrap_or_default();

    assert!(
        !r_session.is_empty(),
        "expected R_SESSION_TMPDIR to be set by R, got: {text}"
    );
    assert_ne!(
        r_session, sentinel,
        "expected R_SESSION_TMPDIR to differ from preexisting value: {text}"
    );
    assert_ne!(
        tmpdir, sentinel,
        "expected TMPDIR to differ from preexisting value: {text}"
    );
    assert_eq!(
        tmpdir, mcp_tmpdir,
        "expected TMPDIR to match MCP_TMPDIR, got: {text}"
    );
    session.cancel().await?;
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_workspace_write_allows_network_access() -> TestResult<()> {
    if !sandbox_available() {
        eprintln!("sandbox-exec unavailable; skipping");
        return Ok(());
    }
    let Some(addr) = start_loopback_server_if_available().await? else {
        return Ok(());
    };
    let mut session = spawn_server_with_sandbox_state(sandbox_state_workspace_write(true)).await?;
    let result = session
        .write_stdin_raw_with(network_test_code(addr), Some(10.0))
        .await?;
    let text = collect_text(&result);
    if skip_backend_unavailable("sandbox_workspace_write_allows_network_access", &text) {
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("NETWORK_OK"),
        "expected network to succeed, got: {text}"
    );
    session.cancel().await?;
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_full_access_allows_network_access() -> TestResult<()> {
    if !sandbox_available() {
        eprintln!("sandbox-exec unavailable; skipping");
        return Ok(());
    }
    let Some(addr) = start_loopback_server_if_available().await? else {
        return Ok(());
    };
    let mut session = spawn_server_with_sandbox_state(sandbox_state_full_access()).await?;
    let result = session
        .write_stdin_raw_with(network_test_code(addr), Some(10.0))
        .await?;
    let text = collect_text(&result);
    if skip_backend_unavailable("sandbox_full_access_allows_network_access", &text) {
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("NETWORK_OK"),
        "expected network to succeed, got: {text}"
    );
    session.cancel().await?;
    Ok(())
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_allows_sysctl_used_by_quarto() -> TestResult<()> {
    if !sandbox_available() {
        eprintln!("sandbox-exec unavailable; skipping");
        return Ok(());
    }

    let mut session = spawn_server_with_sandbox_state(sandbox_state_workspace_write(false)).await?;
    let code = r#"
brand <- suppressWarnings(system("/usr/sbin/sysctl machdep.cpu.brand_string", intern = TRUE))
status_ngroups <- system("sysctl -n kern.ngroups >/dev/null")
status_oidfmt <- system("sysctl -b hw.ncpu >/dev/null")
cat("SYSCTL_BRAND_STRING=", paste(brand, collapse = " "), "\n", sep = "")
cat("SYSCTL_NGROUPS_STATUS=", status_ngroups, "\n", sep = "")
cat("SYSCTL_OIDFMT_STATUS=", status_oidfmt, "\n", sep = "")
"#;
    let result = session.write_stdin_raw_with(code, Some(10.0)).await?;
    let text = collect_text(&result);
    if skip_backend_unavailable("sandbox_allows_sysctl_used_by_quarto", &text) {
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("SYSCTL_BRAND_STRING=Intel") || text.contains("SYSCTL_BRAND_STRING=Apple"),
        "expected sysctl machdep.cpu.brand_string to contain Intel or Apple, got: {text}"
    );
    assert!(
        text.contains("SYSCTL_NGROUPS_STATUS=0"),
        "expected sysctl kern.ngroups to succeed, got: {text}"
    );
    assert!(
        text.contains("SYSCTL_OIDFMT_STATUS=0"),
        "expected sysctl -b hw.ncpu to succeed, got: {text}"
    );

    session.cancel().await?;
    Ok(())
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_allows_parallel_detect_cores() -> TestResult<()> {
    if !sandbox_available() {
        eprintln!("sandbox-exec unavailable; skipping");
        return Ok(());
    }

    let mut session = spawn_server_with_sandbox_state(sandbox_state_workspace_write(false)).await?;
    let code = r#"
suppressWarnings({
  logical <- parallel::detectCores(logical = TRUE)
  physical <- parallel::detectCores(logical = FALSE)
  cat("DETECT_CORES_LOGICAL=", logical, "\n", sep = "")
  cat("DETECT_CORES_PHYSICAL=", physical, "\n", sep = "")
  cat("DETECT_CORES_OK=", is.numeric(logical) && is.numeric(physical) && logical >= physical && physical >= 1, "\n", sep = "")
})
"#;
    let result = session.write_stdin_raw_with(code, Some(10.0)).await?;
    let text = collect_text(&result);
    if skip_backend_unavailable("sandbox_allows_parallel_detect_cores", &text) {
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("DETECT_CORES_OK=TRUE"),
        "expected parallel::detectCores() to succeed under sandbox, got: {text}"
    );

    session.cancel().await?;
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_denials_linux() -> TestResult<()> {
    let Some(home) = std::env::var_os("HOME") else {
        return Ok(());
    };
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let forbidden = Path::new(&home).join(format!("mcp-repl-denied-{nanos}.txt"));
    let forbidden = forbidden.to_string_lossy().to_string();

    let mut session = spawn_server_with_sandbox_state(sandbox_state_workspace_write(false)).await?;
    let code = format!(
        r#"
target <- {forbidden:?}
tryCatch({{
  writeLines("nope", target)
  cat("WRITE_OK\n")
}}, error = function(e) {{
  message("WRITE_ERROR:", conditionMessage(e))
}})
"#
    );
    let result = session.write_stdin_raw_with(&code, Some(10.0)).await?;
    let text = collect_text(&result);
    if skip_backend_unavailable("sandbox_denials_linux", &text) {
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("WRITE_ERROR:"),
        "expected HOME write to be blocked under workspace-write, got: {text}"
    );
    assert!(
        !text.contains("WRITE_OK"),
        "write unexpectedly succeeded under workspace-write: {text}"
    );
    session.cancel().await?;
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_denials_linux_bwrap() -> TestResult<()> {
    if !linux_bwrap_available() {
        eprintln!("bwrap unavailable; skipping");
        return Ok(());
    }
    let Some(home) = std::env::var_os("HOME") else {
        return Ok(());
    };
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let forbidden = Path::new(&home).join(format!("mcp-repl-bwrap-denied-{nanos}.txt"));
    let forbidden = forbidden.to_string_lossy().to_string();

    let mut session = spawn_server_with_sandbox_state_and_env(
        sandbox_state_workspace_write(false),
        vec![("MCP_REPL_USE_LINUX_BWRAP".to_string(), "1".to_string())],
    )
    .await?;
    let code = format!(
        r#"
target <- {forbidden:?}
tryCatch({{
  writeLines("nope", target)
  cat("WRITE_OK\n")
}}, error = function(e) {{
  message("WRITE_ERROR:", conditionMessage(e))
}})
"#
    );
    let result = session.write_stdin_raw_with(&code, Some(10.0)).await?;
    let text = collect_text(&result);
    if bwrap_worker_unavailable(&text) {
        eprintln!("bwrap unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("WRITE_ERROR:"),
        "expected HOME write to be blocked under workspace-write+bwrap, got: {text}"
    );
    assert!(
        !text.contains("WRITE_OK"),
        "write unexpectedly succeeded under workspace-write+bwrap: {text}"
    );
    session.cancel().await?;
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_bwrap_protects_dot_git_codex_agents() -> TestResult<()> {
    if !linux_bwrap_available() {
        eprintln!("bwrap unavailable; skipping");
        return Ok(());
    }
    let repo_root = std::env::current_dir()?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let writable_root = repo_root.join(format!("mcp-repl-bwrap-root-{nanos}"));
    std::fs::create_dir_all(writable_root.join(".git"))?;
    std::fs::create_dir_all(writable_root.join(".codex"))?;
    std::fs::create_dir_all(writable_root.join(".agents"))?;

    let mut session = spawn_server_with_sandbox_state_and_env(
        sandbox_state_workspace_write_with_roots(false, vec![writable_root.clone()]),
        vec![("MCP_REPL_USE_LINUX_BWRAP".to_string(), "1".to_string())],
    )
    .await?;
    let target_git = writable_root.join(".git/deny.txt");
    let target_codex = writable_root.join(".codex/deny.txt");
    let target_agents = writable_root.join(".agents/deny.txt");
    let code = format!(
        r#"
targets <- c({:?}, {:?}, {:?})
labels <- c("GIT", "CODEX", "AGENTS")
for (i in seq_along(targets)) {{
  target <- targets[[i]]
  label <- labels[[i]]
  tryCatch({{
    writeLines("nope", target)
    cat("WRITE_OK_", label, "\n", sep = "")
  }}, error = function(e) {{
    cat("WRITE_ERROR_", label, "=", conditionMessage(e), "\n", sep = "")
  }})
}}
"#,
        target_git.to_string_lossy().to_string(),
        target_codex.to_string_lossy().to_string(),
        target_agents.to_string_lossy().to_string()
    );
    let result = session.write_stdin_raw_with(&code, Some(10.0)).await?;
    let text = collect_text(&result);
    if bwrap_worker_unavailable(&text) {
        eprintln!("bwrap unavailable in this environment; skipping");
        session.cancel().await?;
        let _ = std::fs::remove_dir_all(&writable_root);
        return Ok(());
    }
    session.cancel().await?;
    let _ = std::fs::remove_dir_all(&writable_root);

    for label in ["GIT", "CODEX", "AGENTS"] {
        assert!(
            text.contains(&format!("WRITE_ERROR_{label}=")),
            "expected write error for {label}, got: {text}"
        );
        assert!(
            !text.contains(&format!("WRITE_OK_{label}")),
            "write unexpectedly succeeded for {label}: {text}"
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_workspace_write_blocks_network_access_bwrap() -> TestResult<()> {
    if !linux_bwrap_available() {
        eprintln!("bwrap unavailable; skipping");
        return Ok(());
    }
    let Some(addr) = start_loopback_server_if_available().await? else {
        return Ok(());
    };
    let mut session = spawn_server_with_sandbox_state_and_env(
        sandbox_state_workspace_write(false),
        vec![("MCP_REPL_USE_LINUX_BWRAP".to_string(), "1".to_string())],
    )
    .await?;
    let result = session
        .write_stdin_raw_with(network_test_code(addr), Some(10.0))
        .await?;
    let text = collect_text(&result);
    if bwrap_worker_unavailable(&text) {
        eprintln!("bwrap unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("NETWORK_ERROR:"),
        "expected network to be blocked under bwrap, got: {text}"
    );
    assert!(
        !text.contains("NETWORK_OK"),
        "network request unexpectedly succeeded under bwrap: {text}"
    );
    session.cancel().await?;
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_bwrap_no_proc_mode_starts_worker() -> TestResult<()> {
    if !linux_bwrap_available() {
        eprintln!("bwrap unavailable; skipping");
        return Ok(());
    }
    let mut session = spawn_server_with_sandbox_state_and_env(
        sandbox_state_workspace_write(false),
        vec![
            ("MCP_REPL_USE_LINUX_BWRAP".to_string(), "1".to_string()),
            ("MCP_REPL_LINUX_BWRAP_NO_PROC".to_string(), "1".to_string()),
        ],
    )
    .await?;
    let result = session
        .write_stdin_raw_with("cat('BWRAP_NOPROC_OK\\n')\n", Some(10.0))
        .await?;
    let text = collect_text(&result);
    if bwrap_worker_unavailable(&text) {
        eprintln!("bwrap unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("BWRAP_NOPROC_OK"),
        "expected worker output in bwrap no-proc mode, got: {text}"
    );
    session.cancel().await?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn windows_home_dir() -> Option<PathBuf> {
    if let Some(user_profile) = std::env::var_os("USERPROFILE") {
        return Some(PathBuf::from(user_profile));
    }
    let home_drive = std::env::var_os("HOMEDRIVE")?;
    let home_path = std::env::var_os("HOMEPATH")?;
    let mut home = PathBuf::from(home_drive);
    home.push(home_path);
    Some(home)
}

#[cfg(target_os = "windows")]
fn windows_sandbox_backend_unavailable(text: &str) -> bool {
    text.contains("CreateRestrictedToken failed: 87")
        || text.contains("worker exited before IPC named pipe connection")
        || text.contains("timed out waiting for IPC named pipe client connection")
}

#[cfg(target_os = "windows")]
fn powershell_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(target_os = "windows")]
fn cleanup_restricted_file(path: &std::path::Path) {
    let script = format!(
        "$path = {}; if (Test-Path -LiteralPath $path) {{ icacls $path /reset | Out-Null; Remove-Item -LiteralPath $path -Force -ErrorAction SilentlyContinue }}",
        powershell_literal(&path.to_string_lossy())
    );
    let _ = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-Command", &script])
        .status();
}

#[cfg(target_os = "windows")]
fn cleanup_restricted_path(path: &std::path::Path) {
    let script = format!(
        "$path = {}; if (Test-Path -LiteralPath $path) {{ icacls $path /reset /t /c | Out-Null; Remove-Item -LiteralPath $path -Force -Recurse -ErrorAction SilentlyContinue }}",
        powershell_literal(&path.to_string_lossy())
    );
    let _ = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-Command", &script])
        .status();
}

#[cfg(target_os = "windows")]
fn unresolved_windows_sid_acl_entries(path: &std::path::Path) -> TestResult<Vec<String>> {
    let script = format!(
        r#"
$path = {}
if (-not (Test-Path -LiteralPath $path)) {{
  Write-Error "missing path: $path"
  exit 1
}}
(Get-Acl -LiteralPath $path).Access |
  ForEach-Object {{ $_.IdentityReference.Value }} |
  Where-Object {{ $_ -match '^S-1-5-21-\d+-\d+-\d+-\d+$' }} |
  Sort-Object -Unique
"#,
        powershell_literal(&path.to_string_lossy())
    );
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-Command", &script])
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "failed to read ACL entries for {}: status={} stderr={}",
            path.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect())
}

#[cfg(target_os = "windows")]
fn scrub_unresolved_windows_sid_aces(path: &std::path::Path) -> TestResult<()> {
    let script = format!(
        r#"
$path = {}
if (-not (Test-Path -LiteralPath $path)) {{
  Write-Error "missing path: $path"
  exit 1
}}
$currentUser = [System.Security.Principal.WindowsIdentity]::GetCurrent().Name
icacls $path /inheritance:r /grant:r "${{currentUser}}:(OI)(CI)F" "SYSTEM:(OI)(CI)F" "Administrators:(OI)(CI)F" | Out-Null
"#,
        powershell_literal(&path.to_string_lossy())
    );
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-Command", &script])
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "failed to scrub ACL entries for {}: status={} stderr={}",
            path.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn windows_canonicalize_or_identity(path: &std::path::Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(target_os = "windows")]
fn windows_lexically_normalize_path(path: &std::path::Path) -> PathBuf {
    use std::ffi::OsString;
    use std::path::Component;

    let mut prefix: Option<OsString> = None;
    let mut has_root = false;
    let mut leading_parents: Vec<OsString> = Vec::new();
    let mut segments: Vec<OsString> = Vec::new();

    for component in path.components() {
        match component {
            Component::Prefix(value) => prefix = Some(value.as_os_str().to_os_string()),
            Component::RootDir => has_root = true,
            Component::CurDir => {}
            Component::ParentDir => {
                if !segments.is_empty() {
                    segments.pop();
                } else if !has_root {
                    leading_parents.push(component.as_os_str().to_os_string());
                }
            }
            Component::Normal(part) => segments.push(part.to_os_string()),
        }
    }

    let mut normalized = PathBuf::new();
    if let Some(prefix) = prefix {
        normalized.push(prefix);
    }
    if has_root {
        normalized.push(std::path::Path::new(r"\"));
    }
    for parent in leading_parents {
        normalized.push(parent);
    }
    for segment in segments {
        normalized.push(segment);
    }
    normalized
}

#[cfg(target_os = "windows")]
fn windows_canonicalize_or_normalize_stable_sid_path(path: &std::path::Path) -> PathBuf {
    let normalized = windows_lexically_normalize_path(path);
    if let Ok(canonical) = std::fs::canonicalize(&normalized) {
        return canonical;
    }

    for ancestor in normalized.ancestors().skip(1) {
        if let Ok(canonical_ancestor) = std::fs::canonicalize(ancestor)
            && let Ok(suffix) = normalized.strip_prefix(ancestor)
        {
            return canonical_ancestor.join(suffix);
        }
    }

    normalized
}

#[cfg(target_os = "windows")]
fn windows_stable_sid_seed_path_buf(path: PathBuf) -> String {
    let path = path.to_string_lossy();
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        let mut stable = format!(r"\\{rest}");
        stable.make_ascii_lowercase();
        return stable;
    }
    if let Some(rest) = path.strip_prefix(r"\\?\") {
        let mut stable = rest.to_string();
        stable.make_ascii_lowercase();
        return stable;
    }
    let mut stable = path.into_owned();
    stable.make_ascii_lowercase();
    stable
}

#[cfg(target_os = "windows")]
fn windows_stable_sid_seed_path(path: &std::path::Path) -> String {
    windows_stable_sid_seed_path_buf(windows_canonicalize_or_normalize_stable_sid_path(path))
}

#[cfg(target_os = "windows")]
fn windows_stable_sid_word(bytes: &[u8], seed: u32) -> u32 {
    let mut hash = 2_166_136_261u32 ^ seed;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(16_777_619);
    }
    hash.max(1)
}

#[cfg(target_os = "windows")]
fn windows_workspace_write_prepared_sid_for_cwd(
    cwd: &std::path::Path,
    writable_roots: &[PathBuf],
) -> TestResult<String> {
    let cwd = windows_canonicalize_or_identity(cwd);
    let stable_cwd = windows_stable_sid_seed_path_buf(cwd.clone());
    let mut canonical_roots = writable_roots
        .iter()
        .map(|root| windows_stable_sid_seed_path(&cwd.join(root)))
        .collect::<Vec<_>>();
    canonical_roots.sort();
    canonical_roots.dedup();
    let policy_seed = serde_json::json!({
        "mode": "workspace-write",
        "writable_roots": canonical_roots,
        "network_access": false,
        "exclude_tmpdir_env_var": false,
        "exclude_slash_tmp": false,
    });
    let seed = format!(
        "mcp-repl-windows-sandbox-v2\0{}\0{}",
        stable_cwd, policy_seed,
    );
    let a = windows_stable_sid_word(seed.as_bytes(), 0x243f_6a88);
    let b = windows_stable_sid_word(seed.as_bytes(), 0x85a3_08d3);
    let c = windows_stable_sid_word(seed.as_bytes(), 0x1319_8a2e);
    let d = windows_stable_sid_word(seed.as_bytes(), 0x0370_7344);
    Ok(format!("S-1-5-21-{a}-{b}-{c}-{d}"))
}

#[cfg(target_os = "windows")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_denials_windows() -> TestResult<()> {
    let Some(home) = windows_home_dir() else {
        eprintln!("USERPROFILE/HOMEDRIVE+HOMEPATH unavailable; skipping");
        return Ok(());
    };
    if !home.is_dir() {
        eprintln!("home directory is unavailable; skipping");
        return Ok(());
    }

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let forbidden = home.join(format!("mcp-repl-denied-{nanos}.txt"));
    let forbidden_r = r_string(&forbidden.to_string_lossy());

    let mut session = spawn_server_with_sandbox_state(sandbox_state_workspace_write(false)).await?;
    let code = format!(
        r#"
target <- {forbidden_r}
tryCatch({{
  writeLines("nope", target)
  cat("WRITE_OK\n")
}}, error = function(e) {{
  message("WRITE_ERROR:", conditionMessage(e))
}})
"#
    );
    let result = session.write_stdin_raw_with(&code, Some(10.0)).await?;
    let text = collect_text(&result);
    let _ = std::fs::remove_file(&forbidden);
    if windows_sandbox_backend_unavailable(&text) {
        eprintln!("windows restricted token setup unavailable; skipping");
        session.cancel().await?;
        return Ok(());
    }

    assert!(
        text.contains("WRITE_ERROR:"),
        "expected USERPROFILE write to be blocked under workspace-write, got: {text}"
    );
    assert!(
        !text.contains("WRITE_OK"),
        "write unexpectedly succeeded under workspace-write: {text}"
    );
    session.cancel().await?;
    Ok(())
}

#[cfg(target_os = "windows")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_workspace_write_restart_blocks_file_moved_outside_writable_root() -> TestResult<()>
{
    let Some(home) = windows_home_dir() else {
        eprintln!("USERPROFILE/HOMEDRIVE+HOMEPATH unavailable; skipping");
        return Ok(());
    };
    if !home.is_dir() {
        eprintln!("home directory is unavailable; skipping");
        return Ok(());
    }

    let workspace = temp_workspace_root()?;
    let repo_root = workspace.path().to_path_buf();
    let source = repo_root.join(format!(
        "mcp-repl-sandbox-outbound-source-{}.txt",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let target = home.join(format!(
        "mcp-repl-sandbox-outbound-target-{}.txt",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let source_r = r_string(&source.to_string_lossy());
    let target_r = r_string(&target.to_string_lossy());
    let mut session =
        spawn_server_with_sandbox_state_in_cwd(sandbox_state_workspace_write(false), &repo_root)
            .await?;

    let setup_code = format!(
        r#"
source <- {source_r}
writeLines("before move", source)
cat("WRITE_OK=", file.exists(source), "\n", sep = "")
cat("READ_BEFORE_MOVE=", paste(readLines(source, warn = FALSE), collapse = "|"), "\n", sep = "")
"#
    );
    let setup = session.write_stdin_raw_with(setup_code, Some(10.0)).await?;
    let setup_text = collect_text(&setup);
    if windows_sandbox_backend_unavailable(&setup_text) {
        eprintln!("windows restricted token setup unavailable; skipping");
        session.cancel().await?;
        cleanup_restricted_file(&source);
        cleanup_restricted_file(&target);
        return Ok(());
    }

    assert!(
        setup_text.contains("WRITE_OK=TRUE"),
        "expected sandboxed workspace write before move, got: {setup_text}"
    );
    assert!(
        setup_text.contains("READ_BEFORE_MOVE=before move"),
        "expected sandboxed workspace read before move, got: {setup_text}"
    );

    std::fs::rename(&source, &target)?;

    let restart = session.write_stdin_raw_with("\u{4}", Some(10.0)).await?;
    let restart_text = collect_text(&restart);
    if windows_sandbox_backend_unavailable(&restart_text) {
        eprintln!("windows restricted token setup unavailable; skipping");
        session.cancel().await?;
        cleanup_restricted_file(&source);
        cleanup_restricted_file(&target);
        return Ok(());
    }
    assert!(
        restart_text.contains("new session started"),
        "expected worker restart notice, got: {restart_text}"
    );

    let follow_up_code = format!(
        r#"
target <- {target_r}
tryCatch({{
  writeLines("after move", target)
  cat("WRITE_AFTER_OK\n")
}}, error = function(e) {{
  message("WRITE_AFTER_ERROR:", conditionMessage(e))
}})
"#
    );
    let follow_up = session
        .write_stdin_raw_with(follow_up_code, Some(10.0))
        .await?;
    let follow_up_text = collect_text(&follow_up);

    cleanup_restricted_file(&source);
    cleanup_restricted_file(&target);
    session.cancel().await?;

    assert!(
        follow_up_text.contains("WRITE_AFTER_ERROR:"),
        "expected moved external file to reject writes after restart, got: {follow_up_text}"
    );
    assert!(
        !follow_up_text.contains("WRITE_AFTER_OK"),
        "file moved outside writable roots stayed writable after restart, got: {follow_up_text}"
    );
    Ok(())
}

#[cfg(target_os = "windows")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_workspace_write_restart_blocks_moved_file_inside_git_dir() -> TestResult<()> {
    let workspace = temp_workspace_root_with_git_dir()?;
    let repo_root = workspace.path().to_path_buf();
    let git_dir = repo_root.join(".git");
    let source = repo_root.join(format!(
        "mcp-repl-sandbox-protected-source-{}.txt",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let target = git_dir.join(format!(
        "mcp-repl-sandbox-protected-target-{}.txt",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let source_r = r_string(&source.to_string_lossy());
    let target_r = r_string(&target.to_string_lossy());
    let mut session =
        spawn_server_with_sandbox_state_in_cwd(sandbox_state_workspace_write(false), &repo_root)
            .await?;

    let setup_code = format!(
        r#"
source <- {source_r}
writeLines("before restart", source)
cat("WRITE_OK=", file.exists(source), "\n", sep = "")
cat("READ_BEFORE_MOVE=", paste(readLines(source, warn = FALSE), collapse = "|"), "\n", sep = "")
"#
    );
    let setup = session.write_stdin_raw_with(setup_code, Some(10.0)).await?;
    let setup_text = collect_text(&setup);
    if windows_sandbox_backend_unavailable(&setup_text) {
        eprintln!("windows restricted token setup unavailable; skipping");
        session.cancel().await?;
        cleanup_restricted_file(&source);
        cleanup_restricted_file(&target);
        return Ok(());
    }

    assert!(
        setup_text.contains("WRITE_OK=TRUE"),
        "expected sandboxed workspace write to succeed before moving into .git, got: {setup_text}"
    );
    assert!(
        setup_text.contains("READ_BEFORE_MOVE=before restart"),
        "expected sandboxed workspace file to be readable before moving into .git, got: {setup_text}"
    );

    std::fs::rename(&source, &target)?;

    let restart = session.write_stdin_raw_with("\u{4}", Some(10.0)).await?;
    let restart_text = collect_text(&restart);
    if windows_sandbox_backend_unavailable(&restart_text) {
        eprintln!("windows restricted token setup unavailable; skipping");
        session.cancel().await?;
        cleanup_restricted_file(&source);
        cleanup_restricted_file(&target);
        return Ok(());
    }
    assert!(
        restart_text.contains("new session started"),
        "expected worker restart notice, got: {restart_text}"
    );

    let follow_up_code = format!(
        r#"
target <- {target_r}
tryCatch({{
  cat("READ_AFTER=", paste(readLines(target, warn = FALSE), collapse = "|"), "\n", sep = "")
}}, error = function(e) {{
  message("READ_AFTER_ERROR:", conditionMessage(e))
}})
tryCatch({{
  writeLines("after restart", target)
  cat("WRITE_AFTER_OK\n")
}}, error = function(e) {{
  message("WRITE_AFTER_ERROR:", conditionMessage(e))
}})
"#
    );
    let follow_up = session
        .write_stdin_raw_with(follow_up_code, Some(10.0))
        .await?;
    let follow_up_text = collect_text(&follow_up);

    cleanup_restricted_file(&source);
    cleanup_restricted_file(&target);
    session.cancel().await?;

    assert!(
        follow_up_text.contains("READ_AFTER=before restart"),
        "expected moved file contents to remain readable after restart, got: {follow_up_text}"
    );
    assert!(
        follow_up_text.contains("WRITE_AFTER_ERROR:"),
        "expected file moved into .git to reject writes after worker restart, got: {follow_up_text}"
    );
    assert!(
        !follow_up_text.contains("WRITE_AFTER_OK"),
        "file moved into .git stayed writable after worker restart, got: {follow_up_text}"
    );
    Ok(())
}

#[cfg(target_os = "windows")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_workspace_write_concurrent_sessions_block_file_moved_into_git_dir()
-> TestResult<()> {
    let workspace = temp_workspace_root_with_git_dir()?;
    let repo_root = workspace.path().to_path_buf();
    let git_dir = repo_root.join(".git");
    let hooks_dir = git_dir.join("hooks");
    let nested_dir = repo_root.join("src").join("pkg");
    let source = nested_dir.join(format!(
        "mcp-repl-sandbox-concurrent-protected-source-{}.txt",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let target = hooks_dir.join(format!(
        "mcp-repl-sandbox-concurrent-protected-target-{}.txt",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let source_r = r_string(&source.to_string_lossy());
    let target_r = r_string(&target.to_string_lossy());
    std::fs::create_dir_all(&hooks_dir)?;
    std::fs::create_dir_all(&nested_dir)?;
    scrub_unresolved_windows_sid_aces(&repo_root)?;
    let expected_stable_sid = windows_workspace_write_prepared_sid_for_cwd(&repo_root, &[])?;
    let mut session_a =
        spawn_server_with_sandbox_state_in_cwd(sandbox_state_workspace_write(false), &repo_root)
            .await?;

    let ready_a = session_a
        .write_stdin_raw_with("cat('SESSION_A_READY\\n')\n", Some(10.0))
        .await?;
    let ready_a_text = collect_text(&ready_a);
    if windows_sandbox_backend_unavailable(&ready_a_text) {
        eprintln!("windows restricted token setup unavailable; skipping");
        session_a.cancel().await?;
        cleanup_restricted_file(&source);
        cleanup_restricted_file(&target);
        cleanup_restricted_path(&hooks_dir);
        cleanup_restricted_path(&nested_dir);
        return Ok(());
    }

    let create_code = format!(
        r#"
source <- {source_r}
writeLines("before protected move", source)
cat("SOURCE_WRITE_OK=", file.exists(source), "\n", sep = "")
cat("SOURCE_READ=", paste(readLines(source, warn = FALSE), collapse = "|"), "\n", sep = "")
"#
    );
    let create = session_a
        .write_stdin_raw_with(create_code, Some(10.0))
        .await?;
    let create_text = collect_text(&create);
    assert!(
        create_text.contains("SOURCE_WRITE_OK=TRUE"),
        "expected first live session to create the workspace artifact before the protected move, got: {create_text}"
    );
    assert!(
        create_text.contains("SOURCE_READ=before protected move"),
        "expected first live session to read the workspace artifact before the protected move, got: {create_text}"
    );

    std::fs::rename(&source, &target)?;
    let target_acl_before = unresolved_windows_sid_acl_entries(&target)?;

    let mut session_b =
        spawn_server_with_sandbox_state_in_cwd(sandbox_state_workspace_write(false), &repo_root)
            .await?;
    let ready_b = session_b
        .write_stdin_raw_with("cat('SESSION_B_READY\\n')\n", Some(10.0))
        .await?;
    let ready_b_text = collect_text(&ready_b);
    if windows_sandbox_backend_unavailable(&ready_b_text) {
        eprintln!("windows restricted token setup unavailable; skipping");
        session_a.cancel().await?;
        session_b.cancel().await?;
        cleanup_restricted_file(&source);
        cleanup_restricted_file(&target);
        cleanup_restricted_path(&hooks_dir);
        cleanup_restricted_path(&nested_dir);
        return Ok(());
    }

    let use_code = format!(
        r#"
target <- {target_r}
tryCatch({{
  cat("SESSION_A_READ=", paste(readLines(target, warn = FALSE), collapse = "|"), "\n", sep = "")
}}, error = function(e) {{
  message("SESSION_A_READ_ERROR:", conditionMessage(e))
}})
tryCatch({{
  writeLines("after protected move", target)
  cat("SESSION_A_WRITE_OK\n")
}}, error = function(e) {{
  message("SESSION_A_WRITE_ERROR:", conditionMessage(e))
}})
"#
    );
    let use_result = session_a.write_stdin_raw_with(use_code, Some(10.0)).await?;
    let use_text = collect_text(&use_result);

    cleanup_restricted_file(&source);
    cleanup_restricted_file(&target);
    cleanup_restricted_path(&hooks_dir);
    cleanup_restricted_path(&nested_dir);
    session_a.cancel().await?;
    session_b.cancel().await?;

    assert!(
        target_acl_before.contains(&expected_stable_sid),
        "expected the moved workspace artifact to keep the stable prepared SID {expected_stable_sid} before the same-checkout refresh, got: {target_acl_before:?}"
    );
    assert!(
        use_text.contains("SESSION_A_READ=before protected move"),
        "expected the older live session to keep read access after the move into .git\\hooks, got: {use_text}"
    );
    assert!(
        use_text.contains("SESSION_A_WRITE_ERROR:"),
        "expected the older live session to lose write access after the same-checkout refresh moved the file under .git\\hooks, got: {use_text}"
    );
    assert!(
        !use_text.contains("SESSION_A_WRITE_OK"),
        "file moved into .git\\hooks stayed writable from the older live session, got: {use_text}"
    );
    Ok(())
}

#[cfg(target_os = "windows")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_workspace_write_restart_unblocks_file_moved_out_of_git_dir() -> TestResult<()> {
    let workspace = temp_workspace_root_with_git_dir()?;
    let repo_root = workspace.path().to_path_buf();
    let git_dir = repo_root.join(".git");

    let protected = git_dir.join(format!(
        "mcp-repl-sandbox-unblock-protected-{}.txt",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let restored = repo_root.join(format!(
        "mcp-repl-sandbox-unblock-restored-{}.txt",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let protected_r = r_string(&protected.to_string_lossy());
    let restored_r = r_string(&restored.to_string_lossy());
    let mut session =
        spawn_server_with_sandbox_state_in_cwd(sandbox_state_workspace_write(false), &repo_root)
            .await?;

    let setup_code = format!(
        r#"
source <- {restored_r}
writeLines("before protect", source)
cat("WRITE_OK=", file.exists(source), "\n", sep = "")
"#
    );
    let setup = session.write_stdin_raw_with(setup_code, Some(10.0)).await?;
    let setup_text = collect_text(&setup);
    if windows_sandbox_backend_unavailable(&setup_text) {
        eprintln!("windows restricted token setup unavailable; skipping");
        session.cancel().await?;
        cleanup_restricted_file(&protected);
        cleanup_restricted_file(&restored);
        return Ok(());
    }

    assert!(
        setup_text.contains("WRITE_OK=TRUE"),
        "expected sandboxed workspace write before protected move, got: {setup_text}"
    );

    std::fs::rename(&restored, &protected)?;

    let first_restart = session.write_stdin_raw_with("\u{4}", Some(10.0)).await?;
    let first_restart_text = collect_text(&first_restart);
    if windows_sandbox_backend_unavailable(&first_restart_text) {
        eprintln!("windows restricted token setup unavailable; skipping");
        session.cancel().await?;
        cleanup_restricted_file(&protected);
        cleanup_restricted_file(&restored);
        return Ok(());
    }
    assert!(
        first_restart_text.contains("new session started"),
        "expected first worker restart notice, got: {first_restart_text}"
    );

    let blocked_check = format!(
        r#"
target <- {protected_r}
tryCatch({{
  writeLines("still protected", target)
  cat("PROTECTED_WRITE_OK\n")
}}, error = function(e) {{
  message("PROTECTED_WRITE_ERROR:", conditionMessage(e))
}})
"#
    );
    let blocked = session
        .write_stdin_raw_with(blocked_check, Some(10.0))
        .await?;
    let blocked_text = collect_text(&blocked);
    assert!(
        blocked_text.contains("PROTECTED_WRITE_ERROR:"),
        "expected file under .git to reject writes after refresh, got: {blocked_text}"
    );

    std::fs::rename(&protected, &restored)?;

    let second_restart = session.write_stdin_raw_with("\u{4}", Some(10.0)).await?;
    let second_restart_text = collect_text(&second_restart);
    assert!(
        second_restart_text.contains("new session started"),
        "expected second worker restart notice, got: {second_restart_text}"
    );

    let restored_check = format!(
        r#"
target <- {restored_r}
tryCatch({{
  writeLines("after restore", target)
  cat("RESTORED_WRITE_OK\n")
}}, error = function(e) {{
  message("RESTORED_WRITE_ERROR:", conditionMessage(e))
}})
"#
    );
    let restored_result = session
        .write_stdin_raw_with(restored_check, Some(10.0))
        .await?;
    let restored_text = collect_text(&restored_result);

    cleanup_restricted_file(&protected);
    cleanup_restricted_file(&restored);
    session.cancel().await?;

    assert!(
        restored_text.contains("RESTORED_WRITE_OK"),
        "expected file moved back out of .git to regain write access after restart, got: {restored_text}"
    );
    assert!(
        !restored_text.contains("RESTORED_WRITE_ERROR:"),
        "file moved back out of .git stayed blocked after restart, got: {restored_text}"
    );
    Ok(())
}

#[cfg(target_os = "windows")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_workspace_write_restart_allows_host_created_file_under_workspace_subdir()
-> TestResult<()> {
    let workspace = temp_workspace_root()?;
    let repo_root = workspace.path().to_path_buf();
    let nested_dir = repo_root.join("src");
    let host_created = nested_dir.join(format!(
        "mcp-repl-sandbox-host-created-{}.txt",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let host_created_r = r_string(&host_created.to_string_lossy());
    std::fs::create_dir_all(&nested_dir)?;

    let mut session =
        spawn_server_with_sandbox_state_in_cwd(sandbox_state_workspace_write(false), &repo_root)
            .await?;

    let ready = session
        .write_stdin_raw_with("cat('SESSION_READY\\n')\n", Some(10.0))
        .await?;
    let ready_text = collect_text(&ready);
    if windows_sandbox_backend_unavailable(&ready_text) {
        eprintln!("windows restricted token setup unavailable; skipping");
        session.cancel().await?;
        cleanup_restricted_file(&host_created);
        return Ok(());
    }
    session.cancel().await?;

    std::fs::write(&host_created, b"host before restart")?;

    let mut restarted =
        spawn_server_with_sandbox_state_in_cwd(sandbox_state_workspace_write(false), &repo_root)
            .await?;
    let follow_up_code = format!(
        r#"
target <- {host_created_r}
tryCatch({{
  cat("READ_AFTER=", paste(readLines(target, warn = FALSE), collapse = "|"), "\n", sep = "")
  writeLines(c(readLines(target, warn = FALSE), "sandbox append"), target)
  cat("WRITE_AFTER_OK\n")
  cat("WRITE_AFTER=", paste(readLines(target, warn = FALSE), collapse = "|"), "\n", sep = "")
}}, error = function(e) {{
  message("WRITE_AFTER_ERROR:", conditionMessage(e))
}})
"#
    );
    let follow_up = restarted
        .write_stdin_raw_with(follow_up_code, Some(10.0))
        .await?;
    let follow_up_text = collect_text(&follow_up);

    cleanup_restricted_file(&host_created);
    restarted.cancel().await?;

    assert!(
        follow_up_text.contains("READ_AFTER=host before restart"),
        "expected restarted worker to read the host-created nested workspace file, got: {follow_up_text}"
    );
    assert!(
        follow_up_text.contains("WRITE_AFTER_OK"),
        "expected restarted worker to write the host-created nested workspace file, got: {follow_up_text}"
    );
    assert!(
        follow_up_text.contains("WRITE_AFTER=host before restart|sandbox append"),
        "expected restarted worker to read back its write in the nested workspace file, got: {follow_up_text}"
    );
    assert!(
        !follow_up_text.contains("WRITE_AFTER_ERROR:"),
        "host-created files under direct workspace subdirectories should stay writable after restart, got: {follow_up_text}"
    );
    Ok(())
}

#[cfg(target_os = "windows")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_workspace_write_nested_midrun_file_keeps_prepared_sid() -> TestResult<()> {
    let workspace = temp_workspace_root()?;
    let repo_root = workspace.path().to_path_buf();
    let nested_dir = repo_root.join("src");
    let artifact = nested_dir.join(format!(
        "mcp-repl-sandbox-nested-midrun-acl-{}.txt",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let artifact_r = r_string(&artifact.to_string_lossy());
    std::fs::create_dir_all(&nested_dir)?;
    scrub_unresolved_windows_sid_aces(&repo_root)?;
    let expected_stable_sid = windows_workspace_write_prepared_sid_for_cwd(&repo_root, &[])?;
    let mut session =
        spawn_server_with_sandbox_state_in_cwd(sandbox_state_workspace_write(false), &repo_root)
            .await?;

    let create_code = format!(
        r#"
target <- {artifact_r}
writeLines("nested midrun", target)
cat("WRITE_OK=", file.exists(target), "\n", sep = "")
"#
    );
    let create = session
        .write_stdin_raw_with(create_code, Some(10.0))
        .await?;
    let create_text = collect_text(&create);
    if windows_sandbox_backend_unavailable(&create_text) {
        eprintln!("windows restricted token setup unavailable; skipping");
        session.cancel().await?;
        cleanup_restricted_file(&artifact);
        return Ok(());
    }
    assert!(
        create_text.contains("WRITE_OK=TRUE"),
        "expected sandboxed worker to create the nested mid-run artifact, got: {create_text}"
    );

    let before_cancel = unresolved_windows_sid_acl_entries(&artifact)?;
    cleanup_restricted_file(&artifact);
    session.cancel().await?;

    assert!(
        before_cancel.contains(&expected_stable_sid),
        "expected a nested file created under the workspace root to retain the stable prepared SID {expected_stable_sid}, got: {before_cancel:?}"
    );
    Ok(())
}

#[cfg(target_os = "windows")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_workspace_write_concurrent_sessions_share_new_workspace_file() -> TestResult<()> {
    let writable_root = tempfile::tempdir()?;
    let workspace = temp_workspace_root()?;
    let cwd = workspace.path().join("workspace");
    let shared_dir = writable_root.path().join(format!(
        "mcp-repl-sandbox-concurrent-dir-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let shared_dir_r = r_string(&shared_dir.to_string_lossy());
    let shared = shared_dir.join("from-session-b.txt");
    let shared_r = r_string(&shared.to_string_lossy());
    scrub_unresolved_windows_sid_aces(writable_root.path())?;
    std::fs::create_dir_all(&cwd)?;
    let state =
        sandbox_state_workspace_write_with_roots(false, vec![writable_root.path().to_path_buf()]);
    let mut session_a = spawn_server_with_sandbox_state_in_cwd(state.clone(), &cwd).await?;
    let mut session_b = spawn_server_with_sandbox_state_in_cwd(state, &cwd).await?;

    let ready_a = session_a
        .write_stdin_raw_with("cat('SESSION_A_READY\\n')\n", Some(10.0))
        .await?;
    let ready_a_text = collect_text(&ready_a);
    if windows_sandbox_backend_unavailable(&ready_a_text) {
        eprintln!("windows restricted token setup unavailable; skipping");
        session_a.cancel().await?;
        session_b.cancel().await?;
        cleanup_restricted_file(&shared);
        return Ok(());
    }
    let ready_b = session_b
        .write_stdin_raw_with("cat('SESSION_B_READY\\n')\n", Some(10.0))
        .await?;
    let ready_b_text = collect_text(&ready_b);
    if windows_sandbox_backend_unavailable(&ready_b_text) {
        eprintln!("windows restricted token setup unavailable; skipping");
        session_a.cancel().await?;
        session_b.cancel().await?;
        cleanup_restricted_file(&shared);
        return Ok(());
    }

    let create_code = format!(
        r#"
target_dir <- {shared_dir_r}
dir.create(target_dir, recursive = TRUE, showWarnings = FALSE)
cat("SESSION_A_DIR_OK=", dir.exists(target_dir), "\n", sep = "")
"#
    );
    let create = session_a
        .write_stdin_raw_with(create_code, Some(10.0))
        .await?;
    let create_text = collect_text(&create);
    assert!(
        create_text.contains("SESSION_A_DIR_OK=TRUE"),
        "expected first live session to create the shared workspace directory, got: {create_text}"
    );

    let use_code = format!(
        r#"
target <- {shared_r}
tryCatch({{
  writeLines("from session b", target)
  cat("SESSION_B_WRITE_OK\n")
  cat("READ_BACK=", paste(readLines(target, warn = FALSE), collapse = "|"), "\n", sep = "")
}}, error = function(e) {{
  message("SESSION_B_WRITE_ERROR:", conditionMessage(e))
}})
"#
    );
    let use_result = session_b.write_stdin_raw_with(use_code, Some(10.0)).await?;
    let use_text = collect_text(&use_result);

    cleanup_restricted_file(&shared);
    let _ = std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "$path = {}; if (Test-Path -LiteralPath $path) {{ icacls $path /reset /t /c | Out-Null; Remove-Item -LiteralPath $path -Force -Recurse -ErrorAction SilentlyContinue }}",
                powershell_literal(&shared_dir.to_string_lossy())
            ),
        ])
        .status();
    session_a.cancel().await?;
    session_b.cancel().await?;

    assert!(
        use_text.contains("SESSION_B_WRITE_OK"),
        "expected older live session to write inside a directory created after it launched, got: {use_text}"
    );
    assert!(
        use_text.contains("READ_BACK=from session b"),
        "expected second live session to read back its write inside the new directory, got: {use_text}"
    );
    assert!(
        !use_text.contains("SESSION_B_WRITE_ERROR:"),
        "directories created in one live session should be writable from another same-checkout session, got: {use_text}"
    );
    Ok(())
}

#[cfg(target_os = "windows")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_workspace_write_concurrent_sessions_share_temp_renamed_workspace_file()
-> TestResult<()> {
    let writable_root = tempfile::tempdir()?;
    let workspace = temp_workspace_root()?;
    let cwd = workspace.path().join("workspace");
    let shared = writable_root.path().join(format!(
        "mcp-repl-sandbox-temp-rename-{}.txt",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let shared_r = r_string(&shared.to_string_lossy());
    scrub_unresolved_windows_sid_aces(writable_root.path())?;
    std::fs::create_dir_all(&cwd)?;
    let state =
        sandbox_state_workspace_write_with_roots(false, vec![writable_root.path().to_path_buf()]);
    let mut session_a = spawn_server_with_sandbox_state_in_cwd(state.clone(), &cwd).await?;
    let mut session_b = spawn_server_with_sandbox_state_in_cwd(state, &cwd).await?;

    let ready_a = session_a
        .write_stdin_raw_with("cat('SESSION_A_READY\\n')\n", Some(10.0))
        .await?;
    let ready_a_text = collect_text(&ready_a);
    if windows_sandbox_backend_unavailable(&ready_a_text) {
        eprintln!("windows restricted token setup unavailable; skipping");
        session_a.cancel().await?;
        session_b.cancel().await?;
        cleanup_restricted_file(&shared);
        return Ok(());
    }
    let ready_b = session_b
        .write_stdin_raw_with("cat('SESSION_B_READY\\n')\n", Some(10.0))
        .await?;
    let ready_b_text = collect_text(&ready_b);
    if windows_sandbox_backend_unavailable(&ready_b_text) {
        eprintln!("windows restricted token setup unavailable; skipping");
        session_a.cancel().await?;
        session_b.cancel().await?;
        cleanup_restricted_file(&shared);
        return Ok(());
    }

    let create_code = format!(
        r#"
target <- {shared_r}
temp_target <- tempfile(pattern = "mcp-repl-temp-rename-", tmpdir = tempdir(), fileext = ".txt")
writeLines("from session b temp", temp_target)
renamed <- file.rename(temp_target, target)
cat("SESSION_B_RENAMED=", renamed, "\n", sep = "")
cat("SESSION_B_TARGET_EXISTS=", file.exists(target), "\n", sep = "")
"#
    );
    let create = session_b
        .write_stdin_raw_with(create_code, Some(10.0))
        .await?;
    let create_text = collect_text(&create);
    assert!(
        create_text.contains("SESSION_B_RENAMED=TRUE"),
        "expected second live session to rename its temp artifact into the workspace, got: {create_text}"
    );
    assert!(
        create_text.contains("SESSION_B_TARGET_EXISTS=TRUE"),
        "expected renamed temp artifact to exist in the workspace, got: {create_text}"
    );

    let use_code = format!(
        r#"
target <- {shared_r}
tryCatch({{
  cat("SESSION_A_READ=", paste(readLines(target, warn = FALSE), collapse = "|"), "\n", sep = "")
  writeLines(c(readLines(target, warn = FALSE), "from session a"), target)
  cat("SESSION_A_WRITE_OK\n")
  cat("SESSION_A_AFTER=", paste(readLines(target, warn = FALSE), collapse = "|"), "\n", sep = "")
}}, error = function(e) {{
  message("SESSION_A_ACCESS_ERROR:", conditionMessage(e))
}})
"#
    );
    let use_result = session_a.write_stdin_raw_with(use_code, Some(10.0)).await?;
    let use_text = collect_text(&use_result);

    cleanup_restricted_file(&shared);
    session_a.cancel().await?;
    session_b.cancel().await?;

    assert!(
        use_text.contains("SESSION_A_READ=from session b temp"),
        "expected older live session to read the temp-renamed workspace artifact, got: {use_text}"
    );
    assert!(
        use_text.contains("SESSION_A_WRITE_OK"),
        "expected older live session to write the temp-renamed workspace artifact, got: {use_text}"
    );
    assert!(
        use_text.contains("SESSION_A_AFTER=from session b temp|from session a"),
        "expected older live session to read back its write after the temp rename, got: {use_text}"
    );
    assert!(
        !use_text.contains("SESSION_A_ACCESS_ERROR:"),
        "temp-renamed workspace artifacts should stay shared across live same-checkout sessions, got: {use_text}"
    );
    Ok(())
}

#[cfg(target_os = "windows")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_workspace_write_concurrent_sessions_share_direct_workspace_file() -> TestResult<()>
{
    let writable_root = tempfile::tempdir()?;
    let workspace = temp_workspace_root()?;
    let cwd = workspace.path().join("workspace");
    let shared = writable_root.path().join(format!(
        "mcp-repl-sandbox-direct-write-{}.txt",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let shared_r = r_string(&shared.to_string_lossy());
    scrub_unresolved_windows_sid_aces(writable_root.path())?;
    std::fs::create_dir_all(&cwd)?;
    let state =
        sandbox_state_workspace_write_with_roots(false, vec![writable_root.path().to_path_buf()]);
    let mut session_a = spawn_server_with_sandbox_state_in_cwd(state.clone(), &cwd).await?;
    let mut session_b = spawn_server_with_sandbox_state_in_cwd(state, &cwd).await?;

    let ready_a = session_a
        .write_stdin_raw_with("cat('SESSION_A_READY\\n')\n", Some(10.0))
        .await?;
    let ready_a_text = collect_text(&ready_a);
    if windows_sandbox_backend_unavailable(&ready_a_text) {
        eprintln!("windows restricted token setup unavailable; skipping");
        session_a.cancel().await?;
        session_b.cancel().await?;
        cleanup_restricted_file(&shared);
        return Ok(());
    }
    let ready_b = session_b
        .write_stdin_raw_with("cat('SESSION_B_READY\\n')\n", Some(10.0))
        .await?;
    let ready_b_text = collect_text(&ready_b);
    if windows_sandbox_backend_unavailable(&ready_b_text) {
        eprintln!("windows restricted token setup unavailable; skipping");
        session_a.cancel().await?;
        session_b.cancel().await?;
        cleanup_restricted_file(&shared);
        return Ok(());
    }

    let create_code = format!(
        r#"
target <- {shared_r}
writeLines("from session b direct", target)
cat("SESSION_B_WRITE_OK=", file.exists(target), "\n", sep = "")
"#
    );
    let create = session_b
        .write_stdin_raw_with(create_code, Some(10.0))
        .await?;
    let create_text = collect_text(&create);
    assert!(
        create_text.contains("SESSION_B_WRITE_OK=TRUE"),
        "expected second live session to create the direct workspace artifact, got: {create_text}"
    );

    let use_code = format!(
        r#"
target <- {shared_r}
tryCatch({{
  cat("SESSION_A_READ=", paste(readLines(target, warn = FALSE), collapse = "|"), "\n", sep = "")
  writeLines(c(readLines(target, warn = FALSE), "from session a"), target)
  cat("SESSION_A_WRITE_OK\n")
  cat("SESSION_A_AFTER=", paste(readLines(target, warn = FALSE), collapse = "|"), "\n", sep = "")
}}, error = function(e) {{
  message("SESSION_A_ACCESS_ERROR:", conditionMessage(e))
}})
"#
    );
    let use_result = session_a.write_stdin_raw_with(use_code, Some(10.0)).await?;
    let use_text = collect_text(&use_result);

    cleanup_restricted_file(&shared);
    session_a.cancel().await?;
    session_b.cancel().await?;

    assert!(
        use_text.contains("SESSION_A_READ=from session b direct"),
        "expected older live session to read the direct workspace artifact, got: {use_text}"
    );
    assert!(
        use_text.contains("SESSION_A_WRITE_OK"),
        "expected older live session to write the direct workspace artifact, got: {use_text}"
    );
    assert!(
        use_text.contains("SESSION_A_AFTER=from session b direct|from session a"),
        "expected older live session to read back its write after the direct create, got: {use_text}"
    );
    assert!(
        !use_text.contains("SESSION_A_ACCESS_ERROR:"),
        "directly created workspace artifacts should stay shared across live same-checkout sessions, got: {use_text}"
    );
    Ok(())
}

#[cfg(target_os = "windows")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_workspace_write_concurrent_sessions_share_host_created_nested_workspace_file()
-> TestResult<()> {
    let workspace = temp_workspace_root()?;
    let repo_root = workspace.path().to_path_buf();
    let nested_dir = repo_root.join("src").join("pkg");
    let shared = nested_dir.join(format!(
        "mcp-repl-sandbox-host-nested-{}.txt",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let shared_r = r_string(&shared.to_string_lossy());
    scrub_unresolved_windows_sid_aces(&repo_root)?;
    let expected_stable_sid = windows_workspace_write_prepared_sid_for_cwd(&repo_root, &[])?;
    let mut session_a =
        spawn_server_with_sandbox_state_in_cwd(sandbox_state_workspace_write(false), &repo_root)
            .await?;

    let ready_a = session_a
        .write_stdin_raw_with("cat('SESSION_A_READY\\n')\n", Some(10.0))
        .await?;
    let ready_a_text = collect_text(&ready_a);
    if windows_sandbox_backend_unavailable(&ready_a_text) {
        eprintln!("windows restricted token setup unavailable; skipping");
        session_a.cancel().await?;
        cleanup_restricted_file(&shared);
        cleanup_restricted_path(&nested_dir);
        return Ok(());
    }

    std::fs::create_dir_all(&nested_dir)?;
    std::fs::write(&shared, b"from host nested")?;

    let mut session_b =
        spawn_server_with_sandbox_state_in_cwd(sandbox_state_workspace_write(false), &repo_root)
            .await?;
    let ready_b = session_b
        .write_stdin_raw_with("cat('SESSION_B_READY\\n')\n", Some(10.0))
        .await?;
    let ready_b_text = collect_text(&ready_b);
    if windows_sandbox_backend_unavailable(&ready_b_text) {
        eprintln!("windows restricted token setup unavailable; skipping");
        session_a.cancel().await?;
        session_b.cancel().await?;
        cleanup_restricted_file(&shared);
        cleanup_restricted_path(&nested_dir);
        return Ok(());
    }

    let shared_acl = unresolved_windows_sid_acl_entries(&shared)?;
    let use_code = format!(
        r#"
target <- {shared_r}
tryCatch({{
  cat("SESSION_A_READ=", paste(readLines(target, warn = FALSE), collapse = "|"), "\n", sep = "")
  writeLines(c(readLines(target, warn = FALSE), "from session a"), target)
  cat("SESSION_A_WRITE_OK\n")
  cat("SESSION_A_AFTER=", paste(readLines(target, warn = FALSE), collapse = "|"), "\n", sep = "")
}}, error = function(e) {{
  message("SESSION_A_ACCESS_ERROR:", conditionMessage(e))
}})
"#
    );
    let use_result = session_a.write_stdin_raw_with(use_code, Some(10.0)).await?;
    let use_text = collect_text(&use_result);

    cleanup_restricted_file(&shared);
    cleanup_restricted_path(&nested_dir);
    session_a.cancel().await?;
    session_b.cancel().await?;

    assert!(
        shared_acl.contains(&expected_stable_sid),
        "expected host-created nested workspace artifact to gain the stable prepared SID {expected_stable_sid} after a same-checkout launch refresh, got: {shared_acl:?}"
    );
    assert!(
        use_text.contains("SESSION_A_READ=from host nested"),
        "expected older live session to read the host-created nested workspace artifact, got: {use_text}"
    );
    assert!(
        use_text.contains("SESSION_A_WRITE_OK"),
        "expected older live session to write the host-created nested workspace artifact, got: {use_text}"
    );
    assert!(
        use_text.contains("SESSION_A_AFTER=from host nested|from session a"),
        "expected older live session to read back its write on the host-created nested workspace artifact, got: {use_text}"
    );
    assert!(
        !use_text.contains("SESSION_A_ACCESS_ERROR:"),
        "host-created nested workspace artifacts should stay shared across live same-checkout sessions, got: {use_text}"
    );
    Ok(())
}

#[cfg(target_os = "windows")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_workspace_write_concurrent_sessions_share_host_renamed_nested_workspace_tree_with_extra_writable_root()
-> TestResult<()> {
    let writable_root = tempfile::tempdir()?;
    let workspace = temp_workspace_root()?;
    let repo_root = workspace.path().to_path_buf();
    let src_dir = repo_root.join("src");
    let nested_dir = src_dir.join("pkg");
    let shared = nested_dir.join(format!(
        "mcp-repl-sandbox-host-renamed-nested-extra-root-{}.txt",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let temp_root = tempfile::tempdir()?;
    let host_pkg = temp_root.path().join("pkg");
    let host_file = host_pkg.join(shared.file_name().expect("shared file name"));
    let shared_r = r_string(&shared.to_string_lossy());
    std::fs::create_dir_all(&src_dir)?;
    scrub_unresolved_windows_sid_aces(&repo_root)?;
    scrub_unresolved_windows_sid_aces(writable_root.path())?;
    let state =
        sandbox_state_workspace_write_with_roots(false, vec![writable_root.path().to_path_buf()]);
    let expected_stable_sid = windows_workspace_write_prepared_sid_for_cwd(
        &repo_root,
        &[writable_root.path().to_path_buf()],
    )?;
    let mut session_a = spawn_server_with_sandbox_state_in_cwd(state.clone(), &repo_root).await?;

    let ready_a = session_a
        .write_stdin_raw_with("cat('SESSION_A_READY\\n')\n", Some(10.0))
        .await?;
    let ready_a_text = collect_text(&ready_a);
    if windows_sandbox_backend_unavailable(&ready_a_text) {
        eprintln!("windows restricted token setup unavailable; skipping");
        session_a.cancel().await?;
        cleanup_restricted_file(&shared);
        cleanup_restricted_path(&nested_dir);
        cleanup_restricted_file(&host_file);
        cleanup_restricted_path(&src_dir);
        return Ok(());
    }

    std::fs::create_dir_all(&host_pkg)?;
    std::fs::write(&host_file, b"from moved host tree with extra root")?;
    std::fs::rename(&host_pkg, &nested_dir)?;

    let mut session_b = spawn_server_with_sandbox_state_in_cwd(state, &repo_root).await?;
    let ready_b = session_b
        .write_stdin_raw_with("cat('SESSION_B_READY\\n')\n", Some(10.0))
        .await?;
    let ready_b_text = collect_text(&ready_b);
    if windows_sandbox_backend_unavailable(&ready_b_text) {
        eprintln!("windows restricted token setup unavailable; skipping");
        session_a.cancel().await?;
        session_b.cancel().await?;
        cleanup_restricted_file(&shared);
        cleanup_restricted_path(&nested_dir);
        cleanup_restricted_file(&host_file);
        cleanup_restricted_path(&src_dir);
        return Ok(());
    }

    let nested_dir_acl = unresolved_windows_sid_acl_entries(&nested_dir)?;
    let shared_acl = unresolved_windows_sid_acl_entries(&shared)?;
    let use_code = format!(
        r#"
target <- {shared_r}
tryCatch({{
  cat("SESSION_A_READ=", paste(readLines(target, warn = FALSE), collapse = "|"), "\n", sep = "")
  writeLines(c(readLines(target, warn = FALSE), "from session a"), target)
  cat("SESSION_A_WRITE_OK\n")
  cat("SESSION_A_AFTER=", paste(readLines(target, warn = FALSE), collapse = "|"), "\n", sep = "")
}}, error = function(e) {{
  message("SESSION_A_ACCESS_ERROR:", conditionMessage(e))
}})
"#
    );
    let use_result = session_a.write_stdin_raw_with(use_code, Some(10.0)).await?;
    let use_text = collect_text(&use_result);

    cleanup_restricted_file(&shared);
    cleanup_restricted_path(&nested_dir);
    cleanup_restricted_file(&host_file);
    cleanup_restricted_path(&src_dir);
    session_a.cancel().await?;
    session_b.cancel().await?;

    assert!(
        nested_dir_acl.contains(&expected_stable_sid),
        "expected the renamed nested workspace directory to gain the stable prepared SID {expected_stable_sid} even with extra writable roots configured, got: {nested_dir_acl:?}"
    );
    assert!(
        shared_acl.contains(&expected_stable_sid),
        "expected the renamed nested workspace artifact to gain the stable prepared SID {expected_stable_sid} after the same-checkout refresh even with extra writable roots configured, got: {shared_acl:?}"
    );
    assert!(
        use_text.contains("SESSION_A_READ=from moved host tree with extra root"),
        "expected the older live session to read the host-renamed nested workspace artifact with extra writable roots configured, got: {use_text}"
    );
    assert!(
        use_text.contains("SESSION_A_WRITE_OK"),
        "expected the older live session to write the host-renamed nested workspace artifact with extra writable roots configured, got: {use_text}"
    );
    assert!(
        use_text.contains("SESSION_A_AFTER=from moved host tree with extra root|from session a"),
        "expected the older live session to read back its write on the host-renamed nested workspace artifact with extra writable roots configured, got: {use_text}"
    );
    assert!(
        !use_text.contains("SESSION_A_ACCESS_ERROR:"),
        "host-renamed nested workspace trees should stay shared across live same-checkout sessions even when extra writable roots are configured, got: {use_text}"
    );
    Ok(())
}

#[cfg(target_os = "windows")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_workspace_write_concurrent_sessions_share_host_renamed_nested_workspace_file()
-> TestResult<()> {
    let workspace = temp_workspace_root()?;
    let repo_root = workspace.path().to_path_buf();
    let nested_dir = repo_root.join("src").join("pkg");
    let shared = nested_dir.join(format!(
        "mcp-repl-sandbox-host-renamed-nested-{}.txt",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let temp_root = tempfile::tempdir()?;
    let host_temp = temp_root.path().join("host-temp.txt");
    let shared_r = r_string(&shared.to_string_lossy());
    scrub_unresolved_windows_sid_aces(&repo_root)?;
    let expected_stable_sid = windows_workspace_write_prepared_sid_for_cwd(&repo_root, &[])?;
    let mut session_a =
        spawn_server_with_sandbox_state_in_cwd(sandbox_state_workspace_write(false), &repo_root)
            .await?;

    let ready_a = session_a
        .write_stdin_raw_with("cat('SESSION_A_READY\\n')\n", Some(10.0))
        .await?;
    let ready_a_text = collect_text(&ready_a);
    if windows_sandbox_backend_unavailable(&ready_a_text) {
        eprintln!("windows restricted token setup unavailable; skipping");
        session_a.cancel().await?;
        cleanup_restricted_file(&shared);
        cleanup_restricted_file(&host_temp);
        cleanup_restricted_path(&nested_dir);
        return Ok(());
    }

    std::fs::create_dir_all(&nested_dir)?;
    std::fs::write(&host_temp, b"from host temp")?;
    std::fs::rename(&host_temp, &shared)?;

    let mut session_b =
        spawn_server_with_sandbox_state_in_cwd(sandbox_state_workspace_write(false), &repo_root)
            .await?;
    let ready_b = session_b
        .write_stdin_raw_with("cat('SESSION_B_READY\\n')\n", Some(10.0))
        .await?;
    let ready_b_text = collect_text(&ready_b);
    if windows_sandbox_backend_unavailable(&ready_b_text) {
        eprintln!("windows restricted token setup unavailable; skipping");
        session_a.cancel().await?;
        session_b.cancel().await?;
        cleanup_restricted_file(&shared);
        cleanup_restricted_file(&host_temp);
        cleanup_restricted_path(&nested_dir);
        return Ok(());
    }

    let shared_acl = unresolved_windows_sid_acl_entries(&shared)?;
    let use_code = format!(
        r#"
target <- {shared_r}
tryCatch({{
  cat("SESSION_A_READ=", paste(readLines(target, warn = FALSE), collapse = "|"), "\n", sep = "")
  writeLines(c(readLines(target, warn = FALSE), "from session a"), target)
  cat("SESSION_A_WRITE_OK\n")
  cat("SESSION_A_AFTER=", paste(readLines(target, warn = FALSE), collapse = "|"), "\n", sep = "")
}}, error = function(e) {{
  message("SESSION_A_ACCESS_ERROR:", conditionMessage(e))
}})
"#
    );
    let use_result = session_a.write_stdin_raw_with(use_code, Some(10.0)).await?;
    let use_text = collect_text(&use_result);

    cleanup_restricted_file(&shared);
    cleanup_restricted_file(&host_temp);
    cleanup_restricted_path(&nested_dir);
    session_a.cancel().await?;
    session_b.cancel().await?;

    assert!(
        shared_acl.contains(&expected_stable_sid),
        "expected host temp-renamed nested workspace artifact to gain the stable prepared SID {expected_stable_sid} after a same-checkout launch refresh, got: {shared_acl:?}"
    );
    assert!(
        use_text.contains("SESSION_A_READ=from host temp"),
        "expected older live session to read the host temp-renamed nested workspace artifact, got: {use_text}"
    );
    assert!(
        use_text.contains("SESSION_A_WRITE_OK"),
        "expected older live session to write the host temp-renamed nested workspace artifact, got: {use_text}"
    );
    assert!(
        use_text.contains("SESSION_A_AFTER=from host temp|from session a"),
        "expected older live session to read back its write on the host temp-renamed nested workspace artifact, got: {use_text}"
    );
    assert!(
        !use_text.contains("SESSION_A_ACCESS_ERROR:"),
        "host temp-renamed nested workspace artifacts should stay shared across live same-checkout sessions, got: {use_text}"
    );
    Ok(())
}

#[cfg(target_os = "windows")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_workspace_write_concurrent_sessions_share_host_renamed_nested_workspace_tree()
-> TestResult<()> {
    let workspace = temp_workspace_root()?;
    let repo_root = workspace.path().to_path_buf();
    let src_dir = repo_root.join("src");
    let nested_dir = src_dir.join("pkg");
    let shared = nested_dir.join(format!(
        "mcp-repl-sandbox-host-renamed-nested-tree-{}.txt",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let temp_root = tempfile::tempdir()?;
    let host_pkg = temp_root.path().join("pkg");
    let host_file = host_pkg.join(shared.file_name().expect("shared file name"));
    let shared_r = r_string(&shared.to_string_lossy());
    std::fs::create_dir_all(&src_dir)?;
    scrub_unresolved_windows_sid_aces(&repo_root)?;
    let expected_stable_sid = windows_workspace_write_prepared_sid_for_cwd(&repo_root, &[])?;
    let mut session_a =
        spawn_server_with_sandbox_state_in_cwd(sandbox_state_workspace_write(false), &repo_root)
            .await?;

    let ready_a = session_a
        .write_stdin_raw_with("cat('SESSION_A_READY\\n')\n", Some(10.0))
        .await?;
    let ready_a_text = collect_text(&ready_a);
    if windows_sandbox_backend_unavailable(&ready_a_text) {
        eprintln!("windows restricted token setup unavailable; skipping");
        session_a.cancel().await?;
        cleanup_restricted_path(&nested_dir);
        cleanup_restricted_path(&src_dir);
        return Ok(());
    }

    std::fs::create_dir_all(&host_pkg)?;
    std::fs::write(&host_file, b"from moved host tree")?;
    std::fs::rename(&host_pkg, &nested_dir)?;

    let mut session_b =
        spawn_server_with_sandbox_state_in_cwd(sandbox_state_workspace_write(false), &repo_root)
            .await?;
    let ready_b = session_b
        .write_stdin_raw_with("cat('SESSION_B_READY\\n')\n", Some(10.0))
        .await?;
    let ready_b_text = collect_text(&ready_b);
    if windows_sandbox_backend_unavailable(&ready_b_text) {
        eprintln!("windows restricted token setup unavailable; skipping");
        session_a.cancel().await?;
        session_b.cancel().await?;
        cleanup_restricted_path(&nested_dir);
        cleanup_restricted_path(&src_dir);
        return Ok(());
    }

    let nested_dir_acl = unresolved_windows_sid_acl_entries(&nested_dir)?;
    let shared_acl = unresolved_windows_sid_acl_entries(&shared)?;
    let use_code = format!(
        r#"
target <- {shared_r}
tryCatch({{
  cat("SESSION_A_READ=", paste(readLines(target, warn = FALSE), collapse = "|"), "\n", sep = "")
  writeLines(c(readLines(target, warn = FALSE), "from session a"), target)
  cat("SESSION_A_WRITE_OK\n")
  cat("SESSION_A_AFTER=", paste(readLines(target, warn = FALSE), collapse = "|"), "\n", sep = "")
}}, error = function(e) {{
  message("SESSION_A_ACCESS_ERROR:", conditionMessage(e))
}})
"#
    );
    let use_result = session_a.write_stdin_raw_with(use_code, Some(10.0)).await?;
    let use_text = collect_text(&use_result);

    cleanup_restricted_path(&nested_dir);
    cleanup_restricted_path(&src_dir);
    session_a.cancel().await?;
    session_b.cancel().await?;

    assert!(
        nested_dir_acl.contains(&expected_stable_sid),
        "expected host-renamed nested workspace directory to gain the stable prepared SID {expected_stable_sid} after a same-checkout launch refresh, got: {nested_dir_acl:?}"
    );
    assert!(
        shared_acl.contains(&expected_stable_sid),
        "expected host-renamed nested workspace artifact to gain the stable prepared SID {expected_stable_sid} after a same-checkout launch refresh, got: {shared_acl:?}"
    );
    assert!(
        use_text.contains("SESSION_A_READ=from moved host tree"),
        "expected older live session to read the host-renamed nested workspace artifact, got: {use_text}"
    );
    assert!(
        use_text.contains("SESSION_A_WRITE_OK"),
        "expected older live session to write the host-renamed nested workspace artifact, got: {use_text}"
    );
    assert!(
        use_text.contains("SESSION_A_AFTER=from moved host tree|from session a"),
        "expected older live session to read back its write on the host-renamed nested workspace artifact, got: {use_text}"
    );
    assert!(
        !use_text.contains("SESSION_A_ACCESS_ERROR:"),
        "host-renamed nested workspace trees should stay shared across live same-checkout sessions, got: {use_text}"
    );
    Ok(())
}

#[cfg(target_os = "windows")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_workspace_write_concurrent_sessions_share_file_created_inside_host_renamed_nested_workspace_tree()
-> TestResult<()> {
    let workspace = temp_workspace_root()?;
    let repo_root = workspace.path().to_path_buf();
    let src_dir = repo_root.join("src");
    let nested_dir = src_dir.join("pkg");
    let shared = nested_dir.join(format!(
        "mcp-repl-sandbox-renamed-tree-host-file-{}.txt",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let temp_root = tempfile::tempdir()?;
    let host_pkg = temp_root.path().join("pkg");
    let shared_r = r_string(&shared.to_string_lossy());
    std::fs::create_dir_all(&src_dir)?;
    scrub_unresolved_windows_sid_aces(&repo_root)?;
    let expected_stable_sid = windows_workspace_write_prepared_sid_for_cwd(&repo_root, &[])?;
    let mut session_a =
        spawn_server_with_sandbox_state_in_cwd(sandbox_state_workspace_write(false), &repo_root)
            .await?;

    let ready_a = session_a
        .write_stdin_raw_with("cat('SESSION_A_READY\\n')\n", Some(10.0))
        .await?;
    let ready_a_text = collect_text(&ready_a);
    if windows_sandbox_backend_unavailable(&ready_a_text) {
        eprintln!("windows restricted token setup unavailable; skipping");
        session_a.cancel().await?;
        cleanup_restricted_path(&nested_dir);
        cleanup_restricted_path(&src_dir);
        return Ok(());
    }

    std::fs::create_dir_all(&host_pkg)?;
    std::fs::rename(&host_pkg, &nested_dir)?;
    std::fs::write(&shared, b"from host file in renamed tree")?;

    let mut session_b =
        spawn_server_with_sandbox_state_in_cwd(sandbox_state_workspace_write(false), &repo_root)
            .await?;
    let ready_b = session_b
        .write_stdin_raw_with("cat('SESSION_B_READY\\n')\n", Some(10.0))
        .await?;
    let ready_b_text = collect_text(&ready_b);
    if windows_sandbox_backend_unavailable(&ready_b_text) {
        eprintln!("windows restricted token setup unavailable; skipping");
        session_a.cancel().await?;
        session_b.cancel().await?;
        cleanup_restricted_path(&nested_dir);
        cleanup_restricted_path(&src_dir);
        return Ok(());
    }

    let nested_dir_acl = unresolved_windows_sid_acl_entries(&nested_dir)?;
    let shared_acl = unresolved_windows_sid_acl_entries(&shared)?;
    let use_code = format!(
        r#"
target <- {shared_r}
tryCatch({{
  cat("SESSION_A_READ=", paste(readLines(target, warn = FALSE), collapse = "|"), "\n", sep = "")
  writeLines(c(readLines(target, warn = FALSE), "from session a"), target)
  cat("SESSION_A_WRITE_OK\n")
  cat("SESSION_A_AFTER=", paste(readLines(target, warn = FALSE), collapse = "|"), "\n", sep = "")
}}, error = function(e) {{
  message("SESSION_A_ACCESS_ERROR:", conditionMessage(e))
}})
"#
    );
    let use_result = session_a.write_stdin_raw_with(use_code, Some(10.0)).await?;
    let use_text = collect_text(&use_result);

    cleanup_restricted_path(&nested_dir);
    cleanup_restricted_path(&src_dir);
    session_a.cancel().await?;
    session_b.cancel().await?;

    assert!(
        nested_dir_acl.contains(&expected_stable_sid),
        "expected the renamed nested workspace directory to keep the stable prepared SID {expected_stable_sid}, got: {nested_dir_acl:?}"
    );
    assert!(
        shared_acl.contains(&expected_stable_sid),
        "expected the host-created file inside the renamed nested workspace tree to gain the stable prepared SID {expected_stable_sid} after a same-checkout launch refresh, got: {shared_acl:?}"
    );
    assert!(
        use_text.contains("SESSION_A_READ=from host file in renamed tree"),
        "expected older live session to read the host-created file inside the renamed nested workspace tree, got: {use_text}"
    );
    assert!(
        use_text.contains("SESSION_A_WRITE_OK"),
        "expected older live session to write the host-created file inside the renamed nested workspace tree, got: {use_text}"
    );
    assert!(
        use_text.contains("SESSION_A_AFTER=from host file in renamed tree|from session a"),
        "expected older live session to read back its write inside the renamed nested workspace tree, got: {use_text}"
    );
    assert!(
        !use_text.contains("SESSION_A_ACCESS_ERROR:"),
        "host-created files inside renamed nested workspace trees should stay shared across live same-checkout sessions, got: {use_text}"
    );
    Ok(())
}

#[cfg(target_os = "windows")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_workspace_write_direct_midrun_file_keeps_prepared_sid() -> TestResult<()> {
    let writable_root = tempfile::tempdir()?;
    let artifact = writable_root.path().join(format!(
        "mcp-repl-sandbox-direct-midrun-acl-{}.txt",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let artifact_r = r_string(&artifact.to_string_lossy());
    scrub_unresolved_windows_sid_aces(writable_root.path())?;
    let state =
        sandbox_state_workspace_write_with_roots(false, vec![writable_root.path().to_path_buf()]);
    let (mut session, _workspace, cwd) = spawn_server_with_sandbox_state_in_temp_cwd(state).await?;
    let expected_stable_sid =
        windows_workspace_write_prepared_sid_for_cwd(&cwd, &[writable_root.path().to_path_buf()])?;

    let create_code = format!(
        r#"
target <- {artifact_r}
writeLines("midrun", target)
cat("WRITE_OK=", file.exists(target), "\n", sep = "")
"#
    );
    let create = session
        .write_stdin_raw_with(create_code, Some(10.0))
        .await?;
    let create_text = collect_text(&create);
    if windows_sandbox_backend_unavailable(&create_text) {
        eprintln!("windows restricted token setup unavailable; skipping");
        session.cancel().await?;
        cleanup_restricted_file(&artifact);
        return Ok(());
    }
    assert!(
        create_text.contains("WRITE_OK=TRUE"),
        "expected sandboxed worker to create the direct mid-run artifact, got: {create_text}"
    );

    let before_cancel = unresolved_windows_sid_acl_entries(&artifact)?;
    cleanup_restricted_file(&artifact);
    session.cancel().await?;

    assert!(
        before_cancel.contains(&expected_stable_sid),
        "expected a direct file created under a writable root to retain the stable prepared SID {expected_stable_sid}, got: {before_cancel:?}"
    );
    Ok(())
}

#[cfg(target_os = "windows")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_workspace_write_session_exit_removes_launch_acl_from_nested_workspace_tree()
-> TestResult<()> {
    let workspace = temp_workspace_root()?;
    let repo_root = workspace.path().to_path_buf();
    let nested_dir = repo_root.join("src").join("pkg");
    let artifact = nested_dir.join(format!(
        "mcp-repl-sandbox-nested-exit-acl-{}.txt",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let nested_dir_r = r_string(&nested_dir.to_string_lossy());
    let artifact_r = r_string(&artifact.to_string_lossy());
    scrub_unresolved_windows_sid_aces(&repo_root)?;
    let expected_stable_sid = windows_workspace_write_prepared_sid_for_cwd(&repo_root, &[])?;
    let mut session =
        spawn_server_with_sandbox_state_in_cwd(sandbox_state_workspace_write(false), &repo_root)
            .await?;

    let create_code = format!(
        r#"
target_dir <- {nested_dir_r}
target <- {artifact_r}
dir.create(target_dir, recursive = TRUE, showWarnings = FALSE)
writeLines("nested exit", target)
cat("WRITE_OK=", file.exists(target), "\n", sep = "")
"#
    );
    let create = session
        .write_stdin_raw_with(create_code, Some(10.0))
        .await?;
    let create_text = collect_text(&create);
    if windows_sandbox_backend_unavailable(&create_text) {
        eprintln!("windows restricted token setup unavailable; skipping");
        session.cancel().await?;
        cleanup_restricted_file(&artifact);
        cleanup_restricted_path(&nested_dir);
        return Ok(());
    }
    assert!(
        create_text.contains("WRITE_OK=TRUE"),
        "expected sandboxed worker to create the nested workspace tree, got: {create_text}"
    );

    let before_cancel_dir = unresolved_windows_sid_acl_entries(&nested_dir)?;
    let before_cancel_file = unresolved_windows_sid_acl_entries(&artifact)?;
    let before_dir_non_stable = before_cancel_dir
        .iter()
        .filter(|sid| *sid != &expected_stable_sid)
        .cloned()
        .collect::<Vec<_>>();
    let before_file_non_stable = before_cancel_file
        .iter()
        .filter(|sid| *sid != &expected_stable_sid)
        .cloned()
        .collect::<Vec<_>>();
    if !before_cancel_dir.contains(&expected_stable_sid)
        || !before_cancel_file.contains(&expected_stable_sid)
    {
        eprintln!(
            "prepared launch SID not active on this public Windows surface; skipping nested ACL teardown probe"
        );
        cleanup_restricted_file(&artifact);
        cleanup_restricted_path(&nested_dir);
        session.cancel().await?;
        return Ok(());
    }

    let session_end = session
        .write_stdin_raw_with("quit(\"no\")", Some(10.0))
        .await?;
    let session_end_text = collect_text(&session_end);
    assert!(
        session_end_text.contains("session ended")
            || session_end_text.contains("ipc disconnected while waiting for request completion"),
        "expected backend quit to end the worker session, got: {session_end_text}"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let (after_cancel_dir, after_cancel_file) = loop {
        let current_dir = unresolved_windows_sid_acl_entries(&nested_dir)?;
        let current_file = unresolved_windows_sid_acl_entries(&artifact)?;
        let old_dir_launch_removed = before_dir_non_stable
            .iter()
            .all(|sid| !current_dir.contains(sid));
        let old_file_launch_removed = before_file_non_stable
            .iter()
            .all(|sid| !current_file.contains(sid));
        if current_dir.contains(&expected_stable_sid)
            && current_file.contains(&expected_stable_sid)
            && old_dir_launch_removed
            && old_file_launch_removed
        {
            break (current_dir, current_file);
        }
        if std::time::Instant::now() >= deadline {
            break (current_dir, current_file);
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    };

    cleanup_restricted_file(&artifact);
    cleanup_restricted_path(&nested_dir);
    session.cancel().await?;

    let after_dir_non_stable = after_cancel_dir
        .iter()
        .filter(|sid| *sid != &expected_stable_sid)
        .cloned()
        .collect::<Vec<_>>();
    let after_file_non_stable = after_cancel_file
        .iter()
        .filter(|sid| *sid != &expected_stable_sid)
        .cloned()
        .collect::<Vec<_>>();

    assert!(
        !before_dir_non_stable.is_empty() || !before_file_non_stable.is_empty(),
        "expected nested workspace ACLs to include launch-scoped SIDs before session exit, got dir={before_cancel_dir:?} file={before_cancel_file:?}"
    );
    if before_dir_non_stable
        .iter()
        .any(|sid| after_cancel_dir.contains(sid))
        && after_cancel_dir
            .iter()
            .any(|sid| sid != &expected_stable_sid && !before_cancel_dir.contains(sid))
    {
        eprintln!(
            "eager worker respawn re-applied a fresh launch SID before the old nested directory cleanup settled on this public Windows surface; skipping teardown probe"
        );
        return Ok(());
    }
    if before_file_non_stable
        .iter()
        .any(|sid| after_cancel_file.contains(sid))
        && after_cancel_file
            .iter()
            .any(|sid| sid != &expected_stable_sid && !before_cancel_file.contains(sid))
    {
        eprintln!(
            "eager worker respawn re-applied a fresh launch SID before the old nested file cleanup settled on this public Windows surface; skipping teardown probe"
        );
        return Ok(());
    }
    assert!(
        after_dir_non_stable.is_empty(),
        "expected session exit to remove launch-scoped SIDs from the nested workspace directory, got: {after_cancel_dir:?}"
    );
    assert!(
        after_file_non_stable.is_empty(),
        "expected session exit to remove launch-scoped SIDs from the nested workspace file, got: {after_cancel_file:?}"
    );
    Ok(())
}

#[cfg(target_os = "windows")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_workspace_write_first_launch_accepts_missing_writable_root_parent_segment()
-> TestResult<()> {
    let writable_root_parent = tempfile::tempdir()?;
    let workspace = temp_workspace_root()?;
    let cwd = workspace.path().join("workspace");
    let actual_root = writable_root_parent.path().join("out");
    let declared_root = writable_root_parent
        .path()
        .join("child")
        .join("..")
        .join("out");
    let artifact = actual_root.join(format!(
        "mcp-repl-sandbox-missing-parent-segment-{}.txt",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let artifact_r = r_string(&artifact.to_string_lossy());
    scrub_unresolved_windows_sid_aces(writable_root_parent.path())?;
    std::fs::create_dir_all(&cwd)?;
    let mut session = spawn_server_with_sandbox_state_in_cwd(
        sandbox_state_workspace_write_with_roots(false, vec![declared_root]),
        &cwd,
    )
    .await?;

    let create_code = format!(
        r#"
target <- {artifact_r}
tryCatch({{
  writeLines("ok", target)
  cat("WRITE_OK=", file.exists(target), "\n", sep = "")
}}, error = function(e) {{
  message("WRITE_ERROR:", conditionMessage(e))
}})
"#
    );
    let create = session
        .write_stdin_raw_with(create_code, Some(10.0))
        .await?;
    let create_text = collect_text(&create);
    if windows_sandbox_backend_unavailable(&create_text) {
        eprintln!("windows restricted token setup unavailable; skipping");
        session.cancel().await?;
        cleanup_restricted_file(&artifact);
        return Ok(());
    }

    cleanup_restricted_file(&artifact);
    session.cancel().await?;

    assert!(
        create_text.contains("WRITE_OK=TRUE"),
        "expected first launch to accept a missing writable root spelled with parent segments, got: {create_text}"
    );
    Ok(())
}

#[cfg(target_os = "windows")]
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_workspace_write_session_exit_removes_launch_acl_from_midrun_file() -> TestResult<()>
{
    let writable_root = tempfile::tempdir()?;
    let workspace = temp_workspace_root()?;
    let cwd = workspace.path().join("workspace");
    let artifact = writable_root.path().join(format!(
        "mcp-repl-sandbox-midrun-acl-{}.txt",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let artifact_r = r_string(&artifact.to_string_lossy());
    scrub_unresolved_windows_sid_aces(writable_root.path())?;
    std::fs::create_dir_all(&cwd)?;
    let expected_stable_sid =
        windows_workspace_write_prepared_sid_for_cwd(&cwd, &[writable_root.path().to_path_buf()])?;
    let mut session = spawn_server_with_sandbox_state_in_cwd(
        sandbox_state_workspace_write_with_roots(false, vec![writable_root.path().to_path_buf()]),
        &cwd,
    )
    .await?;

    let create_code = format!(
        r#"
target <- {artifact_r}
writeLines("midrun", target)
cat("WRITE_OK=", file.exists(target), "\n", sep = "")
"#
    );
    let create = session
        .write_stdin_raw_with(create_code, Some(10.0))
        .await?;
    let create_text = collect_text(&create);
    if windows_sandbox_backend_unavailable(&create_text) {
        eprintln!("windows restricted token setup unavailable; skipping");
        session.cancel().await?;
        cleanup_restricted_file(&artifact);
        return Ok(());
    }
    assert!(
        create_text.contains("WRITE_OK=TRUE"),
        "expected sandboxed worker to create the mid-run artifact, got: {create_text}"
    );

    let before_cancel = unresolved_windows_sid_acl_entries(&artifact)?;
    if !before_cancel.contains(&expected_stable_sid) {
        eprintln!(
            "prepared launch SID not active on this public Windows surface; skipping mid-run ACL teardown probe"
        );
        cleanup_restricted_file(&artifact);
        session.cancel().await?;
        return Ok(());
    }
    let session_end = session
        .write_stdin_raw_with("quit(\"no\")", Some(10.0))
        .await?;
    let session_end_text = collect_text(&session_end);
    assert!(
        session_end_text.contains("session ended")
            || session_end_text.contains("ipc disconnected while waiting for request completion"),
        "expected backend quit to end the worker session, got: {session_end_text}"
    );
    let before_non_stable = before_cancel
        .iter()
        .filter(|sid| *sid != &expected_stable_sid)
        .cloned()
        .collect::<Vec<_>>();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let after_cancel = loop {
        let current = unresolved_windows_sid_acl_entries(&artifact)?;
        let old_launch_removed = before_non_stable.iter().all(|sid| !current.contains(sid));
        if current.contains(&expected_stable_sid) && old_launch_removed {
            break current;
        }
        if std::time::Instant::now() >= deadline {
            break current;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    };
    cleanup_restricted_file(&artifact);
    session.cancel().await?;

    assert!(
        !before_cancel.is_empty(),
        "expected a live sandbox-created file to carry at least one unresolved capability SID, got: {before_cancel:?}"
    );
    assert!(
        !after_cancel.is_empty(),
        "expected the artifact to keep sandbox ACL state after restart, got: {after_cancel:?}"
    );
    assert_ne!(
        before_cancel, after_cancel,
        "expected session shutdown to remove launch-scoped ACEs from files created mid-run; expected stable sid: {expected_stable_sid}; before={before_cancel:?}; after={after_cancel:?}"
    );
    if before_non_stable
        .iter()
        .any(|sid| after_cancel.contains(sid))
        && after_cancel
            .iter()
            .any(|sid| sid != &expected_stable_sid && !before_cancel.contains(sid))
    {
        eprintln!(
            "eager worker respawn re-applied a fresh launch SID before the old launch cleanup settled on this public Windows surface; skipping teardown probe"
        );
        return Ok(());
    }
    assert!(
        before_non_stable
            .iter()
            .all(|sid| !after_cancel.contains(sid)),
        "expected pre-shutdown launch-scoped ACEs to be removed after session shutdown, before={before_cancel:?} after={after_cancel:?}"
    );
    assert!(
        after_cancel.contains(&expected_stable_sid),
        "expected restarted artifact ACLs to retain the stable prepared SID {expected_stable_sid}, got: {after_cancel:?}"
    );
    Ok(())
}
