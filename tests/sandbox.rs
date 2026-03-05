mod common;

#[cfg(target_os = "windows")]
use std::path::PathBuf;
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
const SESSION_MARKER_FILE: &str = "mcp-console-session-marker.txt";
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

#[cfg(any(target_os = "macos", target_os = "linux"))]
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
cat("MCP_TMPDIR=", Sys.getenv("MCP_CONSOLE_R_SESSION_TMPDIR"), "\n", sep = "")
cat("TMPDIR=", Sys.getenv("TMPDIR"), "\n", sep = "")
cat("R_TMPDIR=", tempdir(), "\n", sep = "")
marker <- file.path(Sys.getenv("TMPDIR"), "mcp-console-session-marker.txt")
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
cat("MCP_TMPDIR=", Sys.getenv("MCP_CONSOLE_R_SESSION_TMPDIR"), "\n", sep = "")
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
    root.join(format!("mcp-console-sandbox-{label}-{nanos}.txt"))
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
    let root = home.join(format!(".mcp-console-sandbox-r-cache-probe-{nanos}"));

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
    let root = home.join(format!(
        ".mcp-console-sandbox-r-cache-probe-read-only-{nanos}"
    ));

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
    let sentinel = format!("/tmp/mcp-console-preexisting-{nanos}");
    let mut session = common::spawn_server_with_args_env_and_pager_page_chars(
        Vec::new(),
        vec![("R_SESSION_TMPDIR".to_string(), sentinel.clone())],
        SANDBOX_PAGER_PAGE_CHARS,
    )
    .await?;

    let code = r#"
cat("R_SESSION_TMPDIR=", Sys.getenv("R_SESSION_TMPDIR"), "\n", sep = "")
cat("TMPDIR=", Sys.getenv("TMPDIR"), "\n", sep = "")
cat("MCP_TMPDIR=", Sys.getenv("MCP_CONSOLE_R_SESSION_TMPDIR"), "\n", sep = "")
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
    let forbidden = Path::new(&home).join(format!("mcp-console-denied-{nanos}.txt"));
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
    let forbidden = Path::new(&home).join(format!("mcp-console-bwrap-denied-{nanos}.txt"));
    let forbidden = forbidden.to_string_lossy().to_string();

    let mut session = spawn_server_with_sandbox_state_and_env(
        sandbox_state_workspace_write(false),
        vec![("MCP_CONSOLE_USE_LINUX_BWRAP".to_string(), "1".to_string())],
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
    let writable_root = repo_root.join(format!("mcp-console-bwrap-root-{nanos}"));
    std::fs::create_dir_all(writable_root.join(".git"))?;
    std::fs::create_dir_all(writable_root.join(".codex"))?;
    std::fs::create_dir_all(writable_root.join(".agents"))?;

    let mut session = spawn_server_with_sandbox_state_and_env(
        sandbox_state_workspace_write_with_roots(false, vec![writable_root.clone()]),
        vec![("MCP_CONSOLE_USE_LINUX_BWRAP".to_string(), "1".to_string())],
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
        vec![("MCP_CONSOLE_USE_LINUX_BWRAP".to_string(), "1".to_string())],
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
            ("MCP_CONSOLE_USE_LINUX_BWRAP".to_string(), "1".to_string()),
            (
                "MCP_CONSOLE_LINUX_BWRAP_NO_PROC".to_string(),
                "1".to_string(),
            ),
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
    let forbidden = home.join(format!("mcp-console-denied-{nanos}.txt"));
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
    if text.contains("CreateRestrictedToken failed: 87")
        || text.contains("worker exited before IPC named pipe connection")
        || text.contains("timed out waiting for IPC named pipe client connection")
    {
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
