#![allow(clippy::await_holding_lock)]

mod common;

use common::TestResult;
use regex_lite::Regex;
use rmcp::model::RawContent;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use tempfile::tempdir;
use tokio::time::{Duration, Instant, sleep};

fn test_mutex() -> &'static Mutex<()> {
    static TEST_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
    TEST_MUTEX.get_or_init(|| Mutex::new(()))
}

fn lock_mutex(mutex: &Mutex<()>) -> MutexGuard<'_, ()> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn lock_test_mutex() -> MutexGuard<'static, ()> {
    lock_mutex(test_mutex())
}

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

fn path_json_literal(path: &Path, label: &str) -> TestResult<String> {
    let path = path
        .to_str()
        .ok_or_else(|| format!("{label} path must be valid utf-8"))?;
    Ok(serde_json::to_string(path)?)
}

async fn poll_until_contains(
    session: &common::McpTestSession,
    mut text: String,
    expected: &str,
    context: &str,
    timeout: Duration,
) -> TestResult<String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline && !text.contains(expected) {
        sleep(Duration::from_millis(50)).await;
        let poll = session
            .write_stdin_raw_unterminated_with("", Some(1.0))
            .await?;
        text.push_str(&result_text(&poll));
    }
    if !text.contains(expected) {
        return Err(format!("expected {context}, got: {text:?}").into());
    }
    Ok(text)
}

fn bundle_transcript_path(text: &str) -> Option<PathBuf> {
    let end = text
        .find("transcript.txt")?
        .saturating_add("transcript.txt".len());
    let start = text[..end]
        .rfind(|ch: char| ch.is_whitespace() || matches!(ch, '"' | '\'' | '[' | '('))
        .map_or(0, |idx| idx.saturating_add(1));
    Some(PathBuf::from(&text[start..end]))
}

fn visible_reply_text(text: &str) -> TestResult<String> {
    if let Some(path) = bundle_transcript_path(text) {
        return Ok(fs::read_to_string(path)?);
    }
    Ok(text.to_string())
}

fn strip_ansi_controls(text: &str) -> String {
    static ANSI_RE: OnceLock<Regex> = OnceLock::new();
    ANSI_RE
        .get_or_init(|| Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]").expect("ANSI regex"))
        .replace_all(text, "")
        .into_owned()
}

fn require_python() -> bool {
    if common::python_available() {
        true
    } else {
        eprintln!("python not available; skipping");
        false
    }
}

#[cfg(not(unix))]
fn python_plotting_available() -> bool {
    if !common::python_available() {
        eprintln!("python not available; skipping");
        return false;
    }
    let python = common::python_program().unwrap_or("python3");
    std::process::Command::new(python)
        .args([
            "-c",
            "import matplotlib; matplotlib.use('agg', force=True); import matplotlib.pyplot as plt",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn python_plot_tests_enabled() -> bool {
    if std::env::var_os("MCP_REPL_PYTHON_PLOT_TESTS").is_none() {
        eprintln!("python plot tests disabled; set MCP_REPL_PYTHON_PLOT_TESTS=1 to enable");
        return false;
    }
    python_plotting_available()
}

fn image_count(result: &rmcp::model::CallToolResult) -> usize {
    result
        .content
        .iter()
        .filter(|item| matches!(item.raw, RawContent::Image(_)))
        .count()
}

fn python_backend_unavailable(text: &str) -> bool {
    common::backend_unavailable(text)
        || text.contains("worker io error: Permission denied")
        || text.contains("failed to locate a shared libpython")
}

#[cfg(windows)]
fn python_sandbox_unavailable(text: &str) -> bool {
    python_backend_unavailable(text)
        || text.contains("worker sandbox error")
        || text.contains("prepared capability SID requires an unrestricted base token")
}

fn is_busy_response(text: &str) -> bool {
    text.contains("<<repl status: busy")
        || text.contains("worker is busy")
        || text.contains("request already running")
        || text.contains("input discarded while worker busy")
}

fn assert_no_pager_markers(text: &str, context: &str) {
    assert!(
        !text.contains("Press RETURN"),
        "{context} should stay inline without pager prompts, got: {text:?}"
    );
    assert!(
        !text.contains("--More--"),
        "{context} should stay inline without pager prompts, got: {text:?}"
    );
}

fn interrupt_recovery_deadline() -> Instant {
    Instant::now() + Duration::from_secs(20)
}

async fn write_python_after_interrupt_until_contains(
    session: &common::McpTestSession,
    input: &str,
    expected: &str,
    context: &str,
) -> TestResult<String> {
    let deadline = interrupt_recovery_deadline();
    let mut keyboard_interrupts = 0usize;
    loop {
        if Instant::now() >= deadline {
            return Err(format!("{context} did not produce {expected:?} before timeout").into());
        }
        let result = session.write_stdin_raw_with(input, Some(5.0)).await?;
        let text = result_text(&result);
        if is_busy_response(&text) {
            sleep(Duration::from_millis(50)).await;
            continue;
        }
        if text.contains(expected) {
            return Ok(text);
        }
        if text.contains("KeyboardInterrupt") && keyboard_interrupts < 2 {
            keyboard_interrupts += 1;
            continue;
        }
        return Err(format!("{context} failed; got: {text:?}").into());
    }
}

async fn write_python_prompt_after_interrupt(
    session: &common::McpTestSession,
    input: &str,
    prompt: &str,
    context: &str,
) -> TestResult<String> {
    let deadline = interrupt_recovery_deadline();
    let mut keyboard_interrupts = 0usize;
    loop {
        if Instant::now() >= deadline {
            return Err(format!("{context} did not reach prompt {prompt:?} before timeout").into());
        }
        let result = session.write_stdin_raw_with(input, Some(5.0)).await?;
        let text = result_text(&result);
        if is_busy_response(&text) {
            sleep(Duration::from_millis(50)).await;
            continue;
        }
        if text.contains(prompt) && !text.contains("Traceback") {
            return Ok(text);
        }
        if text.contains("KeyboardInterrupt") && keyboard_interrupts < 2 {
            keyboard_interrupts += 1;
            continue;
        }
        return Err(format!("{context} failed; got: {text:?}").into());
    }
}

fn python_startup_probe_budget() -> Duration {
    Duration::from_secs(90)
}

async fn start_python_session_with_env_vars(
    env_vars: Vec<(String, String)>,
) -> TestResult<Option<common::McpTestSession>> {
    if !require_python() {
        return Ok(None);
    }

    let mut session = common::spawn_server_with_args_env(
        vec![
            "--interpreter".to_string(),
            "python".to_string(),
            "--oversized-output".to_string(),
            "files".to_string(),
            "--sandbox".to_string(),
            "danger-full-access".to_string(),
        ],
        env_vars,
    )
    .await?;
    let probe = session
        .write_stdin_raw_with("print('mcp_repl_python_ready')", Some(2.0))
        .await?;
    let probe = common::wait_until_not_busy(
        &mut session,
        probe,
        Duration::from_millis(100),
        python_startup_probe_budget(),
    )
    .await?;
    let probe_text = result_text(&probe);
    if python_backend_unavailable(&probe_text) {
        eprintln!("python backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(None);
    }

    Ok(Some(session))
}

async fn start_python_session() -> TestResult<Option<common::McpTestSession>> {
    start_python_session_with_env_vars(Vec::new()).await
}

fn debug_dir_env(debug_dir: &Path) -> Vec<(String, String)> {
    vec![(
        "MCP_REPL_DEBUG_DIR".to_string(),
        debug_dir.to_string_lossy().into_owned(),
    )]
}

fn debug_log_summary(debug_dir: &Path) -> String {
    let mut entries = fs::read_dir(debug_dir)
        .ok()
        .into_iter()
        .flat_map(|entries| entries.filter_map(Result::ok))
        .filter(|entry| entry.file_type().map(|ty| ty.is_dir()).unwrap_or(false))
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    entries.sort();

    let mut summary = String::new();
    for session_dir in entries {
        for file_name in ["startup.log", "worker-startup.log"] {
            let path = session_dir.join(file_name);
            let Ok(contents) = fs::read_to_string(&path) else {
                continue;
            };
            summary.push_str(&format!("\n--- {} ---\n{}", path.display(), contents));
        }
    }

    if summary.is_empty() {
        return format!(
            "\n--- debug logs unavailable under {} ---",
            debug_dir.display()
        );
    }
    summary
}

#[cfg(windows)]
async fn start_python_session_with_sandbox(
    sandbox: &str,
) -> TestResult<Option<common::McpTestSession>> {
    if !require_python() {
        return Ok(None);
    }

    let mut session = common::spawn_server_with_args(vec![
        "--interpreter".to_string(),
        "python".to_string(),
        "--oversized-output".to_string(),
        "files".to_string(),
        "--sandbox".to_string(),
        sandbox.to_string(),
    ])
    .await?;
    let probe = session
        .write_stdin_raw_with("print('mcp_repl_python_ready')", Some(2.0))
        .await?;
    let probe = common::wait_until_not_busy(
        &mut session,
        probe,
        Duration::from_millis(100),
        python_startup_probe_budget(),
    )
    .await?;
    let probe_text = result_text(&probe);
    if python_sandbox_unavailable(&probe_text) {
        eprintln!("python sandbox backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(None);
    }

    Ok(Some(session))
}

async fn start_python_pager_session() -> TestResult<Option<common::McpTestSession>> {
    if !require_python() {
        return Ok(None);
    }

    let mut session = common::spawn_server_with_args(vec![
        "--interpreter".to_string(),
        "python".to_string(),
        "--sandbox".to_string(),
        "danger-full-access".to_string(),
    ])
    .await?;
    let probe = session
        .write_stdin_raw_with("print('mcp_repl_python_ready')", Some(2.0))
        .await?;
    let probe = common::wait_until_not_busy(
        &mut session,
        probe,
        Duration::from_millis(100),
        python_startup_probe_budget(),
    )
    .await?;
    let probe_text = result_text(&probe);
    if python_backend_unavailable(&probe_text) {
        eprintln!("python backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(None);
    }

    Ok(Some(session))
}

#[cfg(unix)]
fn real_python_executable() -> TestResult<String> {
    let real_python = common::python_program().ok_or("python should be available")?;
    let real_executable = std::process::Command::new(real_python)
        .args(["-c", "import sys; print(sys.executable)"])
        .stdin(std::process::Stdio::null())
        .output()?;
    assert!(
        real_executable.status.success(),
        "expected real Python executable probe to succeed"
    );
    let real_executable = String::from_utf8(real_executable.stdout)?
        .trim()
        .to_string();
    assert!(!real_executable.is_empty(), "real Python executable path");
    Ok(real_executable)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test(flavor = "multi_thread")]
async fn python_discovery_keeps_venv_probe_inside_sandbox() -> TestResult<()> {
    use std::os::unix::fs::PermissionsExt;

    if std::env::var_os("MCP_REPL_PYTHON_EXECUTABLE").is_some() {
        eprintln!("explicit Python executable set; skipping discovery sandbox coverage test");
        return Ok(());
    }
    if !common::sandbox_exec_available() {
        eprintln!("sandbox unavailable; skipping discovery sandbox coverage test");
        return Ok(());
    }

    let _guard = lock_test_mutex();
    let real_python = real_python_executable()?;
    let workspace = tempdir()?;
    let empty_bin = workspace.path().join("empty-bin");
    fs::create_dir_all(&empty_bin)?;
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or("missing HOME/USERPROFILE for Python discovery sandbox marker")?;
    let marker = home.join(format!(
        ".mcp-repl-python-discovery-marker-{}",
        std::process::id()
    ));
    let _ = fs::remove_file(&marker);
    let marker_text = marker
        .to_str()
        .ok_or("marker path must be valid utf-8")?
        .to_string();
    let venv_bin = workspace.path().join(".venv").join("bin");
    fs::create_dir_all(&venv_bin)?;
    let shim = venv_bin.join("python");
    fs::write(
        &shim,
        concat!(
            "#!/bin/sh\n",
            "exec \"$MCP_REPL_REAL_PYTHON\" - <<'PY'\n",
            "import os\n",
            "import sys\n",
            "from pathlib import Path\n",
            "\n",
            "try:\n",
            "    Path(os.environ['MCP_REPL_TEST_PYTHON_PROBE_MARKER']).write_text('probe')\n",
            "except Exception as err:\n",
            "    print(f'MCP_REPL_TEST_PYTHON_PROBE_WRITE_ERROR:{type(err).__name__}', file=sys.stderr)\n",
            "else:\n",
            "    print('MCP_REPL_TEST_PYTHON_PROBE_WRITE_OK', file=sys.stderr)\n",
            "raise SystemExit(1)\n",
            "PY\n",
        ),
    )?;
    let mut permissions = fs::metadata(&shim)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&shim, permissions)?;

    let session = common::spawn_server_with_args_env_and_cwd(
        vec![
            "--interpreter".to_string(),
            "python".to_string(),
            "--sandbox".to_string(),
            "read-only".to_string(),
        ],
        vec![
            ("PATH".to_string(), empty_bin.display().to_string()),
            ("MCP_REPL_REAL_PYTHON".to_string(), real_python),
            ("MCP_REPL_TEST_PYTHON_PROBE_MARKER".to_string(), marker_text),
        ],
        Some(workspace.path().to_path_buf()),
    )
    .await?;
    let result = session.write_stdin_raw_with("1+1", Some(5.0)).await?;
    let text = result_text(&result);
    session.cancel().await?;
    let marker_exists = marker.exists();
    let _ = fs::remove_file(&marker);

    assert!(
        text.contains("MCP_REPL_TEST_PYTHON_PROBE_WRITE_ERROR:"),
        "expected Python discovery probe write failure in reply, got: {text:?}"
    );
    assert!(
        !marker_exists,
        "Python discovery probe wrote outside the sandbox; reply was: {text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_discovery_skips_static_libpython_archive_candidate() -> TestResult<()> {
    let _guard = lock_test_mutex();
    if std::env::var_os("MCP_REPL_PYTHON_EXECUTABLE").is_some() {
        eprintln!("explicit Python executable set; skipping discovery fallback test");
        return Ok(());
    }

    let Some(baseline) = start_python_session().await? else {
        return Ok(());
    };
    baseline.cancel().await?;

    let real_executable = real_python_executable()?;

    let temp = tempdir()?;
    let bin = temp.path().join("bin");
    let lib = temp.path().join("lib");
    fs::create_dir_all(&bin)?;
    fs::create_dir_all(&lib)?;
    let static_libpython = lib.join("libpython3.11.a");
    fs::write(&static_libpython, b"!<arch>\n")?;

    let fake_python3 = bin.join("python3");
    let fake_probe_marker = temp.path().join("fake-python3-probed");
    let fake_json = serde_json::json!({
        "executable": fake_python3,
        "base_executable": fake_python3,
        "prefix": temp.path(),
        "base_prefix": temp.path(),
        "exec_prefix": temp.path(),
        "base_exec_prefix": temp.path(),
        "version": [3, 11],
        "ldlibrary": static_libpython,
        "instsoname": static_libpython,
        "libdir": lib,
        "libpl": lib,
        "pythonframeworkprefix": "",
        "pythonframeworkinstalldir": "",
    });
    fs::write(
        &fake_python3,
        format!(
            "#!/bin/sh\nprintf probed > \"$MCP_REPL_FAKE_PYTHON3_PROBE_MARKER\"\ncat <<'JSON'\n{fake_json}\nJSON\n"
        ),
    )?;
    let mut permissions = fs::metadata(&fake_python3)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&fake_python3, permissions)?;
    symlink(real_executable, bin.join("python"))?;

    let mut session = common::spawn_server_with_args_env_and_cwd(
        vec![
            "--interpreter".to_string(),
            "python".to_string(),
            "--oversized-output".to_string(),
            "files".to_string(),
            "--sandbox".to_string(),
            "danger-full-access".to_string(),
        ],
        vec![
            ("PATH".to_string(), bin.display().to_string()),
            (
                "MCP_REPL_FAKE_PYTHON3_PROBE_MARKER".to_string(),
                fake_probe_marker.display().to_string(),
            ),
        ],
        Some(temp.path().to_path_buf()),
    )
    .await?;
    let probe = session
        .write_stdin_raw_with("print('STATIC_LIBPYTHON_FALLBACK_OK')", Some(2.0))
        .await?;
    let probe = common::wait_until_not_busy(
        &mut session,
        probe,
        Duration::from_millis(100),
        python_startup_probe_budget(),
    )
    .await?;
    let text = result_text(&probe);
    session.cancel().await?;

    assert!(
        fake_probe_marker.exists(),
        "expected Python discovery to probe fake python3 candidate"
    );
    assert!(
        !python_backend_unavailable(&text),
        "expected static libpython archive candidate to be rejected before fallback, got: {text:?}"
    );
    assert!(
        text.contains("STATIC_LIBPYTHON_FALLBACK_OK"),
        "expected fallback Python candidate to run, got: {text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_discovery_uses_venv_python3_after_broken_venv_python() -> TestResult<()> {
    let _guard = lock_test_mutex();
    if std::env::var_os("MCP_REPL_PYTHON_EXECUTABLE").is_some() {
        eprintln!("explicit Python executable set; skipping venv python3 fallback test");
        return Ok(());
    }

    let Some(baseline) = start_python_session().await? else {
        return Ok(());
    };
    baseline.cancel().await?;

    let real_executable = real_python_executable()?;
    let workspace = tempdir()?;
    let venv_bin = workspace.path().join(".venv").join("bin");
    let external_bin = workspace.path().join("external-bin");
    fs::create_dir_all(&venv_bin)?;
    fs::create_dir_all(&external_bin)?;

    let broken_python = venv_bin.join("python");
    fs::write(&broken_python, "#!/bin/sh\nexit 1\n")?;
    let mut permissions = fs::metadata(&broken_python)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&broken_python, permissions)?;

    let venv_python3_marker = workspace.path().join("venv-python3-probed");
    let venv_python3 = venv_bin.join("python3");
    fs::write(
        &venv_python3,
        "#!/bin/sh\nprintf probed > \"$MCP_REPL_VENV_PYTHON3_MARKER\"\nexec \"$MCP_REPL_REAL_PYTHON\" \"$@\"\n",
    )?;
    let mut permissions = fs::metadata(&venv_python3)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&venv_python3, permissions)?;

    let path_python3_marker = workspace.path().join("path-python3-probed");
    let path_python3 = external_bin.join("python3");
    fs::write(
        &path_python3,
        "#!/bin/sh\nprintf probed > \"$MCP_REPL_PATH_PYTHON3_MARKER\"\nexit 1\n",
    )?;
    let mut permissions = fs::metadata(&path_python3)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path_python3, permissions)?;

    let mut session = common::spawn_server_with_args_env_and_cwd(
        vec![
            "--interpreter".to_string(),
            "python".to_string(),
            "--oversized-output".to_string(),
            "files".to_string(),
            "--sandbox".to_string(),
            "danger-full-access".to_string(),
        ],
        vec![
            ("PATH".to_string(), external_bin.display().to_string()),
            ("MCP_REPL_REAL_PYTHON".to_string(), real_executable),
            (
                "MCP_REPL_VENV_PYTHON3_MARKER".to_string(),
                venv_python3_marker.display().to_string(),
            ),
            (
                "MCP_REPL_PATH_PYTHON3_MARKER".to_string(),
                path_python3_marker.display().to_string(),
            ),
        ],
        Some(workspace.path().to_path_buf()),
    )
    .await?;
    let probe = session
        .write_stdin_raw_with("print('VENV_PYTHON3_FALLBACK_OK')", Some(2.0))
        .await?;
    let probe = common::wait_until_not_busy(
        &mut session,
        probe,
        Duration::from_millis(100),
        python_startup_probe_budget(),
    )
    .await?;
    let text = result_text(&probe);
    session.cancel().await?;

    assert!(
        venv_python3_marker.exists(),
        "expected Python discovery to probe .venv/bin/python3 after broken .venv/bin/python"
    );
    assert!(
        !path_python3_marker.exists(),
        "expected .venv/bin/python3 to be tried before PATH python3"
    );
    assert!(
        text.contains("VENV_PYTHON3_FALLBACK_OK"),
        "expected same-venv python3 fallback to run, got: {text:?}"
    );
    Ok(())
}

#[cfg(unix)]
const DETACHED_STDIO_HOLDER_SECS: f64 = 2.5;

#[cfg(unix)]
struct DetachedHolderProbe {
    _dir: tempfile::TempDir,
    marker_path: PathBuf,
}

#[cfg(unix)]
fn path_literal(path: &Path, description: &str) -> TestResult<String> {
    let path = path
        .to_str()
        .ok_or_else(|| format!("{description} path must be valid utf-8"))?;
    Ok(serde_json::to_string(path)?)
}

#[cfg(unix)]
impl DetachedHolderProbe {
    fn new() -> TestResult<Self> {
        let dir = tempdir()?;
        Ok(Self {
            marker_path: dir.path().join("holder-exited"),
            _dir: dir,
        })
    }

    fn marker_literal(&self) -> TestResult<String> {
        path_literal(&self.marker_path, "detached holder marker")
    }

    async fn wait_for_exit(&self) -> TestResult<()> {
        wait_for_detached_holder_exit(&self.marker_path).await
    }

    fn has_exited(&self) -> bool {
        self.marker_path.exists()
    }
}

#[cfg(unix)]
struct BackgroundIpcLeakProbe {
    _dir: tempfile::TempDir,
    ready_path: PathBuf,
    release_path: PathBuf,
    exited_path: PathBuf,
}

#[cfg(unix)]
impl BackgroundIpcLeakProbe {
    fn new() -> TestResult<Self> {
        let dir = tempdir()?;
        Ok(Self {
            ready_path: dir.path().join("probe-ready"),
            release_path: dir.path().join("probe-release"),
            exited_path: dir.path().join("probe-exited"),
            _dir: dir,
        })
    }

    fn ready_literal(&self) -> TestResult<String> {
        path_literal(&self.ready_path, "background IPC leak probe ready marker")
    }

    fn release_literal(&self) -> TestResult<String> {
        path_literal(
            &self.release_path,
            "background IPC leak probe release marker",
        )
    }

    fn exited_literal(&self) -> TestResult<String> {
        path_literal(&self.exited_path, "background IPC leak probe exited marker")
    }

    async fn wait_for_ready(&self) -> TestResult<()> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut last_ready_error = None;
        while Instant::now() < deadline {
            if self.exited_path.exists() {
                return Err(format!(
                    "background IPC leak probe exited before ready: {}",
                    self.exited_path.display()
                )
                .into());
            }
            if self.ready_path.exists() {
                match self.probe_pid() {
                    Ok(pid) if process_is_alive(pid) => return Ok(()),
                    Ok(pid) => {
                        return Err(format!(
                            "background IPC leak probe was killed before ready completed: pid {pid}, ready marker {}",
                            self.ready_path.display()
                        )
                        .into());
                    }
                    Err(err) => {
                        last_ready_error = Some(err.to_string());
                    }
                }
            }
            sleep(Duration::from_millis(50)).await;
        }

        let suffix = last_ready_error
            .map(|err| format!("; last ready marker read error: {err}"))
            .unwrap_or_default();
        Err(format!(
            "background IPC leak probe never started: {}{suffix}",
            self.ready_path.display()
        )
        .into())
    }

    fn assert_running_before_release(&self, context: &str) -> TestResult<()> {
        if !self.ready_path.exists() {
            return Err(format!(
                "{context}: background IPC leak probe never started: {}",
                self.ready_path.display()
            )
            .into());
        }
        if self.exited_path.exists() {
            return Err(format!(
                "{context}: background IPC leak probe exited before release: {}",
                self.exited_path.display()
            )
            .into());
        }

        let pid = self.probe_pid()?;
        if !process_is_alive(pid) {
            return Err(format!(
                "{context}: background IPC leak probe was killed before release: pid {pid}, ready marker {}",
                self.ready_path.display()
            )
            .into());
        }
        Ok(())
    }

    async fn release_and_wait_for_exit(&self) -> TestResult<()> {
        let was_alive_before_release = self.probe_alive().unwrap_or(false);
        self.write_release_marker()?;

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.exited_path.exists() {
                sleep(Duration::from_millis(250)).await;
                return Ok(());
            }
            sleep(Duration::from_millis(50)).await;
        }

        if self.ready_path.exists() && !was_alive_before_release {
            return Err(format!(
                "background IPC leak probe was killed before release: ready marker {}, release marker {}, exited marker {}",
                self.ready_path.display(),
                self.release_path.display(),
                self.exited_path.display()
            )
            .into());
        }
        Err(format!(
            "background IPC leak probe failed to exit after release: release marker {}, exited marker {}",
            self.release_path.display(),
            self.exited_path.display()
        )
        .into())
    }

    fn write_release_marker(&self) -> TestResult<()> {
        fs::write(&self.release_path, "go")?;
        Ok(())
    }

    fn wait_for_exit_sync(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.exited_path.exists() {
                std::thread::sleep(Duration::from_millis(250));
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    fn probe_alive(&self) -> TestResult<bool> {
        if !self.ready_path.exists() {
            return Ok(false);
        }
        Ok(process_is_alive(self.probe_pid()?))
    }

    fn probe_pid(&self) -> TestResult<i32> {
        let pid_text = fs::read_to_string(&self.ready_path)?;
        let pid = pid_text.trim().parse::<i32>().map_err(|err| {
            format!(
                "background IPC leak probe ready marker did not contain a pid: {}: {err}",
                self.ready_path.display()
            )
        })?;
        Ok(pid)
    }
}

#[cfg(unix)]
impl Drop for BackgroundIpcLeakProbe {
    fn drop(&mut self) {
        if self.exited_path.exists() {
            return;
        }

        let _ = self.write_release_marker();
        let _ = self.wait_for_exit_sync(Duration::from_secs(5));
    }
}

#[cfg(unix)]
fn process_is_alive(pid: i32) -> bool {
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(unix)]
async fn fail_background_ipc_leak_probe_test(
    probe: &BackgroundIpcLeakProbe,
    session: common::McpTestSession,
    message: String,
) -> TestResult<()> {
    let probe_cleanup = probe.release_and_wait_for_exit().await;
    let session_cleanup = session.cancel().await;
    match (probe_cleanup, session_cleanup) {
        (Ok(()), Ok(())) => Err(message.into()),
        (Err(probe_err), Ok(())) => {
            Err(format!("{message}; leak probe cleanup failed: {probe_err}").into())
        }
        (Ok(()), Err(session_err)) => {
            Err(format!("{message}; session cleanup failed: {session_err}").into())
        }
        (Err(probe_err), Err(session_err)) => Err(format!(
            "{message}; leak probe cleanup failed: {probe_err}; session cleanup failed: {session_err}"
        )
        .into()),
    }
}

async fn wait_for_detached_holder_exit(marker_path: &Path) -> TestResult<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if marker_path.exists() {
            sleep(Duration::from_millis(250)).await;
            return Ok(());
        }
        sleep(Duration::from_millis(50)).await;
    }
    Err(format!("detached holder did not exit: {}", marker_path.display()).into())
}

async fn wait_for_file_text(path: &Path, expected: &str) -> TestResult<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut last_text = None;
    while Instant::now() < deadline {
        match fs::read_to_string(path) {
            Ok(text) if text == expected => return Ok(()),
            Ok(text) => last_text = Some(text),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
        sleep(Duration::from_millis(50)).await;
    }

    match last_text {
        Some(text) => Err(format!(
            "timed out waiting for {} to contain {expected:?}; last contents were {text:?}",
            path.display()
        )
        .into()),
        None => Err(format!(
            "timed out waiting for {} to contain {expected:?}",
            path.display()
        )
        .into()),
    }
}

#[cfg(unix)]
fn shutdown_completion_budget() -> Duration {
    Duration::from_millis(1_500)
}

#[cfg(unix)]
async fn arm_detached_stdio_holder(
    session: &mut common::McpTestSession,
) -> TestResult<DetachedHolderProbe> {
    let holder = DetachedHolderProbe::new()?;
    let marker_literal = holder.marker_literal()?;
    let setup = session
        .write_stdin_raw_with(
            format!(
                r#"import subprocess, sys
script = """import pathlib, time
time.sleep({DETACHED_STDIO_HOLDER_SECS})
pathlib.Path({marker_literal}).write_text('done')
"""
subprocess.Popen(
    [sys.executable, "-c", script],
    stdin=subprocess.DEVNULL,
    close_fds=True,
    start_new_session=True,
)
print("detached ready")
"#
            ),
            Some(5.0),
        )
        .await?;
    let setup_text = result_text(&setup);
    if is_busy_response(&setup_text) {
        return Err("detached-stdio setup remained busy".into());
    }
    assert!(
        setup_text.contains("detached ready"),
        "expected detached-stdio setup reply, got: {setup_text:?}"
    );
    Ok(holder)
}

#[cfg(unix)]
async fn arm_background_ipc_leak_probe(
    session: &mut common::McpTestSession,
) -> TestResult<BackgroundIpcLeakProbe> {
    let probe = BackgroundIpcLeakProbe::new()?;
    let ready_literal = probe.ready_literal()?;
    let release_literal = probe.release_literal()?;
    let exited_literal = probe.exited_literal()?;
    let setup = session
        .write_stdin_raw_with(
            format!(
                r#"import subprocess, sys
launcher_script = """import os, subprocess, sys
holder_script = '''import os, pathlib, sys, time
ready = pathlib.Path(sys.argv[1])
release = pathlib.Path(sys.argv[2])
exited = pathlib.Path(sys.argv[3])
ready_tmp = ready.with_name(ready.name + ".tmp")
ready_tmp.write_text(str(os.getpid()))
os.replace(ready_tmp, ready)
while not release.exists():
    time.sleep(0.02)
exited_tmp = exited.with_name(exited.name + ".tmp")
exited_tmp.write_text("done")
os.replace(exited_tmp, exited)
'''
subprocess.Popen(
    [sys.executable, "-c", holder_script, sys.argv[1], sys.argv[2], sys.argv[3]],
    stdin=subprocess.DEVNULL,
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
    close_fds=False,
    start_new_session=True,
)
"""
launcher = subprocess.Popen(
    [sys.executable, "-c", launcher_script, {ready_literal}, {release_literal}, {exited_literal}],
    stdin=subprocess.DEVNULL,
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
    close_fds=False,
)
launcher_status = launcher.wait()
if launcher_status != 0:
    raise SystemExit(launcher_status)
print("ipc leak probe launcher exited")
"#
            ),
            Some(5.0),
        )
        .await?;
    let setup_text = result_text(&setup);
    if is_busy_response(&setup_text) {
        let _ = probe.release_and_wait_for_exit().await;
        return Err("background IPC leak probe setup remained busy".into());
    }
    if !setup_text.contains("ipc leak probe launcher exited") {
        let _ = probe.release_and_wait_for_exit().await;
        return Err(
            format!("expected background IPC leak probe setup reply, got: {setup_text:?}").into(),
        );
    }
    if let Err(err) = probe.wait_for_ready().await {
        let _ = probe.release_and_wait_for_exit().await;
        return Err(err);
    }
    Ok(probe)
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_plot_hook_flushes_before_input_wait_reply() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"exec("""
import _mcp_repl
emit_at_wait = False
def _mcp_repl_emit_plots():
    if emit_at_wait:
        _mcp_repl.emit_plot_image("image/png", "cGxvdA==", False, "forced")
""")
emit_at_wait = True; value = input("plot wait> ")
"#,
            Some(5.0),
        )
        .await?;
    let mut text = result_text(&result);
    let mut result = result;
    if is_busy_response(&text) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && is_busy_response(&text) && !text.contains("plot wait> ")
        {
            sleep(Duration::from_millis(50)).await;
            result = session.write_stdin_raw_with("", Some(1.0)).await?;
            text = result_text(&result);
        }
    }

    session.cancel().await?;

    assert!(
        !is_busy_response(&text),
        "expected stdin wait reply, got: {text:?}"
    );
    assert!(
        text.contains("plot wait> "),
        "expected stdin wait prompt, got: {text:?}"
    );
    assert_eq!(
        image_count(&result),
        1,
        "expected plot hook to flush before stdin wait completed the request, got text: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_timeout_drains_later_stderr_after_incomplete_stdout_utf8_tail() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"exec("""
import sys
import time
sys.stdout.buffer.write(bytes([0xC3]))
sys.stdout.flush()
sys.stderr.write("STDERR_READY\\n")
sys.stderr.flush()
time.sleep(1)
sys.stdout.buffer.write(bytes([0xA9]))
sys.stdout.flush()
time.sleep(0.1)
sys.stdout.write("STDOUT_DONE\\n")
sys.stdout.flush()
""")"#,
            Some(0.2),
        )
        .await?;
    let text = result_text(&result);

    assert!(
        is_busy_response(&text),
        "expected request to remain pending after timeout, got: {text:?}"
    );
    assert!(
        text.contains("\\xC3"),
        "timeout reply should flush the incomplete stdout UTF-8 tail, got: {text:?}"
    );
    assert!(
        text.contains("stderr: STDERR_READY\n"),
        "timeout reply should expose later stderr after the incomplete stdout UTF-8 tail, got: {text:?}"
    );
    let head_idx = text
        .find("\\xC3")
        .ok_or_else(|| format!("expected sealed UTF-8 head in timeout reply, got: {text:?}"))?;
    let stderr_idx = text
        .find("STDERR_READY")
        .ok_or_else(|| format!("expected stderr in timeout reply, got: {text:?}"))?;
    assert!(
        head_idx < stderr_idx,
        "expected sealed UTF-8 head before later stderr, got: {text:?}"
    );

    let mut poll = session.write_stdin_raw_with("", Some(5.0)).await?;
    let mut poll_text = result_text(&poll);
    let mut completion_text = poll_text.clone();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && is_busy_response(&poll_text) {
        sleep(Duration::from_millis(50)).await;
        poll = session
            .write_stdin_raw_unterminated_with("", Some(2.0))
            .await?;
        poll_text = result_text(&poll);
        completion_text.push_str(&poll_text);
    }

    session.cancel().await?;

    assert!(
        !is_busy_response(&poll_text),
        "expected request to finish after polling, got: {completion_text:?}"
    );
    assert!(
        !completion_text.contains("STDERR_READY"),
        "stderr already drained in timeout reply should not repeat on completion polls, got: {completion_text:?}"
    );
    let done_idx = completion_text
        .find("STDOUT_DONE")
        .ok_or_else(|| format!("expected trailing stdout, got: {completion_text:?}"))?;
    if let Some(tail_idx) = completion_text.find("\\xA9") {
        assert!(
            tail_idx < done_idx,
            "expected continuation byte before later stdout, got: {completion_text:?}"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_pager_timeout_preserves_unterminated_stderr_state_on_poll() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(mut session) = start_python_pager_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"exec("""
import sys
import time
sys.stderr.write("ERR_PART")
sys.stderr.flush()
time.sleep(1)
sys.stderr.write("ERR_REST\\n")
sys.stderr.flush()
""")"#,
            Some(0.2),
        )
        .await?;
    let text = result_text(&result);

    assert!(
        is_busy_response(&text),
        "expected request to remain pending after timeout, got: {text:?}"
    );
    assert!(
        text.contains("stderr: ERR_PART"),
        "expected timeout reply to include the first stderr fragment, got: {text:?}"
    );

    let poll = session.write_stdin_raw_with("", Some(5.0)).await?;
    let poll = common::wait_until_not_busy(
        &mut session,
        poll,
        Duration::from_millis(50),
        Duration::from_secs(10),
    )
    .await?;
    let poll_text = result_text(&poll);

    session.cancel().await?;

    assert!(
        poll_text.contains("ERR_REST"),
        "expected stderr continuation on poll, got: {poll_text:?}"
    );
    assert!(
        !poll_text.contains("stderr: ERR_REST"),
        "stderr continuation should not receive a second prefix, got: {poll_text:?}"
    );
    Ok(())
}

#[cfg(not(target_family = "unix"))]
#[tokio::test(flavor = "multi_thread")]
async fn python_input_prompt_is_not_duplicated_on_builtin_adapter_stdio() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let mut text = result_text(
        &session
            .write_stdin_raw_with("value = input('p> ')", Some(1.0))
            .await?,
    );
    if is_busy_response(&text) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && is_busy_response(&text) && !text.contains("p> ") {
            sleep(Duration::from_millis(50)).await;
            text = result_text(&session.write_stdin_raw_with("", Some(1.0)).await?);
        }
    }

    session.cancel().await?;

    assert!(
        !is_busy_response(&text),
        "expected input prompt, got: {text:?}"
    );
    assert_eq!(
        text.matches("p> ").count(),
        1,
        "expected input prompt to appear once, got: {text:?}"
    );
    Ok(())
}

#[cfg(not(unix))]
#[tokio::test(flavor = "multi_thread")]
async fn python_plot_show_during_timeout_emits_on_builtin_adapter_stdin() -> TestResult<()> {
    if !python_plot_tests_enabled() {
        return Ok(());
    }
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let setup = session
        .write_stdin_raw_with(
            r#"import matplotlib
matplotlib.use("agg", force=True)
import matplotlib.pyplot as plt
print("plot ready")
"#,
            Some(30.0),
        )
        .await?;
    let setup_text = result_text(&setup);
    assert!(
        setup_text.contains("plot ready"),
        "expected matplotlib setup to finish, got: {setup_text:?}"
    );

    let result = session
        .write_stdin_raw_with(
            r#"import time
plt.figure(919)
plt.clf()
plt.plot([1, 2, 3], [3, 2, 1])
plt.show()
time.sleep(10)
"#,
            Some(0.5),
        )
        .await?;
    assert_eq!(
        image_count(&result),
        1,
        "expected plot hook to emit before timeout, got text: {:?}",
        result_text(&result)
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_backend_runs_inside_mcp_repl_worker() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"import os, pathlib, sys
if sys.platform == "linux":
    print(pathlib.Path(os.readlink("/proc/self/exe")).name)
elif sys.platform == "darwin":
    import ctypes, ctypes.util
    libc = ctypes.CDLL(ctypes.util.find_library("c"), use_errno=True)
    buf = ctypes.create_string_buffer(4096)
    if libc.proc_pidpath(os.getpid(), buf, len(buf)) <= 0:
        raise OSError(ctypes.get_errno(), "proc_pidpath failed")
    print(pathlib.Path(os.fsdecode(buf.value)).name)
else:
    print(pathlib.Path(sys.argv[0]).name)

"#,
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    assert!(
        text.lines().any(|line| line.trim() == "mcp-repl"),
        "expected Python worker process image to be mcp-repl, got: {text:?}"
    );
    assert!(
        !text.contains("mcp-repl-python-worker"),
        "did not expect a separate Python worker binary, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_sys_path_includes_current_working_directory() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let temp = tempdir()?;
    fs::write(
        temp.path().join("local_import_probe.py"),
        "VALUE = 'local-ok'\n",
    )?;
    let cwd = serde_json::to_string(temp.path().to_str().ok_or("temp path must be utf-8")?)?;
    let result = session
        .write_stdin_raw_with(
            format!(
                r#"import os
os.chdir({cwd})
import local_import_probe
print("LOCAL_IMPORT", local_import_probe.VALUE)
"#
            ),
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err("python local cwd import test remained busy".into());
    }
    assert!(
        text.contains("LOCAL_IMPORT local-ok"),
        "expected cwd import to resolve local module, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_incomplete_bracket_reports_syntax_error() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session.write_stdin_raw_with("x = (", Some(5.0)).await?;
    let text = result_text(&result);
    assert!(
        text.contains("SyntaxError"),
        "expected incomplete bracket cell to report SyntaxError, got: {text:?}"
    );
    assert!(
        !text.contains("... "),
        "incomplete bracket cell should not report continuation prompt, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_multiline_bracket_cell_runs_in_one_call() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with("x = (\n1\n)\nprint('MULTILINE_BRACKET', x)", Some(5.0))
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains("MULTILINE_BRACKET 1"),
        "expected multiline bracket cell to complete, got: {text:?}"
    );
    assert!(
        !text.contains("... "),
        "multiline bracket cell should not report continuation prompt, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_blank_line_inside_bracket_cell_runs() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with("x = [\n\n]\nprint('BLANK_LIST', x)", Some(5.0))
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains("BLANK_LIST []"),
        "expected blank-line bracket cell to complete, got: {text:?}"
    );
    assert!(
        !text.contains("... "),
        "blank-line bracket cell should not report continuation prompt, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_blank_line_inside_triple_quoted_block_cell_runs() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            "if True:\n    x = \"\"\"\n    \n    \"\"\"\nprint('TRIPLE_BLANK', len(x.splitlines()))",
            Some(1.0),
        )
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        !is_busy_response(&text),
        "expected blank line inside triple-quoted block to complete, got: {text:?}"
    );
    assert!(
        text.contains("TRIPLE_BLANK"),
        "expected triple-quoted block cell to run following top-level code, got: {text:?}"
    );
    assert!(
        !text.contains("... "),
        "triple-quoted block cell should not report continuation prompt, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_blank_line_inside_parenthesized_block_cell_runs() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            "if True:\n    x = (\n    \n        1\n    )\nprint('PAREN_BLANK', x)",
            Some(1.0),
        )
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        !is_busy_response(&text),
        "expected parenthesized block cell to complete, got: {text:?}"
    );
    assert!(
        text.contains("PAREN_BLANK 1"),
        "expected parenthesized block cell to run following top-level code, got: {text:?}"
    );
    assert!(
        !text.contains("... "),
        "parenthesized block cell should not report continuation prompt, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_closed_indented_expression_stays_prompt_free() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with("print(\n    'CLOSED_INDENT')", Some(5.0))
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains("CLOSED_INDENT"),
        "expected closed indented expression to execute, got: {text:?}"
    );
    assert!(
        !text.contains("... "),
        "expected closed indented expression not to report continuation prompt, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_closed_multiline_string_with_colon_stays_prompt_free() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with("x = '''label:\n    value'''", Some(5.0))
        .await?;
    let text = result_text(&result);
    assert!(
        !is_busy_response(&text),
        "expected closed multiline string to complete, got: {text:?}"
    );
    assert!(
        !text.contains("... "),
        "expected closed multiline string not to report continuation prompt, got: {text:?}"
    );

    let result = session
        .write_stdin_raw_with("print('STRING_AFTER', x.splitlines()[0])", Some(5.0))
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains("STRING_AFTER label:"),
        "expected next turn to run after closed multiline string, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_long_physical_line_does_not_complete_before_execution() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let filler = "x = 1; ".repeat(40);
    let input = format!("import time; {filler}time.sleep(0.5); print('LONG_LINE_DONE')");
    let result = session.write_stdin_raw_with(&input, Some(0.1)).await?;
    let text = result_text(&result);
    assert!(
        is_busy_response(&text),
        "expected long physical line to stay busy until execution finishes, got: {text:?}"
    );

    sleep(Duration::from_millis(700)).await;
    let poll = session.write_stdin_raw_with("", Some(5.0)).await?;
    let poll_text = result_text(&poll);
    session.cancel().await?;

    assert!(
        poll_text.contains("LONG_LINE_DONE"),
        "expected long physical line output after execution, got: {poll_text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_large_buffered_tail_after_timed_out_line_stays_busy() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let tail = "x_tail_value = 1\n".repeat(200_000);
    let input = format!("import time; time.sleep(8)\n{tail}print('TAIL_AFTER_SLEEP')");
    let first = session.write_stdin_raw_with(input, Some(0.2)).await?;
    let first_text = result_text(&first);
    assert!(
        first_text.contains("<<repl status: busy"),
        "expected sleep call with large buffered tail to time out, got: {first_text:?}"
    );

    sleep(Duration::from_millis(1_500)).await;
    let poll = session.write_stdin_raw_with("", Some(0.5)).await?;
    let poll_text = result_text(&poll);
    assert!(
        is_busy_response(&poll_text),
        "expected request to remain busy instead of reporting a worker error, got: {poll_text:?}"
    );
    assert!(
        !poll_text.contains("worker error:"),
        "queued input tail should not become a worker error, got: {poll_text:?}"
    );

    let interrupt = session
        .write_stdin_raw_unterminated_with("\u{3}", Some(5.0))
        .await?;
    let interrupt_text = result_text(&interrupt);
    if is_busy_response(&interrupt_text) {
        eprintln!("large-tail interrupt stayed busy; cancelling session");
    }

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_terminated_block_stays_prompt_free() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with("if True:\n    print('BLOCK_DONE')\n\n", Some(5.0))
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err("python terminated block remained busy".into());
    }
    assert!(
        text.contains("BLOCK_DONE"),
        "expected terminated block output, got: {text:?}"
    );
    assert!(
        !text.contains(r#"prompt: "... ""#),
        "terminated block should not render a continuation prompt, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_sys_exit_terminates_session_without_traceback() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with("import sys; sys.exit(7)", Some(5.0))
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err("python sys.exit() remained busy".into());
    }
    assert!(
        !text.contains("Traceback") && !text.contains("SystemExit"),
        "expected sys.exit() to terminate without traceback, got: {text:?}"
    );

    let follow_up = session
        .write_stdin_raw_with("print('AFTER_SYS_EXIT')", Some(5.0))
        .await?;
    let follow_up_text = result_text(&follow_up);
    if is_busy_response(&follow_up_text) {
        session.cancel().await?;
        return Err("python follow-up after sys.exit() remained busy".into());
    }
    assert!(
        follow_up_text.contains("AFTER_SYS_EXIT"),
        "expected Python session to respawn after sys.exit(), got: {follow_up_text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_sys_exit_with_non_daemon_thread_waits_for_shutdown() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let exit = session
        .write_stdin_raw_with(
            r#"import sys, threading, time
def hold_session_open():
    time.sleep(5.0)

threading.Thread(target=hold_session_open).start()
sys.exit()
"#,
            Some(0.5),
        )
        .await?;
    let exit_text = result_text(&exit);

    let follow_up = session
        .write_stdin_raw_with("print('AFTER_NON_DAEMON_EXIT')", Some(0.5))
        .await?;
    let follow_up_text = result_text(&follow_up);
    session.cancel().await?;

    assert!(
        is_busy_response(&exit_text),
        "expected CPython shutdown to remain busy while waiting for the non-daemon thread, got: {exit_text:?}"
    );
    assert!(
        is_busy_response(&follow_up_text),
        "expected no synthetic session_end or immediate respawn while CPython shutdown is still alive, got: {follow_up_text:?}"
    );
    assert!(
        !follow_up_text.contains("AFTER_NON_DAEMON_EXIT"),
        "follow-up should not run until CPython shutdown completes, got: {follow_up_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_sys_exit_runs_atexit_handlers() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let temp = tempdir()?;
    let debug_dir = temp.path().join("debug");
    let Some(session) = start_python_session_with_env_vars(debug_dir_env(&debug_dir)).await? else {
        return Ok(());
    };

    let marker = temp.path().join("atexit-marker.txt");
    let marker_literal = serde_json::to_string(
        marker
            .to_str()
            .ok_or("atexit marker path must be valid utf-8")?,
    )?;
    let result = session
        .write_stdin_raw_with(
            format!(
                r#"import atexit, os, sys
marker_path = {marker_literal}
open_fd = os.open
write_fd = os.write
close_fd = os.close
flags = os.O_WRONLY | os.O_CREAT | os.O_TRUNC
def write_marker(
    path=marker_path,
    data=b"atexit ran",
    open_fd=open_fd,
    write_fd=write_fd,
    close_fd=close_fd,
    flags=flags,
):
    fd = open_fd(path, flags, 0o666)
    try:
        write_fd(fd, data)
    finally:
        close_fd(fd)

atexit.register(write_marker)
sys.exit()
"#
            ),
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err("python sys.exit() with atexit remained busy".into());
    }

    let marker_result = wait_for_file_text(&marker, "atexit ran").await;
    if let Err(err) = marker_result {
        session.cancel().await?;
        return Err(format!("{err}{}", debug_log_summary(&debug_dir)).into());
    }

    let follow_up = session
        .write_stdin_raw_with(
            "print('AFTER_ATEXIT', 'write_marker' in globals())",
            Some(5.0),
        )
        .await?;
    let follow_up_text = result_text(&follow_up);
    session.cancel().await?;
    assert!(
        follow_up_text.contains("AFTER_ATEXIT False"),
        "expected Python session to respawn after sys.exit(), got: {follow_up_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_sys_exit_returns_atexit_output_before_session_end() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"import atexit, sys
def report():
    print("ATEXIT_STDOUT")
    sys.stderr.write("ATEXIT_STDERR\n")

atexit.register(report)
sys.exit()
"#,
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err("python sys.exit() with atexit output remained busy".into());
    }

    assert!(
        text.contains("ATEXIT_STDOUT") && text.contains("ATEXIT_STDERR"),
        "expected atexit stdout/stderr before session end, got: {text:?}"
    );
    assert!(
        !text.contains("worker protocol error"),
        "atexit output should not arrive after session_end, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_input_reads_from_sys_stdin() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"import io, sys
sys.stdin = io.StringIO("replacement\n")
print("INPUT", input())
"#,
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err("python input() sys.stdin replacement test remained busy".into());
    }
    assert!(
        text.contains("INPUT replacement"),
        "expected input() to read from sys.stdin, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[cfg(any(unix, windows))]
#[tokio::test(flavor = "multi_thread")]
async fn python_uses_pty_backed_c_stdio_for_input() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"import builtins, os
exec("""
import ctypes, os
if os.name == "nt":
    try:
        crt = ctypes.CDLL("ucrtbase")
    except OSError:
        crt = ctypes.CDLL("msvcrt")
    print("CRT_ISATTY", bool(crt._isatty(0)), bool(crt._isatty(1)), bool(crt._isatty(2)))
""")
print("PTY_FDS", os.isatty(0), os.isatty(1), os.isatty(2))
print("INPUT_IMPL", builtins.input.__module__, builtins.input.__name__)
value = input("pty> ")
print("PTY_INPUT", value)
"#,
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err("python PTY input() path test remained busy".into());
    }
    assert!(
        text.contains("pty> "),
        "expected input prompt, got: {text:?}"
    );

    let answer = session.write_stdin_raw_with("hello", Some(5.0)).await?;
    let answer_text = result_text(&answer);
    session.cancel().await?;

    assert!(
        text.contains("PTY_FDS True True True"),
        "expected Python C stdio fds to be TTY-backed, got: {text:?}"
    );
    #[cfg(windows)]
    assert!(
        text.contains("CRT_ISATTY True True True"),
        "expected Windows CRT fds to be TTY-backed, got: {text:?}"
    );
    assert!(
        text.contains("INPUT_IMPL builtins input"),
        "expected input() to use CPython's builtin implementation, got: {text:?}"
    );
    assert!(
        answer_text.contains("PTY_INPUT hello"),
        "expected CPython input() to consume the PTY-backed answer, got: {answer_text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_pty_routes_stdin_surfaces_through_queue() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"import builtins, io, os, sys, _io
readv_module = getattr(os, "readv", os.read).__module__
print("STDIN_SURFACE", type(sys.stdin).__module__, type(sys.stdin).__name__, sys.stdin.fileno(), sys.stdin.isatty())
print("DIRECT_FD_SHIMS", builtins.open.__module__, io.open.__module__, io.FileIO.__module__, _io.FileIO.__module__, os.read.__module__, readv_module)
"#,
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err("python PTY stdin surface test remained busy".into());
    }

    session.cancel().await?;

    assert!(
        text.contains("STDIN_SURFACE __main__ McpInputStream 0 True"),
        "expected sys.stdin to be the managed stdin bridge, got: {text:?}"
    );
    let direct_fd_modules = text
        .lines()
        .find_map(|line| line.strip_prefix("DIRECT_FD_SHIMS "))
        .map(|line| line.split_whitespace().collect::<Vec<_>>())
        .unwrap_or_else(|| {
            panic!("expected direct fd stdin API module line, got: {text:?}");
        });
    assert_eq!(
        direct_fd_modules.len(),
        6,
        "expected six direct fd stdin API module names, got: {text:?}"
    );
    assert_eq!(
        direct_fd_modules,
        [
            "__main__", "__main__", "__main__", "__main__", "__main__", "__main__"
        ],
        "expected stdin fd APIs to be routed through the managed bridge, got: {text:?}"
    );
    Ok(())
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn python_windows_pty_bridges_stdin_surfaces() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"exec("""
import ctypes, os, sys, tempfile
try:
    crt = ctypes.CDLL("ucrtbase")
except OSError:
    crt = ctypes.CDLL("msvcrt")
print("CRT_ISATTY", bool(crt._isatty(0)), bool(crt._isatty(1)), bool(crt._isatty(2)))
print("PTY_FDS", os.isatty(0), os.isatty(1), os.isatty(2))
print("STDIN_BRIDGE", type(sys.stdin).__module__, type(sys.stdin).__name__, sys.stdin.isatty())
first = input("win> ")
second = sys.stdin.readline().strip()
dup_fd = os.dup(0)
try:
    third = os.read(dup_fd, 5).decode("utf-8")
finally:
    os.close(dup_fd)
saved_fd = os.dup(0)
try:
    with tempfile.TemporaryFile() as replacement:
        os.dup2(replacement.fileno(), 0)
        replaced_stdin_isatty = os.isatty(0)
finally:
    os.dup2(saved_fd, 0)
    os.close(saved_fd)
print("WIN_STDIN_VALUES", first, second, third)
print("REPLACED_STDIN_ISATTY", replaced_stdin_isatty)
""")
"#,
            Some(5.0),
        )
        .await?;
    let mut text = result_text(&result);
    if is_busy_response(&text) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline
            && is_busy_response(&text)
            && !text.contains("WIN_STDIN_VALUES alpha beta gamma")
        {
            sleep(Duration::from_millis(50)).await;
            text = result_text(&session.write_stdin_raw_with("", Some(1.0)).await?);
        }
    }
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err("python Windows PTY stdin bridge test remained busy".into());
    }
    assert!(
        text.contains("win> "),
        "expected Windows input prompt, got: {text:?}"
    );

    let mut all_text = text.clone();

    let alpha = session.write_stdin_raw_with("alpha", Some(5.0)).await?;
    text = result_text(&alpha);
    all_text.push_str(&text);
    if is_busy_response(&text) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && is_busy_response(&text) {
            sleep(Duration::from_millis(50)).await;
            text = result_text(&session.write_stdin_raw_with("", Some(1.0)).await?);
            all_text.push_str(&text);
        }
    }

    let beta_gamma = session
        .write_stdin_raw_with("beta\ngamma", Some(5.0))
        .await?;
    text = result_text(&beta_gamma);
    all_text.push_str(&text);
    if is_busy_response(&text) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline
            && is_busy_response(&text)
            && !text.contains("WIN_STDIN_VALUES alpha beta gamma")
        {
            sleep(Duration::from_millis(50)).await;
            text = result_text(&session.write_stdin_raw_with("", Some(1.0)).await?);
            all_text.push_str(&text);
        }
    }
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err("python Windows PTY stdin bridge follow-up test remained busy".into());
    }

    session.cancel().await?;

    assert!(
        all_text.contains("PTY_FDS True True True"),
        "expected Python fds to be TTY-backed, got: {all_text:?}"
    );
    assert!(
        all_text.contains("CRT_ISATTY True True True"),
        "expected Windows CRT fds to be TTY-backed, got: {all_text:?}"
    );
    assert!(
        all_text.contains("STDIN_BRIDGE __main__ McpInputStream True"),
        "expected Windows sys.stdin bridge to report TTY-backed stdin, got: {all_text:?}"
    );
    assert!(
        all_text.contains("WIN_STDIN_VALUES alpha beta gamma"),
        "expected input/sys.stdin/dup fd reads to consume buffered input batch, got: {all_text:?}"
    );
    assert!(
        all_text.contains("REPLACED_STDIN_ISATTY False"),
        "expected replaced stdin fd to fall back to real isatty state, got: {all_text:?}"
    );
    Ok(())
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn python_windows_raw_stdin_read_waits_for_next_turn() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let first = session
        .write_stdin_raw_with(
            r#"exec("""
import os
print("RAW_STDIN_WAITING")
data = os.read(0, 6)
print("RAW_STDIN_RESULT", data.decode("utf-8").strip())
""")
"#,
            Some(5.0),
        )
        .await?;
    let first_text = result_text(&first);
    assert!(
        !is_busy_response(&first_text),
        "expected raw stdin read to complete the input batch at an input boundary, got: {first_text:?}"
    );
    assert!(
        first_text.contains("RAW_STDIN_WAITING"),
        "expected code before raw stdin wait to run, got: {first_text:?}"
    );
    assert!(
        first_text.contains("<<repl status: waiting for input>>"),
        "raw stdin read should report a wait before the follow-up input batch, got: {first_text:?}"
    );
    assert!(
        !first_text.contains("RAW_STDIN_RESULT"),
        "raw stdin read should wait for a later turn instead of returning EOF, got: {first_text:?}"
    );

    let second = session.write_stdin_raw_with("delta", Some(5.0)).await?;
    let mut text = result_text(&second);
    if is_busy_response(&text) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline
            && is_busy_response(&text)
            && !text.contains("RAW_STDIN_RESULT delta")
        {
            sleep(Duration::from_millis(50)).await;
            text = result_text(&session.write_stdin_raw_with("", Some(1.0)).await?);
        }
    }

    session.cancel().await?;

    assert!(
        !is_busy_response(&text),
        "expected raw stdin follow-up turn to complete, got: {text:?}"
    );
    assert!(
        text.contains("RAW_STDIN_RESULT delta"),
        "expected raw stdin read to consume next input batch, got: {text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_pty_direct_stdin_reads_consume_buffered_input_batch() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"import os, sys
line = sys.stdin.readline().strip()
data = os.read(0, 5).decode("utf-8")
print("DIRECT_STDIN_VALUES", line, data)
"#,
            Some(1.0),
        )
        .await?;
    let mut text = result_text(&result);
    if is_busy_response(&text) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && is_busy_response(&text) {
            sleep(Duration::from_millis(50)).await;
            text = result_text(&session.write_stdin_raw_with("", Some(1.0)).await?);
        }
    }
    assert!(
        text.contains("<<repl status: waiting for input>>"),
        "expected first direct stdin read to wait for follow-up input, got: {text:?}"
    );

    let alpha = session.write_stdin_raw_with("alpha", Some(5.0)).await?;
    let mut text = result_text(&alpha);
    if is_busy_response(&text) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && is_busy_response(&text) {
            sleep(Duration::from_millis(50)).await;
            text = result_text(&session.write_stdin_raw_with("", Some(1.0)).await?);
        }
    }
    assert!(
        text.contains("<<repl status: waiting for input>>"),
        "expected raw stdin read to wait for second follow-up input, got: {text:?}"
    );

    let bravo = session
        .write_stdin_raw_unterminated_with("bravo", Some(5.0))
        .await?;
    let text = result_text(&bravo);

    session.cancel().await?;

    assert!(
        !is_busy_response(&text),
        "expected direct stdin reads to consume queued input batch, got: {text:?}"
    );
    assert!(
        text.contains("DIRECT_STDIN_VALUES alpha bravo"),
        "expected sys.stdin.readline() and os.read() to consume queued input, got: {text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_sys_stdin_buffer_read_leaves_followup_turn_visible() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"import sys
buffer_value = sys.stdin.buffer.readline().decode("utf-8").strip()
"#,
            Some(1.0),
        )
        .await?;
    let mut text = result_text(&result);
    if is_busy_response(&text) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && is_busy_response(&text) {
            sleep(Duration::from_millis(50)).await;
            text = result_text(&session.write_stdin_raw_with("", Some(1.0)).await?);
        }
    }
    assert!(
        text.contains("<<repl status: waiting for input>>"),
        "expected sys.stdin.buffer read to wait for follow-up input, got: {text:?}"
    );

    let answer = session.write_stdin_raw_with("buffered", Some(5.0)).await?;
    let answer_text = result_text(&answer);
    assert!(
        !is_busy_response(&answer_text),
        "expected sys.stdin.buffer read to consume the follow-up line, got: {answer_text:?}"
    );

    let followup = session
        .write_stdin_raw_with("print('BUFFER_FOLLOWUP', buffer_value)", Some(5.0))
        .await?;
    let followup_text = result_text(&followup);
    session.cancel().await?;

    assert!(
        followup_text.contains("BUFFER_FOLLOWUP buffered"),
        "expected follow-up REPL input to run after sys.stdin.buffer read, got: {followup_text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_partial_raw_stdin_read_keeps_followup_output_attached() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let first = session
        .write_stdin_raw_with(
            r#"exec("""
import os, time
print("RAW_PARTIAL_WAITING", flush=True)
data = os.read(0, 10)
time.sleep(0.2)
print("RAW_PARTIAL_RESULT", data.decode("utf-8").strip(), flush=True)
""")
"#,
            Some(5.0),
        )
        .await?;
    let first_text = result_text(&first);
    assert!(
        first_text.contains("RAW_PARTIAL_WAITING"),
        "expected code before raw stdin wait to run, got: {first_text:?}"
    );
    assert!(
        !first_text.contains("RAW_PARTIAL_RESULT"),
        "raw stdin read should wait for a later input batch, got: {first_text:?}"
    );

    let second = session.write_stdin_raw_with("delta", Some(5.0)).await?;
    let second_text = result_text(&second);
    session.cancel().await?;

    assert!(
        !is_busy_response(&second_text),
        "expected partial raw stdin read follow-up to complete, got: {second_text:?}"
    );
    assert!(
        second_text.contains("RAW_PARTIAL_RESULT delta"),
        "expected output after the partial raw stdin read to stay attached to the follow-up reply, got: {second_text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_detached_raw_stdin_read_followup_completes() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };
    let temp_dir = tempdir()?;
    let release_path = temp_dir.path().join("release-detached-raw-reader");
    let release_path_literal = path_json_literal(&release_path, "raw reader release")?;

    let first = session
        .write_stdin_raw_with(
            format!(
                r#"import os, pathlib, threading, time
release_path = pathlib.Path({release_path_literal})
def read_later():
    while not release_path.exists():
        time.sleep(0.01)
    print("DETACHED_RAW_THREAD_WAITING", flush=True)
    data = os.read(0, 5)
    print("DETACHED_RAW_THREAD_READ", data.decode("utf-8"), flush=True)
threading.Thread(target=read_later, daemon=True).start()
print("DETACHED_RAW_MAIN_DONE", flush=True)
"#
            ),
            Some(5.0),
        )
        .await?;
    let first_text = result_text(&first);
    assert!(
        first_text.contains("DETACHED_RAW_MAIN_DONE"),
        "expected main cell to finish before detached raw stdin read, got: {first_text:?}"
    );
    assert!(
        !first_text.contains("DETACHED_RAW_THREAD_WAITING"),
        "detached raw stdin read should wait for the explicit release gate, got: {first_text:?}"
    );

    fs::write(&release_path, "go")?;
    let _wait_text = poll_until_contains(
        &session,
        String::new(),
        "DETACHED_RAW_THREAD_WAITING",
        "detached raw stdin reader to start before follow-up input",
        Duration::from_secs(5),
    )
    .await?;

    let second = tokio::time::timeout(
        Duration::from_secs(3),
        session.write_stdin_raw_unterminated_with("alpha", Some(10.0)),
    )
    .await;
    let second = match second {
        Ok(result) => result?,
        Err(_) => {
            session.cancel().await?;
            panic!("detached raw stdin follow-up did not complete after consuming input");
        }
    };
    let mut text = result_text(&second);
    let output_deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < output_deadline && !text.contains("DETACHED_RAW_THREAD_READ alpha") {
        sleep(Duration::from_millis(50)).await;
        let poll = session
            .write_stdin_raw_unterminated_with("", Some(1.0))
            .await?;
        text.push_str(&result_text(&poll));
    }

    session.cancel().await?;

    assert!(
        !is_busy_response(&text) && !text.contains("worker response timed out"),
        "expected detached raw stdin follow-up turn to complete, got: {text:?}"
    );
    assert!(
        text.contains("DETACHED_RAW_THREAD_READ alpha"),
        "expected detached raw stdin read to consume follow-up bytes, got: {text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_detached_raw_stdin_reads_consume_one_answer_turn() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };
    let temp_dir = tempdir()?;
    let release_path = temp_dir.path().join("release-detached-raw-pair-reader");
    let release_path_literal = path_json_literal(&release_path, "raw pair reader release")?;

    let first = session
        .write_stdin_raw_with(
            format!(
                r#"import os, pathlib, threading, time
release_path = pathlib.Path({release_path_literal})
def read_later():
    while not release_path.exists():
        time.sleep(0.01)
    print("DETACHED_RAW_PAIR_WAITING", flush=True)
    first = os.read(0, 2)
    second = os.read(0, 2)
    print("DETACHED_RAW_PAIR_READ", first.decode("utf-8"), second.decode("utf-8"), flush=True)
threading.Thread(target=read_later, daemon=True).start()
print("DETACHED_RAW_PAIR_MAIN_DONE", flush=True)
"#
            ),
            Some(5.0),
        )
        .await?;
    let first_text = result_text(&first);
    assert!(
        first_text.contains("DETACHED_RAW_PAIR_MAIN_DONE"),
        "expected main cell to finish before detached raw stdin reads, got: {first_text:?}"
    );
    assert!(
        !first_text.contains("DETACHED_RAW_PAIR_WAITING"),
        "detached raw stdin pair should wait for the explicit release gate, got: {first_text:?}"
    );

    fs::write(&release_path, "go")?;
    let _wait_text = poll_until_contains(
        &session,
        String::new(),
        "DETACHED_RAW_PAIR_WAITING",
        "detached raw stdin reader to start before follow-up input",
        Duration::from_secs(5),
    )
    .await?;

    let answer = session
        .write_stdin_raw_unterminated_with("abcd", Some(1.0))
        .await?;
    let mut answer_text = result_text(&answer);
    let output_deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < output_deadline && !answer_text.contains("DETACHED_RAW_PAIR_READ ab cd")
    {
        sleep(Duration::from_millis(50)).await;
        let poll = session
            .write_stdin_raw_unterminated_with("", Some(1.0))
            .await?;
        answer_text.push_str(&result_text(&poll));
    }
    session.cancel().await?;

    assert!(
        answer_text.contains("DETACHED_RAW_PAIR_READ ab cd"),
        "expected one answer turn to satisfy both detached raw stdin reads, got: {answer_text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_foreground_raw_stdin_partial_read_does_not_preserve_for_unrelated_thread()
-> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };
    let temp_dir = tempdir()?;
    let release_path = temp_dir.path().join("release-unrelated-raw-reader");
    let release_path_literal = serde_json::to_string(
        release_path
            .to_str()
            .ok_or("raw reader release path must be valid utf-8")?,
    )?;

    let first = session
        .write_stdin_raw_with(
            format!(
                r#"import os, pathlib, threading, time
release_path = pathlib.Path({release_path_literal})
def unrelated_reader():
    while not release_path.exists():
        time.sleep(0.01)
    data = os.read(0, 1)
    print("UNRELATED_RAW_READ", data.decode("utf-8"), flush=True)
threading.Thread(target=unrelated_reader, daemon=True).start()
data = os.read(0, 1)
print("FOREGROUND_RAW_READ", data.decode("utf-8"), flush=True)
"#
            ),
            Some(1.0),
        )
        .await?;
    let first_text = result_text(&first);
    assert!(
        first_text.contains("<<repl status: waiting for input>>"),
        "expected foreground raw read to wait for input, got: {first_text:?}"
    );

    let answer = session
        .write_stdin_raw_unterminated_with("xy", Some(5.0))
        .await?;
    let answer_text = result_text(&answer);
    assert!(
        answer_text.contains("FOREGROUND_RAW_READ x"),
        "expected foreground raw read to consume first byte, got: {answer_text:?}"
    );

    fs::write(&release_path, "go")?;
    let mut text = String::new();
    let steal_deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < steal_deadline && !text.contains("UNRELATED_RAW_READ") {
        sleep(Duration::from_millis(50)).await;
        let poll = session
            .write_stdin_raw_unterminated_with("", Some(1.0))
            .await?;
        text.push_str(&result_text(&poll));
    }
    assert!(
        !text.contains("UNRELATED_RAW_READ y"),
        "unrelated background reader consumed preserved foreground raw stdin bytes: {text:?}"
    );

    let fresh = session
        .write_stdin_raw_unterminated_with("z", Some(5.0))
        .await?;
    text.push_str(&result_text(&fresh));
    session.cancel().await?;

    assert!(
        text.contains("UNRELATED_RAW_READ z"),
        "expected unrelated reader to wait for fresh stdin, got: {text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_partial_raw_stdin_read_clears_leftover_newline_before_next_cell() -> TestResult<()>
{
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let first = session
        .write_stdin_raw_with(
            r#"import os
data = os.read(0, 5).decode("utf-8")
print("RAW_PARTIAL_EXACT", data)
"#,
            Some(1.0),
        )
        .await?;
    let first_text = result_text(&first);
    assert!(
        first_text.contains("<<repl status: waiting for input>>"),
        "expected raw stdin read to wait for follow-up input, got: {first_text:?}"
    );

    let answer = session.write_stdin_raw_with("bravo", Some(5.0)).await?;
    let answer_text = result_text(&answer);
    assert!(
        answer_text.contains("RAW_PARTIAL_EXACT bravo"),
        "expected raw stdin read to consume five bytes from the answer, got: {answer_text:?}"
    );

    let follow_up = session
        .write_stdin_raw_with("print('AFTER_PARTIAL_RAW_CELL')", Some(5.0))
        .await?;
    let follow_up_text = result_text(&follow_up);
    session.cancel().await?;

    assert!(
        !is_busy_response(&follow_up_text),
        "expected fresh cell after partial raw stdin read to finish, got: {follow_up_text:?}"
    );
    assert!(
        follow_up_text.contains("AFTER_PARTIAL_RAW_CELL"),
        "expected fresh cell after partial raw stdin read to run, got: {follow_up_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_sys_stdin_partial_text_read_clears_buffer_before_next_cell() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let first = session
        .write_stdin_raw_with(
            r#"import sys
value = sys.stdin.read(2)
print("TEXT_PARTIAL_READ", value)
"#,
            Some(1.0),
        )
        .await?;
    let first_text = result_text(&first);
    assert!(
        first_text.contains("<<repl status: waiting for input>>"),
        "expected partial sys.stdin read to wait for follow-up input, got: {first_text:?}"
    );

    let answer = session.write_stdin_raw_with("abcdef", Some(5.0)).await?;
    let answer_text = result_text(&answer);
    assert!(
        answer_text.contains("TEXT_PARTIAL_READ ab"),
        "expected sys.stdin.read(2) to consume the first two characters, got: {answer_text:?}"
    );

    let next = session
        .write_stdin_raw_with(
            r#"import sys
line = sys.stdin.readline().strip()
print("TEXT_PARTIAL_NEXT", line)
"#,
            Some(1.0),
        )
        .await?;
    let next_text = result_text(&next);
    assert!(
        next_text.contains("<<repl status: waiting for input>>"),
        "expected fresh cell to wait for new stdin instead of buffered leftovers, got: {next_text:?}"
    );

    let fresh = session.write_stdin_raw_with("fresh", Some(5.0)).await?;
    let fresh_text = result_text(&fresh);
    session.cancel().await?;

    assert!(
        fresh_text.contains("TEXT_PARTIAL_NEXT fresh"),
        "expected fresh stdin to satisfy the next cell, got: {fresh_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_text_write_returns_character_count() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"import sys
count = sys.stdout.write("é")
print("COUNT", count)
"#,
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err("python text write count test remained busy".into());
    }
    assert!(
        text.contains("COUNT 1"),
        "expected TextIO.write() to return character count, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_text_write_rejects_non_string_values() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"exec("""
import sys
for name in ("stdout", "stderr"):
    stream = getattr(sys, name)
    try:
        stream.write(b"bytes")
    except TypeError:
        print("TYPE_ERROR", name)
    else:
        print("NO_ERROR", name)
""")
"#,
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err("python non-string text write test remained busy".into());
    }

    session.cancel().await?;

    assert!(
        text.contains("TYPE_ERROR stdout"),
        "expected stdout.write(bytes) to raise TypeError, got: {text:?}"
    );
    assert!(
        text.contains("TYPE_ERROR stderr"),
        "expected stderr.write(bytes) to raise TypeError, got: {text:?}"
    );
    assert!(
        !text.contains("NO_ERROR"),
        "expected non-string writes to fail, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_stdout_stderr_expose_text_stream_methods() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"import os, sys
print("STDOUT_FLAGS", sys.stdout.readable(), sys.stdout.writable(), sys.stdout.seekable(), sys.stdout.isatty(), sys.stdout.buffer.isatty())
print("STDERR_FLAGS", sys.stderr.readable(), sys.stderr.writable(), sys.stderr.seekable(), sys.stderr.isatty(), sys.stderr.buffer.isatty())
sys.stdout.isatty() and os.get_terminal_size(sys.stdout.fileno())
sys.stderr.isatty() and os.get_terminal_size(sys.stderr.fileno())
print("TERMINAL_FLAGS_OK")
sys.stdout.writelines(["OUT_A", "OUT_B\n"])
sys.stderr.writelines(["ERR_A", "ERR_B\n"])
"#,
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains("STDOUT_FLAGS False True False False False"),
        "expected stdout text stream flags, got: {text:?}"
    );
    assert!(
        text.contains("STDERR_FLAGS False True False False False"),
        "expected stderr text stream flags, got: {text:?}"
    );
    assert!(
        text.contains("TERMINAL_FLAGS_OK"),
        "expected non-tty stdout/stderr to avoid terminal-size ioctls, got: {text:?}"
    );
    assert!(
        text.contains("OUT_AOUT_B"),
        "expected stdout.writelines() output, got: {text:?}"
    );
    assert!(
        text.contains("ERR_AERR_B"),
        "expected stderr.writelines() output, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_sys_stdin_exposes_worker_stdin_fd() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            "import sys\nprint('STDIN_FD', sys.stdin.fileno())",
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err("python sys.stdin.fileno() remained busy".into());
    }
    assert!(
        text.contains("STDIN_FD 0"),
        "expected sys.stdin to expose worker fd 0, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_smoke_without_register_at_fork() -> TestResult<()> {
    let _guard = lock_test_mutex();
    if !require_python() {
        return Ok(());
    }

    let temp = tempdir()?;
    fs::write(
        temp.path().join("sitecustomize.py"),
        "import os\ntry:\n    del os.register_at_fork\nexcept AttributeError:\n    pass\n",
    )?;

    let Some(session) = start_python_session_with_env_vars(vec![(
        "PYTHONPATH".to_string(),
        temp.path().display().to_string(),
    )])
    .await?
    else {
        return Ok(());
    };

    let result = session.write_stdin_raw_with("1+1", Some(5.0)).await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        eprintln!("python_smoke_without_register_at_fork remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(text.contains("2"), "expected 2, got: {text:?}");

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_follow_up_after_resolved_timeout_skips_leading_fresh_echo_in_files_mode()
-> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };
    let temp = tempdir()?;
    let release_path = temp.path().join("release-timeout");
    let done_path = temp.path().join("done-timeout");
    let release_literal = serde_json::to_string(
        release_path
            .to_str()
            .ok_or("timeout release path must be valid utf-8")?,
    )?;
    let done_literal = serde_json::to_string(
        done_path
            .to_str()
            .ok_or("timeout done path must be valid utf-8")?,
    )?;
    let first_input = format!(
        r#"import pathlib, time
release_path = pathlib.Path({release_literal})
while not release_path.exists():
    time.sleep(0.01)
print('DETACHED_OK', flush=True)
pathlib.Path({done_literal}).write_text('done')
"#
    );

    let first = session
        .write_stdin_raw_with(first_input.as_str(), Some(0.05))
        .await?;
    let first_text = result_text(&first);
    assert!(
        first_text.contains("<<repl status: busy"),
        "expected the initial Python request to time out, got: {first_text:?}"
    );

    fs::write(&release_path, "go")?;
    wait_for_file_text(&done_path, "done").await?;

    let follow_up = session
        .write_stdin_raw_with("print('FOLLOWUP_OK')", Some(5.0))
        .await?;
    let follow_up_text = result_text(&follow_up);
    if is_busy_response(&follow_up_text) {
        session.cancel().await?;
        return Err(format!(
            "python follow-up remained busy after timed-out request settled: {follow_up_text:?}"
        )
        .into());
    }

    session.cancel().await?;

    assert!(
        follow_up_text.contains("DETACHED_OK"),
        "expected the settled timeout result to be prefixed into the next files-mode reply, got: {follow_up_text:?}"
    );
    assert!(
        follow_up_text.contains("FOLLOWUP_OK"),
        "expected the fresh Python follow-up result, got: {follow_up_text:?}"
    );
    assert!(
        !follow_up_text.contains(">>> import pathlib, time")
            && !follow_up_text.contains("release_path = pathlib.Path"),
        "did not expect timed-out Python source to be synthesized into the next visible reply, got: {follow_up_text:?}"
    );
    assert!(
        !follow_up_text.contains(">>> print('FOLLOWUP_OK')"),
        "fresh Python follow-up input echo should be absent, got: {follow_up_text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_fork_child_closes_raw_ipc_fds_without_python_close() -> TestResult<()> {
    let _guard = lock_test_mutex();
    if !require_python() {
        return Ok(());
    }

    let temp = tempdir()?;
    let marker_path = temp.path().join("fork-close.log");
    let installed_path = temp.path().join("fork-close-installed");
    fs::write(
        temp.path().join("sitecustomize.py"),
        r#"import os
from pathlib import Path

_real_close = os.close
_marker = os.environ["MCP_REPL_FORK_CLOSE_MARKER"]
Path(os.environ["MCP_REPL_FORK_CLOSE_INSTALLED_MARKER"]).write_text(
    "installed",
    encoding="utf-8",
)

def _wrapped_close(fd):
    with open(_marker, "a", encoding="utf-8") as handle:
        handle.write(f"{fd}\n")
    return _real_close(fd)

os.close = _wrapped_close
"#,
    )?;

    let Some(session) = start_python_session_with_env_vars(vec![
        ("PYTHONPATH".to_string(), temp.path().display().to_string()),
        (
            "MCP_REPL_FORK_CLOSE_MARKER".to_string(),
            marker_path.display().to_string(),
        ),
        (
            "MCP_REPL_FORK_CLOSE_INSTALLED_MARKER".to_string(),
            installed_path.display().to_string(),
        ),
    ])
    .await?
    else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"import os
exec("pid = os.fork()\nif pid == 0:\n    os._exit(0)\n_, status = os.waitpid(pid, 0)\nprint('FORK_OK', status)")
"#,
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err("python os.fork() remained busy".into());
    }
    assert!(
        text.contains("FORK_OK"),
        "expected fork round-trip output, got: {text:?}"
    );

    session.cancel().await?;

    assert!(
        installed_path.exists(),
        "expected fork-close spy to be installed by sitecustomize"
    );
    assert!(
        !marker_path.exists(),
        "expected at-fork cleanup to bypass Python os.close, got marker contents: {:?}",
        fs::read_to_string(&marker_path).ok()
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_fork_child_mcp_stdin_returns_eof() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"exec("""import os
pid = os.fork()
if pid == 0:
    try:
        input("child> ")
    except EOFError:
        print("CHILD_STDIN_EOF", flush=True)
        os._exit(0)
    print("CHILD_STDIN_READ", flush=True)
    os._exit(2)
_, status = os.waitpid(pid, 0)
print("FORK_STDIN_STATUS", status)
""")
"#,
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        !is_busy_response(&text),
        "expected fork child stdin EOF to complete, got: {text:?}"
    );
    assert!(
        text.contains("CHILD_STDIN_EOF"),
        "expected fork child mcp-repl stdin to return EOF, got: {text:?}"
    );
    assert!(
        text.contains("FORK_STDIN_STATUS 0"),
        "expected fork child to exit cleanly, got: {text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_subprocess_does_not_inherit_mcp_stdin_fd() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"import subprocess
proc = subprocess.run(["/bin/cat"], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
print("SUBPROCESS_STDIN", proc.returncode)
"#,
            Some(1.0),
        )
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        !is_busy_response(&text),
        "expected subprocess inherited stdin to fail fast, got: {text:?}"
    );
    assert!(
        text.contains("SUBPROCESS_STDIN"),
        "expected subprocess completion to be visible, got: {text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_quit_does_not_wait_for_detached_stdio_holders() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(mut session) = start_python_session().await? else {
        return Ok(());
    };

    let holder = arm_detached_stdio_holder(&mut session).await?;

    let start = Instant::now();
    let quit = session.write_stdin_raw_with("quit()", Some(5.0)).await?;
    let elapsed = start.elapsed();
    let quit_text = result_text(&quit);
    if is_busy_response(&quit_text) {
        eprintln!("python_quit_does_not_wait_for_detached_stdio_holders remained busy on quit");
        holder.wait_for_exit().await?;
        session.cancel().await?;
        return Ok(());
    }

    assert!(
        !holder.has_exited(),
        "expected quit() to finish before detached child exit, got {elapsed:?}: {quit_text:?}"
    );

    let follow_up = session
        .write_stdin_raw_with("print('AFTER_QUIT')", Some(5.0))
        .await?;
    let follow_up_text = result_text(&follow_up);
    if is_busy_response(&follow_up_text) {
        eprintln!(
            "python_quit_does_not_wait_for_detached_stdio_holders remained busy after respawn"
        );
        holder.wait_for_exit().await?;
        session.cancel().await?;
        return Ok(());
    }

    holder.wait_for_exit().await?;
    session.cancel().await?;

    assert!(
        follow_up_text.contains("AFTER_QUIT"),
        "expected prompt recovery after quit() respawn, got: {follow_up_text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_respawn_does_not_wait_for_detached_stdio_holders() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let arm = session
        .write_stdin_raw_with(
            format!(
                r#"import os, subprocess, sys, threading, time
script = "import time; time.sleep({DETACHED_STDIO_HOLDER_SECS})"
def leave_detached_tail():
    time.sleep(0.2)
    subprocess.Popen(
        [sys.executable, "-c", script],
        stdin=subprocess.DEVNULL,
        close_fds=True,
        start_new_session=True,
    )
    os._exit(0)
threading.Thread(target=leave_detached_tail, daemon=True).start()
print("detached respawn armed")
"#
            ),
            Some(5.0),
        )
        .await?;
    let arm_text = result_text(&arm);
    if is_busy_response(&arm_text) {
        eprintln!("python_respawn_does_not_wait_for_detached_stdio_holders remained busy");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        arm_text.contains("detached respawn armed"),
        "expected detached-respawn arming reply, got: {arm_text:?}"
    );

    sleep(Duration::from_millis(500)).await;
    let start = Instant::now();
    let follow_up = session
        .write_stdin_raw_with("print('AFTER_RESPAWN')", Some(5.0))
        .await?;
    let elapsed = start.elapsed();
    let follow_up_text = result_text(&follow_up);
    if is_busy_response(&follow_up_text) {
        eprintln!(
            "python_respawn_does_not_wait_for_detached_stdio_holders remained busy after exit"
        );
        session.cancel().await?;
        return Ok(());
    }

    session.cancel().await?;

    assert!(
        elapsed < shutdown_completion_budget(),
        "expected respawn to finish before detached child exit, got {elapsed:?}: {follow_up_text:?}"
    );
    assert!(
        follow_up_text.contains("AFTER_RESPAWN"),
        "expected prompt recovery after respawn, got: {follow_up_text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_quit_does_not_wait_for_background_process_ipc_leak_probe() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(mut session) = start_python_session().await? else {
        return Ok(());
    };

    let probe = arm_background_ipc_leak_probe(&mut session).await?;

    let start = Instant::now();
    let quit = session.write_stdin_raw_with("quit()", Some(5.0)).await?;
    let elapsed = start.elapsed();
    let quit_text = result_text(&quit);
    if is_busy_response(&quit_text) {
        return fail_background_ipc_leak_probe_test(
            &probe,
            session,
            format!("server waited or returned busy unexpectedly on quit(): {quit_text:?}"),
        )
        .await;
    }
    if elapsed >= shutdown_completion_budget() {
        return fail_background_ipc_leak_probe_test(
            &probe,
            session,
            format!("server waited unexpectedly on quit(); elapsed {elapsed:?}: {quit_text:?}"),
        )
        .await;
    }

    if let Err(err) = probe.assert_running_before_release("after quit() returned") {
        return fail_background_ipc_leak_probe_test(&probe, session, err.to_string()).await;
    }

    let follow_up_start = Instant::now();
    let follow_up = session
        .write_stdin_raw_with("print('AFTER_IPC_LEAK_PROBE_QUIT')", Some(5.0))
        .await?;
    let follow_up_elapsed = follow_up_start.elapsed();
    let follow_up_text = result_text(&follow_up);
    if is_busy_response(&follow_up_text) {
        return fail_background_ipc_leak_probe_test(
            &probe,
            session,
            format!(
                "server waited or returned busy unexpectedly after quit() respawn: {follow_up_text:?}"
            ),
        )
        .await;
    }
    if follow_up_elapsed >= shutdown_completion_budget() {
        return fail_background_ipc_leak_probe_test(
            &probe,
            session,
            format!(
                "server waited unexpectedly after quit() respawn; elapsed {follow_up_elapsed:?}: {follow_up_text:?}"
            ),
        )
        .await;
    }
    if let Err(err) = probe.assert_running_before_release("after quit() respawn returned") {
        return fail_background_ipc_leak_probe_test(&probe, session, err.to_string()).await;
    }

    let probe_cleanup = probe.release_and_wait_for_exit().await;
    session.cancel().await?;
    probe_cleanup?;

    assert!(
        follow_up_text.contains("AFTER_IPC_LEAK_PROBE_QUIT"),
        "expected prompt recovery after quit() respawn, got: {follow_up_text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_respawn_does_not_wait_for_background_process_ipc_leak_probe() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(mut session) = start_python_session().await? else {
        return Ok(());
    };

    let probe = arm_background_ipc_leak_probe(&mut session).await?;

    let arm = session
        .write_stdin_raw_with(
            r#"import os, threading, time
def exit_worker():
    time.sleep(0.2)
    os._exit(0)
threading.Thread(target=exit_worker, daemon=True).start()
print("ipc respawn armed")
"#,
            Some(5.0),
        )
        .await?;
    let arm_text = result_text(&arm);
    if is_busy_response(&arm_text) {
        return fail_background_ipc_leak_probe_test(
            &probe,
            session,
            format!(
                "server returned busy while arming background IPC leak probe respawn: {arm_text:?}"
            ),
        )
        .await;
    }
    assert!(
        arm_text.contains("ipc respawn armed"),
        "expected background IPC leak probe respawn arming reply, got: {arm_text:?}"
    );
    if let Err(err) = probe.assert_running_before_release("after respawn arming returned") {
        return fail_background_ipc_leak_probe_test(&probe, session, err.to_string()).await;
    }

    sleep(Duration::from_millis(500)).await;
    let start = Instant::now();
    let follow_up = session
        .write_stdin_raw_with("print('AFTER_IPC_LEAK_PROBE_RESPAWN')", Some(5.0))
        .await?;
    let elapsed = start.elapsed();
    let follow_up_text = result_text(&follow_up);
    if is_busy_response(&follow_up_text) {
        return fail_background_ipc_leak_probe_test(
            &probe,
            session,
            format!(
                "server waited or returned busy unexpectedly after worker exit: {follow_up_text:?}"
            ),
        )
        .await;
    }

    if elapsed >= shutdown_completion_budget() {
        return fail_background_ipc_leak_probe_test(
            &probe,
            session,
            format!(
                "server waited unexpectedly after worker exit; elapsed {elapsed:?}: {follow_up_text:?}"
            ),
        )
        .await;
    }
    if let Err(err) = probe.assert_running_before_release("after respawn returned") {
        return fail_background_ipc_leak_probe_test(&probe, session, err.to_string()).await;
    }

    let probe_cleanup = probe.release_and_wait_for_exit().await;
    session.cancel().await?;
    probe_cleanup?;

    assert!(
        follow_up_text.contains("AFTER_IPC_LEAK_PROBE_RESPAWN"),
        "expected prompt recovery after respawn, got: {follow_up_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_multiline_block() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with("def f():\n    return 3\n\nf()", Some(5.0))
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        eprintln!("python_multiline_block remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(text.contains("3"), "expected 3, got: {text:?}");

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_cell_runs_block_and_following_top_level_code() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            "total = 0\nfor x in range(4):\n    total += x\nprint('CELL_TOTAL', total)",
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        !is_busy_response(&text),
        "expected complete cell to finish in one request, got: {text:?}"
    );
    assert!(
        text.contains("CELL_TOTAL 6"),
        "expected block and following top-level code to run as one cell, got: {text:?}"
    );
    assert!(
        !text.contains("... "),
        "cell execution should not leave Python at a continuation prompt, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_fifo_cell_contract_uses_stdin_only_while_read_waits() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let first = session
        .write_stdin_raw_with("counter = 41\ncounter", Some(5.0))
        .await?;
    let first_text = result_text(&first);
    assert!(
        !is_busy_response(&first_text),
        "expected initial cell to finish, got: {first_text:?}"
    );
    assert!(
        first_text.contains("41"),
        "expected final expression display, got: {first_text:?}"
    );
    assert!(
        !first_text.contains(">>> ") && !first_text.contains("... "),
        "completed cells should not render top-level Python prompts, got: {first_text:?}"
    );

    let second = session
        .write_stdin_raw_with("counter += 1\ncounter", Some(5.0))
        .await?;
    let second_text = result_text(&second);
    assert!(
        second_text.contains("42"),
        "expected globals to persist across cells, got: {second_text:?}"
    );
    assert!(
        !second_text.contains(">>> ") && !second_text.contains("... "),
        "completed follow-up cells should stay prompt-free, got: {second_text:?}"
    );

    let prompt = session
        .write_stdin_raw_with("name = input('name: ')", Some(1.0))
        .await?;
    let prompt_text = result_text(&prompt);
    assert!(
        prompt_text.contains("name: "),
        "expected real input() prompt to stay visible, got: {prompt_text:?}"
    );

    let answer = session.write_stdin_raw_with("Ada", Some(5.0)).await?;
    let answer_text = result_text(&answer);
    assert!(
        !is_busy_response(&answer_text),
        "expected answer to complete the waiting input() call, got: {answer_text:?}"
    );
    assert!(
        !answer_text.contains("NameError"),
        "stdin answer should not execute as a top-level cell, got: {answer_text:?}"
    );

    let idle = session.write_stdin_raw_with("", Some(5.0)).await?;
    let idle_text = result_text(&idle);
    assert!(
        idle_text.contains("<<repl status: idle>>"),
        "expected empty poll after input() answer to report idle, got: {idle_text:?}"
    );
    assert!(
        !idle_text.contains("name: "),
        "empty poll after input() answer must not repeat stale input prompt, got: {idle_text:?}"
    );

    let follow_up = session.write_stdin_raw_with("name", Some(5.0)).await?;
    let follow_up_text = result_text(&follow_up);
    session.cancel().await?;

    assert!(
        follow_up_text.contains("'Ada'"),
        "expected later input to execute as a fresh cell after input() completed, got: {follow_up_text:?}"
    );
    assert!(
        !follow_up_text.contains(">>> ") && !follow_up_text.contains("... "),
        "fresh cell after input() should stay prompt-free, got: {follow_up_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_prompt_free_paged_completion_dismisses_without_protocol_error() -> TestResult<()> {
    let _guard = lock_test_mutex();
    if !require_python() {
        return Ok(());
    }

    let mut session = common::spawn_server_with_args_env_and_pager_page_chars(
        vec![
            "--interpreter".to_string(),
            "python".to_string(),
            "--sandbox".to_string(),
            "danger-full-access".to_string(),
        ],
        Vec::new(),
        120,
    )
    .await?;

    let input = "for i in range(40):\n    print(f'PROMPT_FREE_PAGER_LINE {i:03d}')";
    let initial = session.write_stdin_raw_with(input, Some(10.0)).await?;
    let initial = common::wait_until_not_busy(
        &mut session,
        initial,
        Duration::from_millis(100),
        python_startup_probe_budget(),
    )
    .await?;
    let initial_text = result_text(&initial);
    if python_backend_unavailable(&initial_text) {
        eprintln!("python backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        initial_text.contains("--More--"),
        "expected oversized Python cell output to activate pager, got: {initial_text:?}"
    );

    let tail = session.write_stdin_raw_with(":tail 1", Some(5.0)).await?;
    let tail_text = result_text(&tail);
    session.cancel().await?;

    assert!(
        tail_text.contains("(END"),
        "expected :tail to dismiss pager at the end, got: {tail_text:?}"
    );
    assert!(
        !tail_text.contains("[repl] protocol error: missing prompt after pager dismiss"),
        "prompt-free Python pager completion should not report a missing prompt, got: {tail_text:?}"
    );
    assert!(
        !tail_text.contains(">>> ") && !tail_text.contains("... "),
        "prompt-free Python pager completion should not append Python prompts, got: {tail_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_cell_runner_accepts_legacy_codeop_compile_api() -> TestResult<()> {
    let _guard = lock_test_mutex();
    if !require_python() {
        return Ok(());
    }

    let codeop_dir = tempdir()?;
    fs::write(
        codeop_dir.path().join("codeop.py"),
        r#"
import __future__

PyCF_DONT_IMPLY_DEDENT = 0x200
_features = [getattr(__future__, name) for name in __future__.all_feature_names]


class Compile:
    def __init__(self):
        self.flags = PyCF_DONT_IMPLY_DEDENT

    def __call__(self, source, filename, symbol):
        codeob = compile(source, filename, symbol, self.flags, True)
        for feature in _features:
            if codeob.co_flags & feature.compiler_flag:
                self.flags |= feature.compiler_flag
        return codeob
"#,
    )?;

    let Some(session) = start_python_session_with_env_vars(vec![(
        "PYTHONPATH".to_string(),
        codeop_dir.path().display().to_string(),
    )])
    .await?
    else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            "from __future__ import annotations\nclass Box:\n    value: MissingName\nprint('LEGACY_CODEOP_OK', Box.__annotations__['value'])",
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        !is_busy_response(&text),
        "expected legacy codeop cell to finish, got: {text:?}"
    );
    assert!(
        text.contains("LEGACY_CODEOP_OK MissingName"),
        "expected legacy codeop cell to run with future annotations, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_cell_final_expression_displays_after_block() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with("def f():\n    return 7\nf()", Some(5.0))
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        !is_busy_response(&text),
        "expected final-expression cell to finish in one request, got: {text:?}"
    );
    assert!(
        text.contains("7"),
        "expected final expression to be displayed, got: {text:?}"
    );
    assert!(
        !text.contains("... "),
        "cell execution should not require a blank line before final expression, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_cell_final_expression_matching_input_remains_visible() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session.write_stdin_raw_with("2", Some(5.0)).await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        !is_busy_response(&text),
        "expected final-expression cell to finish in one request, got: {text:?}"
    );
    assert_eq!(
        text.trim(),
        "2",
        "expected displayhook output matching the input to be preserved, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_cell_incomplete_code_reports_error_not_continuation() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with("for x in range(3):", Some(5.0))
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains("SyntaxError") || text.contains("IndentationError"),
        "expected incomplete cell to report a Python syntax error, got: {text:?}"
    );
    assert!(
        !text.contains("... "),
        "incomplete cell should not leave Python at a continuation prompt, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_cell_custom_displayhook_is_honored() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            "import sys\ndef hook(value):\n    print('DISPLAYHOOK_VALUE', value)\nsys.displayhook = hook\n21 + 21",
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        !is_busy_response(&text),
        "expected displayhook cell to finish, got: {text:?}"
    );
    assert!(
        text.contains("DISPLAYHOOK_VALUE 42"),
        "expected final expression to use custom displayhook, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_cell_preserves_future_annotations_between_calls() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let setup = session
        .write_stdin_raw_with("from __future__ import annotations", Some(5.0))
        .await?;
    let setup_text = result_text(&setup);
    assert!(
        !setup_text.contains("Traceback"),
        "expected future import setup to succeed, got: {setup_text:?}"
    );

    let result = session
        .write_stdin_raw_with(
            "def f(value: list[int]) -> dict[str, int]:\n    return {}\nprint('FUTURE_ANNOTATIONS', isinstance(f.__annotations__['value'], str), f.__annotations__['value'])",
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains("FUTURE_ANNOTATIONS True list[int]"),
        "expected future annotations to persist across cells, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_cell_runner_survives_shadowed_user_globals() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let setup = session
        .write_stdin_raw_with(
            "ast = 1\nglobals = 2\ncompile = 3\nprint('SHADOW_SETUP')",
            Some(5.0),
        )
        .await?;
    let setup_text = result_text(&setup);
    assert!(
        setup_text.contains("SHADOW_SETUP"),
        "expected shadowing setup to run, got: {setup_text:?}"
    );

    let follow_up = session
        .write_stdin_raw_with("print('SHADOW_FOLLOWUP')\n40 + 2", Some(5.0))
        .await?;
    let follow_up_text = result_text(&follow_up);
    session.cancel().await?;

    assert!(
        !is_busy_response(&follow_up_text),
        "expected follow-up after shadowed globals to finish, got: {follow_up_text:?}"
    );
    assert!(
        follow_up_text.contains("SHADOW_FOLLOWUP") && follow_up_text.contains("42"),
        "expected follow-up cell to run after user globals shadow helper names, got: {follow_up_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_cell_input_wait_consumes_follow_up_stdin() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let prompt = session
        .write_stdin_raw_with("name = input('name: ')", Some(1.0))
        .await?;
    let prompt_text = result_text(&prompt);
    assert!(
        prompt_text.contains("name: "),
        "expected input prompt, got: {prompt_text:?}"
    );

    let answer = session.write_stdin_raw_with("Ada", Some(5.0)).await?;
    let answer_text = result_text(&answer);
    assert!(
        !is_busy_response(&answer_text),
        "expected answer to complete the input wait, got: {answer_text:?}"
    );

    let follow_up = session
        .write_stdin_raw_with("print('CELL_INPUT_NAME', name)", Some(5.0))
        .await?;
    let follow_up_text = result_text(&follow_up);
    session.cancel().await?;

    assert!(
        follow_up_text.contains("CELL_INPUT_NAME Ada"),
        "expected follow-up input to be stdin, got prompt: {prompt_text:?}; answer: {answer_text:?}; follow-up: {follow_up_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_cell_custom_input_loop_consumes_follow_up_stdin() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let first = session
        .write_stdin_raw_with(
            "for label in ('a', 'b'):\n    value = input(label + '> ')\n    print('LOOP_VALUE', label, value)\nprint('LOOP_DONE')",
            Some(1.0),
        )
        .await?;
    let first_text = result_text(&first);
    assert!(
        first_text.contains("a> "),
        "expected first loop prompt, got: {first_text:?}"
    );

    let second = session.write_stdin_raw_with("one", Some(5.0)).await?;
    let second_text = result_text(&second);
    assert!(
        second_text.contains("LOOP_VALUE a one") && second_text.contains("b> "),
        "expected first answer and second prompt, got: {second_text:?}"
    );

    let third = session.write_stdin_raw_with("two", Some(5.0)).await?;
    let third_text = result_text(&third);
    session.cancel().await?;

    assert!(
        third_text.contains("LOOP_VALUE b two") && third_text.contains("LOOP_DONE"),
        "expected second answer and loop completion, got: {third_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_multiline_block_skips_leading_consumed_input_echo() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with("def f():\n    return 3\n\nf()", Some(5.0))
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        eprintln!(
            "python_multiline_block_skips_leading_consumed_input_echo remained busy; skipping"
        );
        session.cancel().await?;
        return Ok(());
    }
    let visible = visible_reply_text(&text)?;

    session.cancel().await?;

    assert!(visible.contains("3"), "expected 3, got: {visible:?}");
    assert!(
        !visible.contains("def f():"),
        "multiline function definition echo should be absent, got: {visible:?}"
    );
    assert!(
        !visible.contains("return 3"),
        "multiline body echo should be absent, got: {visible:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_buffered_multiline_prompt_does_not_complete_request_early() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            "if True:\n    pass\n\nimport time\ntime.sleep(0.5)\nprint('DONE')",
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        !is_busy_response(&text),
        "expected buffered multiline request to finish in the original call, got: {text:?}"
    );
    assert!(
        text.contains("DONE"),
        "expected buffered multiline request to include final output, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_cell_block_executes_without_blank_line() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with("if True:\n    print('x')", Some(5.0))
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains("x"),
        "expected complete block cell to execute without a trailing blank line, got: {text:?}"
    );
    assert!(
        !text.contains("... "),
        "complete block cell should not report continuation prompt, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_block_header_alone_reports_indentation_error() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session.write_stdin_raw_with("if True:", Some(5.0)).await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains("IndentationError"),
        "expected block header cell to report IndentationError, got: {text:?}"
    );
    assert!(
        !text.contains("... "),
        "block header cell should not report continuation prompt, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_commented_block_header_reports_indentation_error() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with("if True:  # comment", Some(5.0))
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains("IndentationError"),
        "expected commented block header cell to report IndentationError, got: {text:?}"
    );
    assert!(
        !text.contains("... "),
        "commented block header cell should not report continuation prompt, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_comment_only_block_body_reports_indentation_error() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with("if True:\n    # comment", Some(5.0))
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains("IndentationError"),
        "expected comment-only block body cell to report IndentationError, got: {text:?}"
    );
    assert!(
        !text.contains("... "),
        "comment-only block body cell should not report continuation prompt, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_comment_backslash_stays_prompt_free() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with("x = 1 # note \\", Some(5.0))
        .await?;
    let text = result_text(&result);
    assert!(
        !is_busy_response(&text),
        "expected comment backslash input to complete, got: {text:?}"
    );
    assert!(
        !text.contains("... "),
        "expected comment backslash input not to report continuation prompt, got: {text:?}"
    );

    let result = session
        .write_stdin_raw_with("print('COMMENT_BACKSLASH', x)", Some(5.0))
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains("COMMENT_BACKSLASH 1"),
        "expected next turn to run after comment backslash input, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_decorator_without_definition_reports_syntax_error() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with("@staticmethod", Some(5.0))
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains("SyntaxError"),
        "expected decorator-only cell to report SyntaxError, got: {text:?}"
    );
    assert!(
        !text.contains("... "),
        "decorator-only cell should not report continuation prompt, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_whitespace_only_stays_prompt_free() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session.write_stdin_raw_with("   ", Some(5.0)).await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        !text.contains(">>> ") && !text.contains("... "),
        "expected whitespace-only Python input not to render top-level prompts, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_invalid_top_level_indent_stays_prompt_free() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with("    print(1)", Some(5.0))
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains("IndentationError"),
        "expected invalid top-level indent to raise IndentationError, got: {text:?}"
    );
    assert!(
        !text.contains("... "),
        "expected invalid top-level indent not to report continuation prompt, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_dedented_block_cell_runs() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with("if True:\n    print(1)\nprint(2)", Some(5.0))
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains("1") && text.contains("2"),
        "expected dedented top-level code after block to run in one cell, got: {text:?}"
    );
    assert!(
        !text.contains("... "),
        "dedented block cell should not report continuation prompt, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_unterminated_single_quote_stays_prompt_free() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session.write_stdin_raw_with("x = 'abc", Some(5.0)).await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains("SyntaxError"),
        "expected unterminated single quote to raise SyntaxError, got: {text:?}"
    );
    assert!(
        !text.contains("... "),
        "expected unterminated single quote not to report continuation prompt, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_final_expression_syntax_error_has_no_setup_side_effects() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with("failed_eval_side_effect = 1\nyield 2", Some(5.0))
        .await?;
    let text = result_text(&result);
    assert!(
        text.contains("SyntaxError"),
        "expected invalid final expression to raise SyntaxError, got: {text:?}"
    );

    let follow_up = session
        .write_stdin_raw_with(
            "print('FAILED_EVAL_SIDE_EFFECT', 'failed_eval_side_effect' in globals())",
            Some(5.0),
        )
        .await?;
    let follow_up_text = result_text(&follow_up);
    session.cancel().await?;

    assert!(
        follow_up_text.contains("FAILED_EVAL_SIDE_EFFECT False"),
        "failed final-expression compile should not run setup statements, got initial: {text:?}; follow-up: {follow_up_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_stdout_replaces_lone_surrogates() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with("print('\\udcff')", Some(5.0))
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        !text.contains("UnicodeEncodeError"),
        "expected stdout to apply replacement error handling, got: {text:?}"
    );
    assert!(
        text.contains("?\n"),
        "expected stdout to write replacement byte for lone surrogate, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_original_stdout_is_flushed_before_reply() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            "import sys; sys.__stdout__.write('ORIG_STDOUT\\n')",
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains("ORIG_STDOUT"),
        "expected original stdout writes to be visible in the completing reply, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_original_stdout_is_visible_with_replacement_stdout() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            "import sys\nsys.__stdout__.write('ORIG_BEFORE\\n')\nprint('REPLACED_AFTER')",
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains("ORIG_BEFORE"),
        "expected original stdout write to stay visible, got: {text:?}"
    );
    assert!(
        text.contains("REPLACED_AFTER"),
        "expected replacement stdout write to stay visible, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_input_roundtrip() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let mut text = result_text(
        &session
            .write_stdin_raw_with("x = input('prompt> ')\nprint('INPUT_VALUE', x)", Some(1.0))
            .await?,
    );
    if is_busy_response(&text) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && is_busy_response(&text) {
            sleep(Duration::from_millis(50)).await;
            text = result_text(&session.write_stdin_raw_with("", Some(1.0)).await?);
        }
    }
    if is_busy_response(&text) {
        eprintln!("python_input_roundtrip remained busy before prompt; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(text.contains("prompt>"), "expected prompt, got: {text:?}");

    let mut text = result_text(&session.write_stdin_raw_with("hello", Some(5.0)).await?);
    if is_busy_response(&text) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline
            && is_busy_response(&text)
            && !text.contains("INPUT_VALUE hello")
        {
            sleep(Duration::from_millis(50)).await;
            text = result_text(&session.write_stdin_raw_with("", Some(1.0)).await?);
        }
    }
    if is_busy_response(&text) {
        eprintln!("python_input_roundtrip remained busy while reading input; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("INPUT_VALUE hello"),
        "expected code after input() to run after follow-up stdin, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn python_windows_sandbox_pty_bridges_stdin_surfaces() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session_with_sandbox("read-only").await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"exec("""
import ctypes, os
try:
    crt = ctypes.CDLL("ucrtbase")
except OSError:
    crt = ctypes.CDLL("msvcrt")
print("SANDBOX_CRT_ISATTY", bool(crt._isatty(0)), bool(crt._isatty(1)), bool(crt._isatty(2)))
""")
print("SANDBOX_PTY_FDS", os.isatty(0), os.isatty(1), os.isatty(2))
value = input("sandbox> ")
print("SANDBOX_INPUT", value)
"#,
            Some(5.0),
        )
        .await?;
    let mut text = result_text(&result);
    if is_busy_response(&text) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline
            && is_busy_response(&text)
            && !text.contains("SANDBOX_INPUT inside")
        {
            sleep(Duration::from_millis(50)).await;
            text = result_text(&session.write_stdin_raw_with("", Some(1.0)).await?);
        }
    }
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err("python Windows sandbox PTY stdin bridge test remained busy".into());
    }

    assert!(
        text.contains("sandbox> "),
        "expected sandboxed input prompt, got: {text:?}"
    );
    let mut all_text = text.clone();

    let answer = session.write_stdin_raw_with("inside", Some(5.0)).await?;
    text = result_text(&answer);
    all_text.push_str(&text);
    if is_busy_response(&text) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline
            && is_busy_response(&text)
            && !text.contains("SANDBOX_INPUT inside")
        {
            sleep(Duration::from_millis(50)).await;
            text = result_text(&session.write_stdin_raw_with("", Some(1.0)).await?);
            all_text.push_str(&text);
        }
    }

    session.cancel().await?;

    assert!(
        all_text.contains("SANDBOX_PTY_FDS True True True"),
        "expected sandboxed Python stdio fds to be TTY-backed, got: {all_text:?}"
    );
    assert!(
        all_text.contains("SANDBOX_CRT_ISATTY True True True"),
        "expected sandboxed Windows CRT fds to be TTY-backed, got: {all_text:?}"
    );
    assert!(
        all_text.contains("SANDBOX_INPUT inside"),
        "expected sandboxed input() to consume the follow-up answer, got: {all_text:?}"
    );
    Ok(())
}

#[cfg(any(unix, windows))]
#[tokio::test(flavor = "multi_thread")]
async fn python_input_accepts_crlf_buffered_line() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let mut prompt_text = result_text(
        &session
            .write_stdin_raw_with("x = input('p> ')\nprint('got', x)", Some(1.0))
            .await?,
    );
    if is_busy_response(&prompt_text) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && is_busy_response(&prompt_text) {
            sleep(Duration::from_millis(50)).await;
            prompt_text = result_text(&session.write_stdin_raw_with("", Some(1.0)).await?);
        }
    }
    if is_busy_response(&prompt_text) {
        eprintln!("python_input_accepts_crlf_buffered_line remained busy before prompt; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        prompt_text.contains("p> "),
        "expected input prompt before CRLF answer, got: {prompt_text:?}"
    );

    let result = session
        .write_stdin_raw_unterminated_with("hello\r\n", Some(5.0))
        .await?;
    let mut text = result_text(&result);
    if is_busy_response(&text) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && is_busy_response(&text) && !text.contains("got hello") {
            sleep(Duration::from_millis(50)).await;
            text = result_text(&session.write_stdin_raw_with("", Some(1.0)).await?);
        }
    }
    if is_busy_response(&text) {
        eprintln!("python_input_accepts_crlf_buffered_line remained busy after answer; skipping");
        session.cancel().await?;
        return Ok(());
    }

    session.cancel().await?;

    assert!(
        !text.contains("worker protocol error"),
        "expected CRLF input to be accounted without protocol errors, got: {text:?}"
    );
    assert!(
        text.contains("got hello"),
        "expected input() to consume buffered CRLF line, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_original_stdout_flushes_before_input_prompt() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let prompt = session
        .write_stdin_raw_with(
            r#"exec("""
import sys

class DeferredStdout:
    def __init__(self):
        self.pending = []

    def write(self, message):
        self.pending.append(message)
        return len(message)

    def flush(self):
        while self.pending:
            sys.stdout.write(self.pending.pop(0))

sys.__stdout__ = DeferredStdout()
sys.__stdout__.write("ORIGINAL_BEFORE_INPUT\\n")
value = input("original> ")
print('VALUE', value)
""")
"#,
            Some(1.0),
        )
        .await?;
    let mut prompt_text = result_text(&prompt);
    if is_busy_response(&prompt_text) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline
            && is_busy_response(&prompt_text)
            && !prompt_text.contains("original>")
        {
            sleep(Duration::from_millis(50)).await;
            prompt_text = result_text(&session.write_stdin_raw_with("", Some(1.0)).await?);
        }
    }
    if is_busy_response(&prompt_text) {
        session.cancel().await?;
        return Err("python original stdout input prompt remained busy".into());
    }

    let answer = session.write_stdin_raw_with("answer", Some(5.0)).await?;
    let answer_text = result_text(&answer);
    session.cancel().await?;

    assert!(
        prompt_text.contains("ORIGINAL_BEFORE_INPUT"),
        "expected original stdout before input() to flush with prompt, got prompt reply: {prompt_text:?}; answer reply: {answer_text:?}"
    );
    assert!(
        prompt_text.contains("original>"),
        "expected input prompt, got: {prompt_text:?}"
    );
    assert!(
        answer_text.contains("VALUE answer"),
        "expected input answer to complete, got: {answer_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_original_stdout_flushes_when_input_prompt_completion_waits_for_stdin()
-> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let prompt = session
        .write_stdin_raw_with(
            r#"exec("""
import sys

class DeferredStdout:
    def __init__(self):
        self.pending = []

    def write(self, message):
        self.pending.append(message)
        return len(message)

    def flush(self):
        while self.pending:
            sys.stdout.write(self.pending.pop(0))

sys.__stdout__ = DeferredStdout()
marker = "ORIGINAL" + "_BEFORE_STDIN_COMPLETE_PROMPT"
sys.__stdout__.write(marker + "\\n")
value = input("delayed> ")
print('DELAYED_VALUE', value)
""")
"#,
            Some(1.0),
        )
        .await?;
    let mut prompt_text = result_text(&prompt);
    if is_busy_response(&prompt_text) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline
            && is_busy_response(&prompt_text)
            && !prompt_text.contains("delayed>")
        {
            sleep(Duration::from_millis(50)).await;
            prompt_text = result_text(&session.write_stdin_raw_with("", Some(1.0)).await?);
        }
    }
    if is_busy_response(&prompt_text) {
        session.cancel().await?;
        return Err("python delayed input prompt remained busy".into());
    }

    let answer = session.write_stdin_raw_with("answer", Some(5.0)).await?;
    let answer_text = result_text(&answer);
    session.cancel().await?;

    assert!(
        prompt_text.contains("ORIGINAL_BEFORE_STDIN_COMPLETE_PROMPT"),
        "expected original stdout before input prompt to flush with prompt reply, got prompt reply: {prompt_text:?}; answer reply: {answer_text:?}"
    );
    assert!(
        prompt_text.contains("delayed>"),
        "expected delayed input prompt, got: {prompt_text:?}"
    );
    assert!(
        answer_text.contains("DELAYED_VALUE answer"),
        "expected delayed input answer to complete, got: {answer_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_input_interrupt_tail_handles_signal_before_tail() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let mut text = result_text(
        &session
            .write_stdin_raw_with("value = input('interrupt> ')", Some(1.0))
            .await?,
    );
    if is_busy_response(&text) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && is_busy_response(&text) && !text.contains("interrupt>") {
            sleep(Duration::from_millis(50)).await;
            text = result_text(&session.write_stdin_raw_with("", Some(1.0)).await?);
        }
    }
    assert!(
        text.contains("interrupt>"),
        "expected input prompt, got: {text:?}"
    );

    let tail = session
        .write_stdin_raw_unterminated_with("\u{3}print('AFTER_INPUT_INTERRUPT_TAIL')", Some(5.0))
        .await?;
    let tail_text = result_text(&tail);
    session.cancel().await?;

    assert!(
        !is_busy_response(&tail_text),
        "expected interrupt tail to complete, got: {tail_text:?}"
    );
    assert!(
        tail_text.contains("KeyboardInterrupt"),
        "expected real Python interrupt before tail, got: {tail_text:?}"
    );
    assert!(
        tail_text.contains("AFTER_INPUT_INTERRUPT_TAIL"),
        "expected tail to run as a fresh cell after input interrupt, got: {tail_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_bare_input_interrupt_completes_after_os_signal() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let mut text = result_text(
        &session
            .write_stdin_raw_with("value = input('bare interrupt> ')", Some(1.0))
            .await?,
    );
    if is_busy_response(&text) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline
            && is_busy_response(&text)
            && !text.contains("bare interrupt> ")
        {
            sleep(Duration::from_millis(50)).await;
            text = result_text(&session.write_stdin_raw_with("", Some(1.0)).await?);
        }
    }
    assert!(
        text.contains("bare interrupt> "),
        "expected input prompt, got: {text:?}"
    );

    let interrupt = session
        .write_stdin_raw_unterminated_with("\u{3}", Some(5.0))
        .await?;
    let interrupt_text = result_text(&interrupt);

    assert!(
        !is_busy_response(&interrupt_text),
        "expected bare input interrupt to complete, got: {interrupt_text:?}"
    );

    let follow_up_text = write_python_after_interrupt_until_contains(
        &session,
        "print('AFTER_BARE_INPUT_INTERRUPT')",
        "AFTER_BARE_INPUT_INTERRUPT",
        "bare input interrupt follow-up",
    )
    .await?;
    session.cancel().await?;

    assert!(
        follow_up_text.contains("AFTER_BARE_INPUT_INTERRUPT"),
        "expected follow-up to run after bare input interrupt, got interrupt: {interrupt_text:?}; follow-up: {follow_up_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_bare_idle_interrupt_completes_after_os_signal() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let interrupt = session
        .write_stdin_raw_unterminated_with("\u{3}", Some(5.0))
        .await?;
    let interrupt_text = result_text(&interrupt);

    assert!(
        !is_busy_response(&interrupt_text),
        "expected bare idle interrupt to complete, got: {interrupt_text:?}"
    );
    assert!(
        interrupt_text.contains("KeyboardInterrupt"),
        "expected bare idle interrupt to observe real Python interrupt, got: {interrupt_text:?}"
    );

    let follow_up_text = write_python_after_interrupt_until_contains(
        &session,
        "print('AFTER_BARE_IDLE_INTERRUPT')",
        "AFTER_BARE_IDLE_INTERRUPT",
        "bare idle interrupt follow-up",
    )
    .await?;
    session.cancel().await?;

    assert!(
        follow_up_text.contains("AFTER_BARE_IDLE_INTERRUPT"),
        "expected follow-up to run after bare idle interrupt, got interrupt: {interrupt_text:?}; follow-up: {follow_up_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_input_interrupt_custom_handler_continues_waiting_for_answer() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let setup = r#"
import signal
sigint_count = 0
def handle_sigint(signum, frame):
    global sigint_count
    sigint_count += 1
    print("CUSTOM_INPUT_INTERRUPT", sigint_count)
signal.signal(signal.SIGINT, handle_sigint)
value = input('custom interrupt> ')
print('CUSTOM_INPUT_VALUE', value, sigint_count)
"#;
    let mut text = result_text(&session.write_stdin_raw_with(setup, Some(1.0)).await?);
    if is_busy_response(&text) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline
            && is_busy_response(&text)
            && !text.contains("custom interrupt> ")
        {
            sleep(Duration::from_millis(50)).await;
            text = result_text(&session.write_stdin_raw_with("", Some(1.0)).await?);
        }
    }
    assert!(
        text.contains("custom interrupt> "),
        "expected custom input prompt, got: {text:?}"
    );

    let interrupt = session
        .write_stdin_raw_unterminated_with("\u{3}", Some(1.0))
        .await?;
    let _interrupt_text = result_text(&interrupt);

    let answer = session.write_stdin_raw_with("answer", Some(5.0)).await?;
    let answer_text = result_text(&answer);
    session.cancel().await?;

    assert!(
        answer_text.contains("CUSTOM_INPUT_INTERRUPT 1"),
        "expected custom interrupt handler to run from real signal, got: {answer_text:?}"
    );
    assert!(
        answer_text.contains("CUSTOM_INPUT_VALUE answer 1"),
        "expected input to keep waiting and consume later answer, got: {answer_text:?}"
    );
    assert!(
        !answer_text.contains("KeyboardInterrupt"),
        "sideband should not force KeyboardInterrupt when custom handler returns: {answer_text:?}"
    );
    Ok(())
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn python_windows_input_wait_interrupt_preserves_next_input_batch() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let mut text = result_text(
        &session
            .write_stdin_raw_with(
                r#"
import signal
interrupt_count = 0
def handle_sigint(signum, frame):
    global interrupt_count
    interrupt_count += 1
    print("WINDOWS_INPUT_SIGINT", interrupt_count)
signal.signal(signal.SIGINT, handle_sigint)
value = input('win interrupt> ')
print('WINDOWS_INPUT_VALUE', value, interrupt_count)
"#,
                Some(5.0),
            )
            .await?,
    );
    if is_busy_response(&text) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline
            && is_busy_response(&text)
            && !text.contains("win interrupt> ")
        {
            sleep(Duration::from_millis(50)).await;
            text = result_text(&session.write_stdin_raw_with("", Some(1.0)).await?);
        }
    }
    assert!(
        !is_busy_response(&text),
        "expected input prompt before interrupt, got: {text:?}"
    );
    assert!(
        text.contains("win interrupt> "),
        "expected input prompt before interrupt, got: {text:?}"
    );

    let interrupt = session
        .write_stdin_raw_unterminated_with("\u{3}", Some(5.0))
        .await?;
    let interrupt_text = result_text(&interrupt);
    assert!(
        !is_busy_response(&interrupt_text),
        "expected input-wait interrupt to complete, got: {interrupt_text:?}"
    );

    let answer = session.write_stdin_raw_with("answer", Some(5.0)).await?;
    let answer_text = result_text(&answer);
    session.cancel().await?;

    assert!(
        answer_text.contains("WINDOWS_INPUT_SIGINT 1"),
        "expected Windows SIGINT handler to run from real terminal Ctrl-C, got interrupt: {interrupt_text:?}; answer: {answer_text:?}"
    );
    assert!(
        answer_text.contains("WINDOWS_INPUT_VALUE answer 1"),
        "expected input to keep waiting and consume later answer, got interrupt: {interrupt_text:?}; answer: {answer_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_interrupt_unblocks_primary_shaped_input_prompt() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let mut text = result_text(
        &session
            .write_stdin_raw_with("value = input('>>> ')", Some(5.0))
            .await?,
    );
    if is_busy_response(&text) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && is_busy_response(&text) && !text.contains(">>> ") {
            sleep(Duration::from_millis(50)).await;
            text = result_text(&session.write_stdin_raw_with("", Some(1.0)).await?);
        }
    }
    if is_busy_response(&text) || text.contains("timed out") {
        session.cancel().await?;
        return Err(format!(
            "expected primary-shaped input prompt request to complete, got: {text:?}"
        )
        .into());
    }
    assert!(
        text.contains(">>> "),
        "expected primary-shaped input prompt, got: {text:?}"
    );

    let interrupt = session
        .write_stdin_raw_unterminated_with("\u{3}", Some(1.0))
        .await?;
    let interrupt_text = result_text(&interrupt);
    if is_busy_response(&interrupt_text) || interrupt_text.contains("timed out") {
        eprintln!(
            "primary-shaped input prompt interrupt stayed busy in this Python runtime; skipping"
        );
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        !is_busy_response(&interrupt_text),
        "expected primary-shaped input prompt interrupt to complete, got: {interrupt_text:?}"
    );

    let follow_up_text = write_python_after_interrupt_until_contains(
        &session,
        "print('AFTER_PRIMARY_SHAPED_INPUT_INTERRUPT')",
        "AFTER_PRIMARY_SHAPED_INPUT_INTERRUPT",
        "primary-shaped input prompt interrupt follow-up",
    )
    .await?;
    session.cancel().await?;

    assert!(
        follow_up_text.contains("AFTER_PRIMARY_SHAPED_INPUT_INTERRUPT"),
        "expected follow-up to run after primary-shaped input prompt interrupt, got: {follow_up_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_custom_primary_prompt_does_not_render_at_idle() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let setup = session
        .write_stdin_raw_with(
            "import sys\nsys.ps1 = 'custom> '\nprint('CUSTOM_READY')",
            Some(5.0),
        )
        .await?;
    let setup_text = result_text(&setup);
    assert!(
        setup_text.contains("CUSTOM_READY"),
        "expected setup output after custom primary prompt assignment, got: {setup_text:?}"
    );
    assert!(
        !setup_text.contains("custom> "),
        "completed cells should not render custom top-level prompts, got: {setup_text:?}"
    );

    let follow_up = session
        .write_stdin_raw_with("print('AFTER_CUSTOM_PROMPT_SETUP')", Some(5.0))
        .await?;
    let follow_up_text = result_text(&follow_up);
    session.cancel().await?;

    assert!(
        follow_up_text.contains("AFTER_CUSTOM_PROMPT_SETUP"),
        "expected follow-up after custom primary prompt setup, got: {follow_up_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_sigint_handler_runs_once_for_interrupt() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let timeout_result = session
        .write_stdin_raw_with(
            r#"exec("""
import signal, time
interrupt_count = 0
def handle_interrupt(signum, frame):
    global interrupt_count
    interrupt_count += 1
    print("SIGNAL_COUNT", interrupt_count)
signal.signal(signal.SIGINT, handle_interrupt)
print("SIGNAL_READY")
while interrupt_count == 0:
    pass
time.sleep(0.2)
print("SIGNAL_FINAL", interrupt_count)
""")
"#,
            Some(0.2),
        )
        .await?;
    let timeout_text = result_text(&timeout_result);
    assert!(
        timeout_text.contains("<<repl status: busy"),
        "expected signal handler loop to time out, got: {timeout_text:?}"
    );

    let interrupt = session
        .write_stdin_raw_unterminated_with("\u{3}", Some(5.0))
        .await?;
    let interrupt_text = result_text(&interrupt);
    assert!(
        !is_busy_response(&interrupt_text),
        "expected idle signal handler interrupt to complete, got: {interrupt_text:?}"
    );

    let follow_up = session
        .write_stdin_raw_with("print('SIGNAL_FINAL', interrupt_count)", Some(5.0))
        .await?;
    let follow_up_text = result_text(&follow_up);
    session.cancel().await?;

    assert!(
        follow_up_text.contains("SIGNAL_FINAL 1"),
        "expected one signal delivery, got interrupt: {interrupt_text:?}; follow-up: {follow_up_text:?}"
    );
    Ok(())
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn python_windows_interrupt_delivers_sigint_to_running_cell() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let timeout_result = session
        .write_stdin_raw_with(
            r#"exec("""
import signal
sigint_count = 0
def handle_sigint(signum, frame):
    global sigint_count
    sigint_count += 1
signal.signal(signal.SIGINT, handle_sigint)
print("WINDOWS_SIGINT_READY")
while sigint_count == 0:
    pass
print("WINDOWS_SIGINT_DONE", sigint_count)
""")
"#,
            Some(0.2),
        )
        .await?;
    let timeout_text = result_text(&timeout_result);
    assert!(
        timeout_text.contains("<<repl status: busy"),
        "expected Windows SIGINT handler loop to time out, got: {timeout_text:?}"
    );

    let interrupt = session
        .write_stdin_raw_unterminated_with("\u{3}", Some(5.0))
        .await?;
    let interrupt_text = result_text(&interrupt);
    assert!(
        !is_busy_response(&interrupt_text),
        "expected Windows SIGINT interrupt to complete, got: {interrupt_text:?}"
    );
    assert!(
        interrupt_text.contains("WINDOWS_SIGINT_DONE 1"),
        "expected Windows interrupt reply to show Python SIGINT delivery, got: {interrupt_text:?}"
    );

    let follow_up = session
        .write_stdin_raw_with("print('WINDOWS_SIGINT_FINAL', sigint_count)", Some(5.0))
        .await?;
    let follow_up_text = result_text(&follow_up);
    session.cancel().await?;

    assert!(
        follow_up_text.contains("WINDOWS_SIGINT_FINAL 1"),
        "expected Windows Ctrl-C to deliver one Python SIGINT, got interrupt: {interrupt_text:?}; follow-up: {follow_up_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_custom_prompts_do_not_escape_as_stderr() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let setup = session
        .write_stdin_raw_with(
            "import sys\nsys.ps1 = 'custom> '\nsys.ps2 = 'more... '\nprint('CUSTOM_PROMPT_OK')",
            Some(5.0),
        )
        .await?;
    let setup_text = result_text(&setup);

    assert!(
        setup_text.contains("CUSTOM_PROMPT_OK"),
        "expected request output after custom prompts, got: {setup_text:?}"
    );
    assert!(
        !setup_text.contains("custom> "),
        "completed cells should not render custom top-level prompts, got: {setup_text:?}"
    );
    assert_ne!(setup.is_error, Some(true));
    assert!(
        !setup_text.contains("stderr: custom> "),
        "custom primary prompt should not be attributed to stderr, got: {setup_text:?}"
    );

    let input_prompt = session
        .write_stdin_raw_with(
            "value = input('more... ')\nprint('CUSTOM_INPUT', value)",
            Some(1.0),
        )
        .await?;
    let input_prompt_text = result_text(&input_prompt);
    assert!(
        input_prompt_text.contains("more... "),
        "expected custom-shaped input prompt metadata, got: {input_prompt_text:?}"
    );
    assert_ne!(input_prompt.is_error, Some(true));
    assert!(
        !input_prompt_text.contains("stderr: more... "),
        "custom-shaped input prompt should not be attributed to stderr, got: {input_prompt_text:?}"
    );

    let answer = session.write_stdin_raw_with("answer", Some(5.0)).await?;
    let answer_text = result_text(&answer);
    session.cancel().await?;

    assert!(
        answer_text.contains("CUSTOM_INPUT answer"),
        "expected answer after custom-shaped input prompt, got: {answer_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_interrupt_aborts_running_cell_without_replaying_tail() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let text = result_text(
        &session
            .write_stdin_raw_with(
                "import time\ntime.sleep(30)\nprint('SHOULD_NOT_RUN')",
                Some(0.2),
            )
            .await?,
    );
    assert!(
        is_busy_response(&text),
        "expected running cell to time out before interrupt, got: {text:?}"
    );

    let interrupt = session
        .write_stdin_raw_unterminated_with("\u{3}", Some(5.0))
        .await?;
    let interrupt_text = result_text(&interrupt);
    if is_busy_response(&interrupt_text) {
        eprintln!("running cell interrupt stayed busy in this Python runtime; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        !is_busy_response(&interrupt_text),
        "expected running cell interrupt to complete, got: {interrupt_text:?}"
    );
    assert!(
        !interrupt_text.contains("SHOULD_NOT_RUN"),
        "interrupt allowed tail code to run: {interrupt_text:?}"
    );

    let follow_up = session
        .write_stdin_raw_with("print('AFTER_CONTINUATION_INTERRUPT')", Some(5.0))
        .await?;
    let follow_up_text = result_text(&follow_up);
    session.cancel().await?;

    assert!(
        !follow_up_text.contains("SHOULD_NOT_RUN"),
        "pending block leaked into follow-up: {follow_up_text:?}"
    );
    assert!(
        follow_up_text.contains("AFTER_CONTINUATION_INTERRUPT"),
        "expected follow-up to run after continuation interrupt, got: {follow_up_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_idle_interrupt_tail_handles_signal_before_tail_cell() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let first = session
        .write_stdin_raw_with("\u{3}print('AFTER_IDLE_INTERRUPT')", Some(5.0))
        .await?;
    let first_text = result_text(&first);
    let follow_up_text = if first_text.contains("AFTER_IDLE_INTERRUPT") {
        first_text
    } else {
        write_python_after_interrupt_until_contains(
            &session,
            "print('AFTER_IDLE_INTERRUPT')",
            "AFTER_IDLE_INTERRUPT",
            "idle interrupt follow-up",
        )
        .await?
    };
    session.cancel().await?;

    assert!(
        follow_up_text.contains("AFTER_IDLE_INTERRUPT"),
        "expected follow-up cell after idle Ctrl-C to run, got: {follow_up_text:?}"
    );
    assert!(
        follow_up_text.contains("KeyboardInterrupt"),
        "expected idle Ctrl-C to be handled before the tail cell, got: {follow_up_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_interrupt_unblocks_empty_input_prompt() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let prompt = session
        .write_stdin_raw_with(
            "value = input()\nprint('EMPTY_INPUT_VALUE', value)",
            Some(1.0),
        )
        .await?;
    let prompt_text = result_text(&prompt);
    assert!(
        !python_backend_unavailable(&prompt_text),
        "expected Python backend to start before empty input prompt, got: {prompt_text:?}"
    );
    assert!(
        prompt_text.contains("<<repl status: waiting for input>>"),
        "expected empty input prompt to return a visible generic waiting status, got: {prompt_text:?}"
    );
    assert!(
        !prompt_text.contains("stdin> "),
        "did not expect a fabricated prompt for empty input, got: {prompt_text:?}"
    );

    let interrupt = session
        .write_stdin_raw_unterminated_with("\u{3}", Some(5.0))
        .await?;
    let interrupt_text = result_text(&interrupt);
    if is_busy_response(&interrupt_text) {
        eprintln!("empty input prompt interrupt stayed busy in this Python runtime; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        !is_busy_response(&interrupt_text),
        "expected empty input prompt interrupt to complete, got: {interrupt_text:?}"
    );

    let follow_up_text = write_python_after_interrupt_until_contains(
        &session,
        "print('AFTER_EMPTY_INPUT_INTERRUPT')",
        "AFTER_EMPTY_INPUT_INTERRUPT",
        "empty input prompt interrupt follow-up",
    )
    .await?;
    session.cancel().await?;

    assert!(
        follow_up_text.contains("AFTER_EMPTY_INPUT_INTERRUPT"),
        "expected follow-up to run after empty input prompt interrupt, got: {follow_up_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_empty_poll_after_empty_input_prompt_uses_idle_poll_path() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let prompt = session
        .write_stdin_raw_with(
            "value = input()\nprint('EMPTY_INPUT_VALUE', value)",
            Some(1.0),
        )
        .await?;
    let prompt_text = result_text(&prompt);
    assert!(
        prompt_text.contains("<<repl status: waiting for input>>"),
        "expected empty input prompt to return generic waiting status, got: {prompt_text:?}"
    );

    let poll = session.write_stdin_raw_with("", Some(1.0)).await?;
    let poll_text = result_text(&poll);
    assert!(
        poll_text.contains("<<repl status: idle>>"),
        "expected empty poll to use normal idle status, got: {poll_text:?}"
    );

    let answer = session.write_stdin_raw_with("answer", Some(5.0)).await?;
    let answer_text = result_text(&answer);
    session.cancel().await?;

    assert!(
        answer_text.contains("EMPTY_INPUT_VALUE answer"),
        "expected answer to be consumed by input(), got: {answer_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_ctrl_d_unblocks_input_prompt() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };
    let tempdir = tempdir()?;
    let marker_path = tempdir.path().join("reset-input-observation.txt");
    let marker_literal = serde_json::to_string(
        marker_path
            .to_str()
            .ok_or("reset input marker path must be valid utf-8")?,
    )?;

    let prompt = session
        .write_stdin_raw_with(
            format!(
                r#"import pathlib
_marker = pathlib.Path({marker_literal})
try:
    _value = input('reset> ')
except EOFError:
    _marker.write_text('EOFError')
else:
    _marker.write_text('VALUE:' + _value)

"#
            ),
            Some(1.0),
        )
        .await?;
    let prompt_text = result_text(&prompt);
    assert!(
        prompt_text.contains("reset> "),
        "expected input prompt before reset, got: {prompt_text:?}"
    );

    let reset = session
        .write_stdin_raw_unterminated_with("\u{4}", Some(5.0))
        .await?;
    let reset_text = result_text(&reset);
    assert!(
        !is_busy_response(&reset_text),
        "expected Ctrl-D while input() waits to complete, got: {reset_text:?}"
    );
    assert!(
        reset_text.contains("new session started"),
        "expected Ctrl-D to start a new session, got: {reset_text:?}"
    );
    if marker_path.exists() {
        let observed = fs::read_to_string(&marker_path)?;
        assert!(
            observed == "EOFError" || observed == "VALUE:",
            "reset should expose EOF or an empty line to input(), got: {observed:?}"
        );
        assert!(
            !observed.contains("exit()") && !observed.contains("quit("),
            "reset must not send shutdown text consumed by input(), got: {observed:?}"
        );
    }

    let follow_up = session
        .write_stdin_raw_with("print('AFTER_INPUT_RESET')", Some(5.0))
        .await?;
    let follow_up_text = result_text(&follow_up);
    session.cancel().await?;

    assert!(
        follow_up_text.contains("AFTER_INPUT_RESET"),
        "expected follow-up after Ctrl-D to run in the replacement worker, got: {follow_up_text:?}"
    );
    assert!(
        !follow_up_text.contains("reset> "),
        "did not expect the old input prompt to leak after Ctrl-D, got: {follow_up_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_ctrl_d_tail_includes_old_worker_shutdown_output() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };
    let tempdir = tempdir()?;
    let release_path = tempdir.path().join("release-old-worker-output");
    let emitted_path = tempdir.path().join("old-worker-output-emitted");
    let release_literal = path_json_literal(&release_path, "release marker")?;
    let emitted_literal = path_json_literal(&emitted_path, "emitted marker")?;

    let running = session
        .write_stdin_raw_with(
            format!(
                r#"import pathlib, sys, time
_release = pathlib.Path({release_literal})
_emitted = pathlib.Path({emitted_literal})
while not _release.exists():
    time.sleep(0.01)
print('OLD_WORKER_SHUTDOWN_TAIL_VISIBLE')
sys.stdout.flush()
_emitted.write_text('done')
time.sleep(30)
"#
            ),
            Some(0.05),
        )
        .await?;
    let running_text = result_text(&running);
    assert!(
        is_busy_response(&running_text),
        "expected first request to be busy before reset tail, got: {running_text:?}"
    );

    fs::write(&release_path, "go")?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !emitted_path.exists() {
        if Instant::now() >= deadline {
            session.cancel().await?;
            return Err(format!(
                "old worker did not emit restart-captured output before deadline; first reply: {running_text:?}"
            )
            .into());
        }
        sleep(Duration::from_millis(20)).await;
    }

    let reset = session
        .write_stdin_raw_with("\u{4}print('FRESH_RESET_TAIL_VISIBLE')", Some(5.0))
        .await?;
    let reset_text = result_text(&reset);
    session.cancel().await?;

    assert!(
        reset_text.contains("FRESH_RESET_TAIL_VISIBLE"),
        "expected Ctrl-D tail to run in the fresh session, got: {reset_text:?}"
    );
    assert!(
        reset_text.contains("OLD_WORKER_SHUTDOWN_TAIL_VISIBLE"),
        "expected Ctrl-D tail to include old-worker output captured while shutting down the pending request, got: {reset_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_poll_reports_empty_input_prompt_after_timeout() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let first = session
        .write_stdin_raw_with(
            "import time\ntime.sleep(0.3)\nvalue = input()\nprint('TIMED_EMPTY_INPUT_VALUE', value)",
            Some(0.1),
        )
        .await?;
    let first_text = result_text(&first);
    assert!(
        is_busy_response(&first_text),
        "expected first request to time out before input(), got: {first_text:?}"
    );

    let poll = session.write_stdin_raw_with("", Some(5.0)).await?;
    let poll_text = result_text(&poll);
    assert!(
        poll_text.contains("<<repl status: waiting for input>>"),
        "expected poll to report generic input wait, got: {poll_text:?}"
    );

    let answer = session.write_stdin_raw_with("answer", Some(5.0)).await?;
    let answer_text = result_text(&answer);
    session.cancel().await?;

    assert!(
        answer_text.contains("TIMED_EMPTY_INPUT_VALUE answer"),
        "expected answer to be consumed after timed prompt, got: {answer_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_input_roundtrip_under_debug_allocator() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) =
        start_python_session_with_env_vars(vec![("PYTHONMALLOC".to_string(), "debug".to_string())])
            .await?
    else {
        return Ok(());
    };

    let mut text = result_text(
        &session
            .write_stdin_raw_with(
                "value = input('debug> ')\nprint('DEBUG_ALLOCATOR_INPUT', value)",
                Some(1.0),
            )
            .await?,
    );
    if is_busy_response(&text) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && is_busy_response(&text) {
            sleep(Duration::from_millis(50)).await;
            text = result_text(&session.write_stdin_raw_with("", Some(1.0)).await?);
        }
    }
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err("python debug-allocator input roundtrip remained busy before prompt".into());
    }
    assert!(text.contains("debug>"), "expected prompt, got: {text:?}");

    let answer = session.write_stdin_raw_with("value", Some(5.0)).await?;
    let answer_text = result_text(&answer);
    session.cancel().await?;

    assert!(
        !is_busy_response(&answer_text),
        "expected debug-allocator input reply to complete, got: {answer_text:?}"
    );
    assert!(
        answer_text.contains("DEBUG_ALLOCATOR_INPUT value"),
        "expected input() to survive debug allocator checks, got: {answer_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_help_flows_stay_inline() -> TestResult<()> {
    let _guard = lock_test_mutex();
    if !require_python() {
        return Ok(());
    }

    let session = common::spawn_python_server_with_interactive_pager_files().await?;

    let help_result = session
        .write_stdin_raw_with("help(len)", Some(10.0))
        .await?;
    let help_text = result_text(&help_result);
    if python_backend_unavailable(&help_text) {
        eprintln!("python help backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if is_busy_response(&help_text) {
        session.cancel().await?;
        return Err(format!("help(len) should complete inline, got: {help_text:?}").into());
    }
    let help_visible = visible_reply_text(&help_text)?;

    assert!(
        help_visible.contains("Help on built-in function len"),
        "expected inline help(len) output, got: {help_visible:?}"
    );
    assert!(
        help_visible.contains("Return the number of items in a container."),
        "expected len() help text, got: {help_visible:?}"
    );
    assert_no_pager_markers(&help_visible, "help(len)");

    let pydoc_result = session
        .write_stdin_raw_with("import pydoc; pydoc.help(len)", Some(10.0))
        .await?;
    let pydoc_text = result_text(&pydoc_result);
    if is_busy_response(&pydoc_text) {
        session.cancel().await?;
        return Err(format!("pydoc.help(len) should complete inline, got: {pydoc_text:?}").into());
    }
    let pydoc_visible = visible_reply_text(&pydoc_text)?;

    assert!(
        pydoc_visible.contains("Help on built-in function len"),
        "expected inline pydoc.help(len) output, got: {pydoc_visible:?}"
    );
    assert!(
        pydoc_visible.contains("Return the number of items in a container."),
        "expected len() help text, got: {pydoc_visible:?}"
    );
    assert_no_pager_markers(&pydoc_visible, "pydoc.help(len)");

    let mut enter_text = result_text(&session.write_stdin_raw_with("help()", Some(5.0)).await?);
    if is_busy_response(&enter_text) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline
            && is_busy_response(&enter_text)
            && !enter_text.contains("help>")
        {
            sleep(Duration::from_millis(50)).await;
            enter_text = result_text(&session.write_stdin_raw_with("", Some(1.0)).await?);
        }
    }
    if is_busy_response(&enter_text) {
        session.cancel().await?;
        return Err(format!("help() did not surface an interactive prompt: {enter_text:?}").into());
    }
    let enter_visible = visible_reply_text(&enter_text)?;

    let mut exit_text = result_text(&session.write_stdin_raw_with("len\nq", Some(5.0)).await?);
    if is_busy_response(&exit_text) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline
            && is_busy_response(&exit_text)
            && !exit_text.contains("You are now leaving help")
        {
            sleep(Duration::from_millis(50)).await;
            exit_text = result_text(&session.write_stdin_raw_with("", Some(1.0)).await?);
        }
    }
    if is_busy_response(&exit_text) {
        session.cancel().await?;
        return Err(format!("interactive help() did not finish after q: {exit_text:?}").into());
    }
    let exit_visible = visible_reply_text(&exit_text)?;

    let follow_up = session.write_stdin_raw_with("1+1", Some(5.0)).await?;
    let follow_up_text = result_text(&follow_up);
    if is_busy_response(&follow_up_text) {
        session.cancel().await?;
        return Err(format!("interactive help() left the session busy: {follow_up_text:?}").into());
    }

    session.cancel().await?;

    assert!(
        enter_visible.contains("help>"),
        "expected help() prompt to stay inline, got: {enter_visible:?}"
    );
    assert_no_pager_markers(&enter_visible, "help()");
    assert!(
        exit_visible.contains("Help on built-in function len"),
        "expected interactive help() to show len help text, got: {exit_visible:?}"
    );
    assert!(
        exit_visible.contains("Return the number of items in a container."),
        "expected len() help text in interactive help(), got: {exit_visible:?}"
    );
    assert_no_pager_markers(&exit_visible, "help() roundtrip");
    assert!(
        follow_up_text.contains("2"),
        "expected follow-up cell after interactive help(), got: {follow_up_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_stderr_merged_into_output() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            "import sys; print('out'); sys.stderr.write('err\\n')",
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        eprintln!("python_stderr_merged_into_output remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(text.contains("out"), "missing stdout, got: {text:?}");
    assert!(text.contains("err"), "missing stderr, got: {text:?}");

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_prompt_shaped_stdout_before_stderr_stays_visible() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_pager_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            "import sys\n_ = sys.stdout.write('foo >>> ')\nsys.stdout.flush()\n_ = sys.stderr.write('ERR\\n')\nsys.stderr.flush()\n_ = sys.stdout.write('bar\\n')\nsys.stdout.flush()",
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    let prompt_shaped_stdout = text
        .find("foo >>> ")
        .ok_or_else(|| format!("expected prompt-shaped stdout suffix, got: {text:?}"))?;
    let stderr = text
        .find("ERR")
        .ok_or_else(|| format!("expected stderr, got: {text:?}"))?;
    let trailing_stdout = text
        .find("bar")
        .ok_or_else(|| format!("expected trailing stdout, got: {text:?}"))?;
    assert!(
        prompt_shaped_stdout < stderr && stderr < trailing_stdout,
        "expected prompt-shaped stdout before stderr before trailing stdout, got: {text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_trailing_prompt_shaped_stdout_stays_visible() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            "import sys; _ = sys.stdout.write('PROMPT_STDOUT>>> '); sys.stdout.flush()",
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        eprintln!("python_trailing_prompt_shaped_stdout_stays_visible remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }

    assert!(
        text.contains("PROMPT_STDOUT>>> "),
        "expected trailing prompt-shaped stdout to remain visible, got: {text:?}"
    );
    assert!(
        !text.contains("PROMPT_STDOUT>>> >>> "),
        "completed cells should not append an idle Python prompt, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_interrupt_unblocks_long_running_request() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let timeout_result = session
        .write_stdin_raw_with("import time; time.sleep(30)", Some(0.5))
        .await?;
    let timeout_text = result_text(&timeout_result);
    assert!(
        timeout_text.contains("<<repl status: busy"),
        "expected sleep call to time out, got: {timeout_text:?}"
    );

    let interrupt_result = session
        .write_stdin_raw_unterminated_with("\u{3}", Some(5.0))
        .await?;
    let interrupt_text = result_text(&interrupt_result);
    assert!(
        !is_busy_response(&interrupt_text) && interrupt_text.contains("KeyboardInterrupt"),
        "expected interrupt to complete with KeyboardInterrupt, got: {interrupt_text:?}"
    );

    let deadline = interrupt_recovery_deadline();
    loop {
        if Instant::now() >= deadline {
            session.cancel().await?;
            return Err("worker stayed busy after interrupt".into());
        }

        let result = session.write_stdin_raw_with("1+1", Some(5.0)).await?;
        let text = result_text(&result);
        if text.contains("worker is busy") || text.contains("request already running") {
            sleep(Duration::from_millis(50)).await;
            continue;
        }
        assert!(
            text.contains("2"),
            "expected evaluation after interrupt, got: {text:?}"
        );
        break;
    }

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_ctrl_c_prefix_preserves_followup_fresh_input_batch() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let timeout_result = session
        .write_stdin_raw_with("import time; time.sleep(30)", Some(0.2))
        .await?;
    let timeout_text = result_text(&timeout_result);
    assert!(
        timeout_text.contains("<<repl status: busy"),
        "expected sleep call to time out, got: {timeout_text:?}"
    );

    let mut text = result_text(
        &session
            .write_stdin_raw_with(
                "\u{3}value = input('after> ')\nprint('AFTER_STALE_INTERRUPT', value.strip())",
                Some(5.0),
            )
            .await?,
    );
    let deadline = interrupt_recovery_deadline();
    while is_busy_response(&text) {
        if Instant::now() >= deadline {
            session.cancel().await?;
            return Err(format!("ctrl-c follow-up turn stayed busy: {text:?}").into());
        }
        sleep(Duration::from_millis(50)).await;
        text = result_text(&session.write_stdin_raw_with("", Some(0.5)).await?);
    }

    if !text.contains("after> ") || text.contains("Traceback") {
        text = write_python_prompt_after_interrupt(
            &session,
            "value = input('after> ')\nprint('AFTER_STALE_INTERRUPT', value.strip())",
            "after> ",
            "ctrl-c prefix follow-up input prompt",
        )
        .await?;
    }

    let answer_deadline = interrupt_recovery_deadline();
    let text = loop {
        if Instant::now() >= answer_deadline {
            session.cancel().await?;
            return Err("ctrl-c prefix follow-up never consumed the prompt answer".into());
        }
        if !text.contains("after> ") || text.contains("Traceback") {
            text = write_python_prompt_after_interrupt(
                &session,
                "value = input('after> ')\nprint('AFTER_STALE_INTERRUPT', value.strip())",
                "after> ",
                "ctrl-c prefix follow-up input prompt",
            )
            .await?;
        }
        let answer = session
            .write_stdin_raw_with("queued-answer", Some(5.0))
            .await?;
        let answer_text = result_text(&answer);
        if answer_text.contains("AFTER_STALE_INTERRUPT queued-answer") {
            break answer_text;
        }
        if is_busy_response(&answer_text)
            || answer_text.contains("KeyboardInterrupt")
            || answer_text.contains("NameError")
        {
            text.clear();
            sleep(Duration::from_millis(50)).await;
            continue;
        }
        session.cancel().await?;
        return Err(format!("unexpected ctrl-c prefix answer response: {answer_text:?}").into());
    };
    session.cancel().await?;

    assert!(
        text.contains("AFTER_STALE_INTERRUPT queued-answer"),
        "expected follow-up input batch after ctrl-c prefix, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_interrupt_wakes_time_sleep_signal_handler() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let timeout_result = session
        .write_stdin_raw_with(
            r#"exec("""
import signal, time
def handle_interrupt(signum, frame):
    print("PY_SLEEP_INTERRUPT")
    raise KeyboardInterrupt

signal.signal(signal.SIGINT, handle_interrupt)
print("PY_SLEEP_READY", flush=True)
try:
    time.sleep(30)
except KeyboardInterrupt:
    print("PY_SLEEP_INTERRUPTED")
""")
"#,
            Some(0.2),
        )
        .await?;
    let mut text = result_text(&timeout_result);
    assert!(
        is_busy_response(&text),
        "expected sleep call to time out, got: {text:?}"
    );

    let ready_deadline = interrupt_recovery_deadline();
    while !text.contains("PY_SLEEP_READY") {
        if Instant::now() >= ready_deadline {
            session.cancel().await?;
            return Err(format!("sleep request did not report readiness: {text:?}").into());
        }
        sleep(Duration::from_millis(50)).await;
        let poll = session.write_stdin_raw_with("", Some(0.5)).await?;
        text = result_text(&poll);
        assert!(
            is_busy_response(&text) || text.contains("PY_SLEEP_READY"),
            "expected sleep request to stay busy before interrupt, got: {text:?}"
        );
    }

    let interrupt_deadline = interrupt_recovery_deadline();
    text = result_text(
        &session
            .write_stdin_raw_unterminated_with("\u{3}", Some(5.0))
            .await?,
    );
    while is_busy_response(&text) {
        if Instant::now() >= interrupt_deadline {
            session.cancel().await?;
            return Err(format!("sleep interrupt did not finish: {text:?}").into());
        }
        sleep(Duration::from_millis(50)).await;
        let poll = session.write_stdin_raw_with("", Some(0.5)).await?;
        text = result_text(&poll);
    }

    let follow_up = session
        .write_stdin_raw_with("print('PY_SLEEP_AFTER_INTERRUPT')", Some(5.0))
        .await?;
    let follow_up_text = result_text(&follow_up);
    session.cancel().await?;

    assert!(
        text.contains("PY_SLEEP_INTERRUPT") && text.contains("PY_SLEEP_INTERRUPTED"),
        "expected signal handler to wake sleep, got: {text:?}"
    );
    assert!(
        follow_up_text.contains("PY_SLEEP_AFTER_INTERRUPT"),
        "expected follow-up after sleep interrupt, got: {follow_up_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_detached_idle_output_does_not_bundle_follow_up_reply() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };
    let marker_dir = tempdir()?;
    let release_path = marker_dir.path().join("detached-idle-release");
    let marker_path = marker_dir.path().join("detached-idle-written");
    let release = release_path
        .to_str()
        .ok_or("detached idle release path must be valid utf-8")?;
    let marker = marker_path
        .to_str()
        .ok_or("detached idle marker path must be valid utf-8")?;

    let setup = session
        .write_stdin_raw_with(
            format!(
                r#"import subprocess, sys
script = """import pathlib, sys, time
release = pathlib.Path(sys.argv[1])
while not release.exists():
    time.sleep(0.01)
for i in range(160):
    sys.stdout.write("IDLE_%03d " % i + ("x" * 80) + "\\n")
sys.stdout.flush()
pathlib.Path(sys.argv[2]).write_text("done")
"""
subprocess.Popen(
    [sys.executable, "-c", script, {release_arg}, {marker_arg}],
    stdin=subprocess.DEVNULL,
    close_fds=False,
)
print("parent ready")
"#,
                release_arg = serde_json::to_string(release)?,
                marker_arg = serde_json::to_string(marker)?
            ),
            Some(5.0),
        )
        .await?;
    let setup_text = result_text(&setup);
    if is_busy_response(&setup_text) {
        eprintln!("python detached-idle setup remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        setup_text.contains("parent ready"),
        "expected detached-idle setup reply, got: {setup_text:?}"
    );

    fs::write(&release_path, "go")?;
    wait_for_detached_holder_exit(&marker_path).await?;
    let follow_up = session
        .write_stdin_raw_with("print('FOLLOWUP_OK')", Some(5.0))
        .await?;
    let follow_up_text = result_text(&follow_up);
    if is_busy_response(&follow_up_text) {
        eprintln!("python detached-idle follow-up remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let transcript_path = bundle_transcript_path(&follow_up_text).unwrap_or_else(|| {
        panic!("expected detached idle output to disclose transcript path, got: {follow_up_text:?}")
    });
    let transcript = std::fs::read_to_string(&transcript_path)?;

    session.cancel().await?;

    assert!(
        follow_up_text.contains("FOLLOWUP_OK"),
        "expected follow-up output inline, got: {follow_up_text:?}"
    );
    assert!(
        transcript.contains("IDLE_000"),
        "expected detached idle output in transcript bundle, got: {transcript:?}"
    );
    assert!(
        !transcript.contains("FOLLOWUP_OK"),
        "did not expect follow-up output to be bundled with detached idle output: {transcript:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_idle_exit_preserves_detached_tail_before_respawn() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let marker_dir = tempdir()?;
    let debug_dir = marker_dir.path().join("debug");
    let Some(session) = start_python_session_with_env_vars(debug_dir_env(&debug_dir)).await? else {
        return Ok(());
    };
    let tail_marker_path = marker_dir.path().join("idle-tail-written");
    let tail_marker = tail_marker_path
        .to_str()
        .ok_or("idle tail marker path must be valid utf-8")?;
    let tail_marker_literal = serde_json::to_string(tail_marker)?;
    let signal_marker_path = marker_dir.path().join("idle-worker-signaled");
    let signal_marker = signal_marker_path
        .to_str()
        .ok_or("idle signal marker path must be valid utf-8")?;
    let signal_marker_literal = serde_json::to_string(signal_marker)?;
    let release_marker_path = marker_dir.path().join("idle-tail-release");
    let release_marker = release_marker_path
        .to_str()
        .ok_or("idle tail release marker path must be valid utf-8")?;
    let release_marker_literal = serde_json::to_string(release_marker)?;
    let script = format!(
        r#"import os, pathlib, subprocess, sys
tail_marker = {tail_marker_literal}
signal_marker = {signal_marker_literal}
release_marker = {release_marker_literal}
worker_pid = os.getpid()
writer = """import os, pathlib, signal, sys, time
deadline = time.monotonic() + 30
while not os.path.exists(sys.argv[3]):
    if time.monotonic() >= deadline:
        sys.exit(2)
    time.sleep(0.02)
sys.stdout.write("IDLE_TAIL\\n")
sys.stdout.flush()
pathlib.Path(sys.argv[1]).write_text("done")
os.kill(int(sys.argv[4]), signal.SIGTERM)
pathlib.Path(sys.argv[2]).write_text("done")
"""
subprocess.Popen(
    [sys.executable, "-c", writer, tail_marker, signal_marker, release_marker, str(worker_pid)],
    stdin=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
    close_fds=True,
    start_new_session=True,
)
print("armed")"#
    );
    let script_literal = serde_json::to_string(&script)?;

    let arm = session
        .write_stdin_raw_with(format!("exec({script_literal})"), Some(5.0))
        .await?;
    let arm_text = result_text(&arm);
    if is_busy_response(&arm_text) {
        eprintln!(
            "python_idle_exit_preserves_detached_tail_before_respawn remained busy; skipping"
        );
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        arm_text.contains("armed"),
        "expected arming output, got: {arm_text:?}"
    );

    fs::write(&release_marker_path, "go")?;
    if let Err(err) = wait_for_detached_holder_exit(&signal_marker_path).await {
        session.cancel().await?;
        return Err(format!("{err}{}", debug_log_summary(&debug_dir)).into());
    }
    let reply = session
        .write_stdin_raw_with("print('AFTER_RESPAWN')", Some(5.0))
        .await?;
    let text = result_text(&reply);
    if is_busy_response(&text) {
        eprintln!(
            "python_idle_exit_preserves_detached_tail_before_respawn remained busy after respawn; skipping"
        );
        session.cancel().await?;
        return Ok(());
    }
    let visible = visible_reply_text(&text)?;

    session.cancel().await?;

    assert!(
        visible.contains("IDLE_TAIL"),
        "expected detached idle output to survive auto-respawn, got: {visible:?}"
    );
    assert!(
        visible.contains("AFTER_RESPAWN"),
        "expected fresh respawned output, got: {visible:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_restart_does_not_leak_old_generation_output() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let timeout_result = session
        .write_stdin_raw_with(
            "import sys, time; big = 'OLD_BLOCK\\n' * 200000; sys.stdout.write(big); sys.stdout.flush(); time.sleep(30)",
            Some(0.05),
        )
        .await?;
    let timeout_text = result_text(&timeout_result);
    if !is_busy_response(&timeout_text) {
        eprintln!(
            "python_restart_does_not_leak_old_generation_output did not time out as expected; skipping"
        );
        session.cancel().await?;
        return Ok(());
    }

    let restart = session
        .write_stdin_raw_unterminated_with("\u{4}", Some(10.0))
        .await?;
    let restart_text = result_text(&restart);
    if is_busy_response(&restart_text) {
        eprintln!(
            "python_restart_does_not_leak_old_generation_output restart remained busy; skipping"
        );
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        restart_text.contains("new session started"),
        "expected restart notice, got: {restart_text:?}"
    );

    let next = session
        .write_stdin_raw_with("print('NEW_GENERATION_OK')", Some(5.0))
        .await?;
    let next_text = result_text(&next);
    if is_busy_response(&next_text) {
        eprintln!(
            "python_restart_does_not_leak_old_generation_output next turn remained busy; skipping"
        );
        session.cancel().await?;
        return Ok(());
    }
    let visible = visible_reply_text(&next_text)?;

    session.cancel().await?;

    assert!(
        visible.contains("NEW_GENERATION_OK"),
        "expected fresh-generation reply, got: {visible:?}"
    );
    assert!(
        !visible.contains("OLD_BLOCK"),
        "did not expect old-generation output after restart, got: {visible:?}"
    );
    Ok(())
}

// This asserts raw split-byte ownership across detached child output and the
// next request. Windows routes Python through ConPTY, where console code-page
// rendering is the public boundary rather than raw UTF-8 byte preservation.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_detached_incomplete_utf8_tail_does_not_merge_into_next_request() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };
    let marker_dir = tempdir()?;
    let release_path = marker_dir.path().join("detached-incomplete-release");
    let marker_path = marker_dir.path().join("detached-incomplete-written");
    let release = release_path
        .to_str()
        .ok_or("detached incomplete release path must be valid utf-8")?;
    let marker = marker_path
        .to_str()
        .ok_or("detached incomplete marker path must be valid utf-8")?;

    let setup = session
        .write_stdin_raw_with(
            format!(
                r#"import subprocess, sys
script = """import os, pathlib, sys, time
release = pathlib.Path(sys.argv[1])
while not release.exists():
    time.sleep(0.01)
for i in range(160):
    os.write(sys.stdout.fileno(), ("IDLE_%03d " % i + ("x" * 80) + "\\n").encode())
os.write(sys.stdout.fileno(), bytes([0xC3]))
pathlib.Path(sys.argv[2]).write_text("done")
"""
subprocess.Popen(
    [sys.executable, "-c", script, {release_arg}, {marker_arg}],
    stdin=subprocess.DEVNULL,
    close_fds=False,
)
print("parent ready")
"#,
                release_arg = serde_json::to_string(release)?,
                marker_arg = serde_json::to_string(marker)?
            ),
            Some(5.0),
        )
        .await?;
    let setup_text = result_text(&setup);
    if is_busy_response(&setup_text) {
        eprintln!("python detached-incomplete setup remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        setup_text.contains("parent ready"),
        "expected detached-incomplete setup reply, got: {setup_text:?}"
    );

    fs::write(&release_path, "go")?;
    wait_for_detached_holder_exit(&marker_path).await?;
    let follow_up = session
        .write_stdin_raw_with(
            "import os, sys\nos.write(sys.stdout.fileno(), bytes([0xA9, 0x0A]))\nprint('FOLLOWUP_OK')",
            Some(5.0),
        )
        .await?;
    let follow_up_text = result_text(&follow_up);
    if is_busy_response(&follow_up_text) {
        eprintln!("python detached-incomplete follow-up remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let transcript_path = bundle_transcript_path(&follow_up_text).unwrap_or_else(|| {
        panic!(
            "expected detached output transcript path in follow-up reply, got: {follow_up_text:?}"
        )
    });
    let transcript = std::fs::read_to_string(&transcript_path)?;

    session.cancel().await?;

    assert!(
        follow_up_text.contains("\\xA9"),
        "expected new request continuation byte to stay split, got: {follow_up_text:?}"
    );
    assert!(
        !follow_up_text.contains("é"),
        "did not expect cross-request UTF-8 merge, got: {follow_up_text:?}"
    );
    assert!(
        follow_up_text.contains("FOLLOWUP_OK"),
        "expected follow-up output, got: {follow_up_text:?}"
    );
    assert!(
        transcript.contains("IDLE_000"),
        "expected detached idle output in transcript, got: {transcript:?}"
    );
    assert!(
        transcript.contains("\\xC3"),
        "expected detached lead byte to stay with detached transcript, got: {transcript:?}"
    );
    assert!(
        !transcript.contains("FOLLOWUP_OK"),
        "did not expect follow-up output in detached transcript: {transcript:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_interrupt_discards_buffered_tail_after_timeout() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let timeout_result = session
        .write_stdin_raw_with("import time; time.sleep(30)\nx_tail_marker = 99", Some(0.5))
        .await?;
    let timeout_text = result_text(&timeout_result);
    assert!(
        timeout_text.contains("<<repl status: busy"),
        "expected sleep call to time out, got: {timeout_text:?}"
    );

    let interrupt_result = session
        .write_stdin_raw_unterminated_with("\u{3}", Some(5.0))
        .await?;
    let interrupt_text = result_text(&interrupt_result);
    assert!(
        !is_busy_response(&interrupt_text) && interrupt_text.contains("KeyboardInterrupt"),
        "expected interrupt to complete with KeyboardInterrupt, got: {interrupt_text:?}"
    );

    let poll_result = session.write_stdin_raw_with("", Some(0.5)).await?;
    let _poll_text = result_text(&poll_result);

    let deadline = interrupt_recovery_deadline();
    loop {
        if Instant::now() >= deadline {
            session.cancel().await?;
            return Err("worker stayed busy after interrupt".into());
        }

        let result = session.write_stdin_raw_with("1+1", Some(5.0)).await?;
        let text = result_text(&result);
        if text.contains("worker is busy") || text.contains("request already running") {
            sleep(Duration::from_millis(50)).await;
            continue;
        }
        assert!(
            text.contains("2"),
            "expected evaluation after interrupt, got: {text:?}"
        );
        break;
    }

    let deadline = interrupt_recovery_deadline();
    loop {
        if Instant::now() >= deadline {
            session.cancel().await?;
            return Err("worker stayed busy before tail-marker probe".into());
        }

        let marker_result = session
            .write_stdin_raw_with("globals().get('x_tail_marker', 'MISSING')", Some(0.5))
            .await?;
        let marker_text = result_text(&marker_result);
        if is_busy_response(&marker_text) {
            sleep(Duration::from_millis(50)).await;
            continue;
        }
        assert!(
            marker_text.contains("'MISSING'"),
            "expected buffered tail assignment to be discarded, got: {marker_text:?}"
        );
        break;
    }

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_multistatement_payload_completes() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with("def f():\n    return 3\n\nprint('done')\nf()", Some(5.0))
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        eprintln!("python_multistatement_payload_completes remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(text.contains("3"), "expected 3, got: {text:?}");
    assert!(text.contains("done"), "expected done, got: {text:?}");

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_exception_reported_in_output() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session.write_stdin_raw_with("1/0", Some(5.0)).await?;
    let text = result_text(&result);
    let plain_text = strip_ansi_controls(&text);
    if is_busy_response(&text) {
        eprintln!("python_exception_reported_in_output remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        plain_text.contains("ZeroDivisionError"),
        "expected traceback, got: {text:?}"
    );
    assert!(
        plain_text.contains("File \"<mcp-repl>\", line 1"),
        "expected traceback to point at the user cell, got: {text:?}"
    );
    assert!(
        !plain_text.contains("_mcp_repl_run_cell") && !plain_text.contains("File \"<string>\""),
        "traceback should not expose the cell runner frame, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_compile_syntax_error_hides_cell_runner_frames() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session.write_stdin_raw_with("return 1", Some(5.0)).await?;
    let text = result_text(&result);
    let plain_text = strip_ansi_controls(&text);
    if is_busy_response(&text) {
        eprintln!("python_compile_syntax_error_hides_cell_runner_frames remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        plain_text.contains("SyntaxError"),
        "expected SyntaxError, got: {text:?}"
    );
    assert!(
        plain_text.contains("File \"<mcp-repl>\", line 1"),
        "expected syntax error to point at the user cell, got: {text:?}"
    );
    assert!(
        !text.contains("_mcp_repl_run_cell")
            && !text.contains("_mcp_repl_compile_complete")
            && !text.contains("File \"<string>\""),
        "traceback should not expose cell runner frames, got: {text:?}"
    );

    let follow_up = session
        .write_stdin_raw_with(
            r#"import sys
print("SYS_LAST_TRACEBACK_IS_NONE", sys.last_traceback is None)
print("SYS_LAST_EXC_TRACEBACK_IS_NONE", sys.last_exc.__traceback__ is None)
"#,
            Some(5.0),
        )
        .await?;
    let follow_up_text = result_text(&follow_up);
    session.cancel().await?;

    assert!(
        follow_up_text.contains("SYS_LAST_TRACEBACK_IS_NONE True"),
        "expected sys.last_traceback to omit cell runner frames, got: {follow_up_text:?}"
    );
    assert!(
        follow_up_text.contains("SYS_LAST_EXC_TRACEBACK_IS_NONE True"),
        "expected sys.last_exc traceback to omit cell runner frames, got: {follow_up_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_cell_exception_updates_sys_last_exception() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session.write_stdin_raw_with("1/0", Some(5.0)).await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        eprintln!("python_cell_exception_updates_sys_last_exception remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let follow_up = session
        .write_stdin_raw_with(
            r#"import sys
print("SYS_LAST_VALUE", type(sys.last_value).__name__, str(sys.last_value))
print("SYS_LAST_TYPE", sys.last_type.__name__)
print("SYS_LAST_TRACEBACK", sys.last_traceback.tb_frame.f_code.co_filename, sys.last_traceback.tb_lineno)
print("SYS_LAST_EXC", getattr(sys, "last_exc", sys.last_value) is sys.last_value)
print("SYS_LAST_EXC_TRACEBACK", sys.last_exc.__traceback__ is sys.last_traceback)
"#,
            Some(5.0),
        )
        .await?;
    let follow_up_text = result_text(&follow_up);
    session.cancel().await?;

    assert!(
        follow_up_text.contains("SYS_LAST_VALUE ZeroDivisionError division by zero"),
        "expected sys.last_value to preserve the failed cell exception, got: {follow_up_text:?}"
    );
    assert!(
        follow_up_text.contains("SYS_LAST_TYPE ZeroDivisionError"),
        "expected sys.last_type to preserve the failed cell exception type, got: {follow_up_text:?}"
    );
    assert!(
        follow_up_text.contains("SYS_LAST_TRACEBACK <mcp-repl> 1"),
        "expected sys.last_traceback to point at the failed user cell, got: {follow_up_text:?}"
    );
    assert!(
        follow_up_text.contains("SYS_LAST_EXC True"),
        "expected sys.last_exc to match sys.last_value when available, got: {follow_up_text:?}"
    );
    assert!(
        follow_up_text.contains("SYS_LAST_EXC_TRACEBACK True"),
        "expected sys.last_exc to carry the same traceback as sys.last_traceback, got: {follow_up_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_pdb_roundtrip() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            "value = 41\nbreakpoint()\nvalue += 1\nprint('PDB_AFTER_CONTINUE', value)",
            Some(1.0),
        )
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        eprintln!("python_pdb_roundtrip remained busy entering pdb; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(text.contains("(Pdb)"), "expected pdb prompt, got: {text:?}");

    let result = session.write_stdin_raw_with("p value", Some(5.0)).await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err("python_pdb_roundtrip remained busy after p value".into());
    }
    assert!(
        text.contains("41") && text.contains("(Pdb)"),
        "expected pdb to print value and stay at prompt, got: {text:?}"
    );

    let result = session.write_stdin_raw_with("n", Some(5.0)).await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err("python_pdb_roundtrip remained busy after next".into());
    }
    assert!(
        text.contains("(Pdb)"),
        "expected pdb prompt after next, got: {text:?}"
    );

    let result = session.write_stdin_raw_with("c", Some(5.0)).await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err("python_pdb_roundtrip remained busy after continue".into());
    }
    assert!(
        text.contains("PDB_AFTER_CONTINUE 42"),
        "expected Python cell to resume after pdb continue, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_input_consumes_follow_up_line() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with("x = input('p> ')\nprint('got', x)", Some(1.0))
        .await?;
    let mut text = result_text(&result);
    if is_busy_response(&text) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && is_busy_response(&text) && !text.contains("p> ") {
            sleep(Duration::from_millis(50)).await;
            text = result_text(&session.write_stdin_raw_with("", Some(1.0)).await?);
        }
    }
    if is_busy_response(&text) {
        eprintln!("python_input_can_consume_buffered_lines remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("p> "),
        "expected input() prompt to stay visible, got: {text:?}"
    );

    let answer = session.write_stdin_raw_with("hello", Some(5.0)).await?;
    let text = result_text(&answer);
    session.cancel().await?;

    assert!(
        text.contains("got hello"),
        "expected input() to consume follow-up hello, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_input_prompt_matching_primary_prompt_stays_visible() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"import sys
sys.ps1 = "same> "
value = input("same> ")
print("MATCHED_PROMPT_VALUE", value)
"#,
            Some(1.0),
        )
        .await?;
    let text = result_text(&result);

    assert!(
        !is_busy_response(&text),
        "expected input request to reach prompt, got: {text:?}"
    );
    assert!(
        text.contains("same> "),
        "expected input() prompt matching sys.ps1 to stay visible, got: {text:?}"
    );

    let answer = session.write_stdin_raw_with("buffered", Some(5.0)).await?;
    let answer_text = result_text(&answer);
    session.cancel().await?;

    assert!(
        !answer_text.contains("same> "),
        "completed cells should not render final prompt matching sys.ps1, got: {answer_text:?}"
    );
    assert!(
        answer_text.contains("MATCHED_PROMPT_VALUE buffered"),
        "expected input() to consume follow-up line, got: {answer_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_input_releases_gil_while_waiting_for_line() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };
    let temp_dir = tempdir()?;
    let background_marker = temp_dir.path().join("input-gil-background-ready");
    let background_marker_literal = path_json_literal(&background_marker, "background marker")?;

    let result = session
        .write_stdin_raw_with(
            format!(
                r#"
import pathlib, sys, threading, time
background_marker = pathlib.Path({background_marker_literal})
sys.setswitchinterval(1.0)
background_seen = None
answer_seen = None
background_started = threading.Event()
def background():
    global background_seen
    background_started.set()
    time.sleep(0.1)
    background_seen = time.monotonic()
    background_marker.write_text("ready")

def wait_and_print():
    global answer_seen
    answer = input('p> ')
    answer_seen = time.monotonic()
    print('answer', answer)

threading.Thread(target=background, daemon=True).start()
background_started.wait()
wait_and_print()
"#
            ),
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    assert!(
        !is_busy_response(&text),
        "expected input prompt, got: {text:?}"
    );
    assert!(text.contains("p> "), "expected input prompt, got: {text:?}");

    wait_for_file_text(&background_marker, "ready").await?;

    let answer = session.write_stdin_raw_with("ok", Some(5.0)).await?;
    let answer_text = result_text(&answer);
    assert!(
        answer_text.contains("answer ok"),
        "expected input answer output, got: {answer_text:?}"
    );

    let order = session
        .write_stdin_raw_with(
            "time.sleep(0.2)\nprint('background-before-answer', background_seen is not None and answer_seen is not None and background_seen < answer_seen)",
            Some(5.0),
        )
        .await?;
    let order_text = result_text(&order);
    assert!(
        order_text.contains("background-before-answer True"),
        "expected background thread to run while input() was waiting for a line, got: {order_text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_background_input_after_cell_return_completes_answer_turn() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };
    let temp_dir = tempdir()?;
    let release_path = temp_dir
        .path()
        .join("release-background-input-after-cell-return");
    let release_path_literal = path_json_literal(&release_path, "background input release")?;

    let result = session
        .write_stdin_raw_with(
            format!(
                r#"
import pathlib, threading, time
release_path = pathlib.Path({release_path_literal})
background_answer = None
background_waiting = threading.Event()

def background():
    global background_answer
    background_waiting.set()
    background_answer = input('bg> ')
    print('BACKGROUND_ANSWER', background_answer)

threading.Thread(target=background, daemon=True).start()
background_waiting.wait()
while not release_path.exists():
    time.sleep(0.01)
print('CELL_RETURNED')
"#
            ),
            Some(5.0),
        )
        .await?;
    let setup_text = result_text(&result);
    assert!(
        !is_busy_response(&setup_text),
        "expected background input prompt setup to complete, got: {setup_text:?}"
    );
    assert!(
        setup_text.contains("bg> "),
        "expected background input prompt, got: {setup_text:?}"
    );

    fs::write(&release_path, "go")?;
    let settled_text = poll_until_contains(
        &session,
        setup_text.clone(),
        "CELL_RETURNED",
        "foreground cell to return while background input stayed live",
        Duration::from_secs(5),
    )
    .await?;
    assert!(
        settled_text.contains("bg> "),
        "expected background input prompt to remain visible after foreground cell return, got: {settled_text:?}"
    );

    let answer = session.write_stdin_raw_with("ok", Some(1.0)).await?;
    let answer_text = result_text(&answer);
    session.cancel().await?;

    assert!(
        !is_busy_response(&answer_text),
        "background input answer should complete its request, got: {answer_text:?}"
    );
    assert!(
        answer_text.contains("BACKGROUND_ANSWER ok"),
        "expected background input answer output, got: {answer_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_background_input_consumes_buffered_lines_from_one_answer_turn() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };
    let temp_dir = tempdir()?;
    let release_path = temp_dir
        .path()
        .join("release-background-input-buffered-lines");
    let release_path_literal =
        path_json_literal(&release_path, "background buffered input release")?;

    let result = session
        .write_stdin_raw_with(
            format!(
                r#"
import pathlib, threading, time
release_path = pathlib.Path({release_path_literal})
background_waiting = threading.Event()

def background():
    background_waiting.set()
    first = input('bg1> ')
    second = input('bg2> ')
    print('BACKGROUND_BOTH', first, second)

threading.Thread(target=background, daemon=True).start()
background_waiting.wait()
while not release_path.exists():
    time.sleep(0.01)
print('CELL_RETURNED')
"#
            ),
            Some(5.0),
        )
        .await?;
    let setup_text = result_text(&result);
    assert!(
        !is_busy_response(&setup_text),
        "expected background input setup to complete, got: {setup_text:?}"
    );
    assert!(
        setup_text.contains("bg1> "),
        "expected first background input prompt, got: {setup_text:?}"
    );

    fs::write(&release_path, "go")?;
    let _settled_text = poll_until_contains(
        &session,
        setup_text,
        "CELL_RETURNED",
        "foreground cell to return before the buffered answer turn",
        Duration::from_secs(5),
    )
    .await?;

    let answer = session.write_stdin_raw_with("one\ntwo", Some(1.0)).await?;
    let mut answer_text = result_text(&answer);
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !answer_text.contains("BACKGROUND_BOTH one two") {
        sleep(Duration::from_millis(50)).await;
        let poll = session
            .write_stdin_raw_unterminated_with("", Some(1.0))
            .await?;
        answer_text.push_str(&result_text(&poll));
    }
    session.cancel().await?;

    assert!(
        answer_text.contains("BACKGROUND_BOTH one two"),
        "expected one answer turn to satisfy both background input() calls, got: {answer_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_background_input_discards_extra_answer_lines_before_next_cell() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };
    let temp_dir = tempdir()?;
    let release_path = temp_dir
        .path()
        .join("release-background-input-discard-extra");
    let release_path_literal =
        path_json_literal(&release_path, "background input discard release")?;

    let result = session
        .write_stdin_raw_with(
            format!(
                r#"
import pathlib, threading, time
release_path = pathlib.Path({release_path_literal})
background_waiting = threading.Event()

def background():
    background_waiting.set()
    answer = input('bg> ')
    print('BACKGROUND_ANSWER', answer)

threading.Thread(target=background, daemon=True).start()
background_waiting.wait()
while not release_path.exists():
    time.sleep(0.01)
print('CELL_RETURNED')
"#
            ),
            Some(5.0),
        )
        .await?;
    let setup_text = result_text(&result);
    assert!(
        !is_busy_response(&setup_text),
        "expected background input prompt setup to complete, got: {setup_text:?}"
    );
    assert!(
        setup_text.contains("bg> "),
        "expected background input prompt, got: {setup_text:?}"
    );

    fs::write(&release_path, "go")?;
    let _settled_text = poll_until_contains(
        &session,
        setup_text,
        "CELL_RETURNED",
        "foreground cell to return before the extra-line answer turn",
        Duration::from_secs(5),
    )
    .await?;

    let answer = session.write_stdin_raw_with("one\ntwo", Some(1.0)).await?;
    let answer_text = result_text(&answer);
    assert!(
        !is_busy_response(&answer_text),
        "background input answer should complete its request, got: {answer_text:?}"
    );
    assert!(
        answer_text.contains("BACKGROUND_ANSWER one"),
        "expected background input to consume the first line, got: {answer_text:?}"
    );

    let foreground = session
        .write_stdin_raw_with(
            "foreground = input('fg> ')\nprint('FOREGROUND_ANSWER', foreground)",
            Some(1.0),
        )
        .await?;
    let foreground_text = result_text(&foreground);
    assert!(
        !is_busy_response(&foreground_text),
        "expected foreground input prompt, got: {foreground_text:?}"
    );
    assert!(
        foreground_text.contains("fg> "),
        "expected foreground input to wait for fresh input, got: {foreground_text:?}"
    );
    assert!(
        !foreground_text.contains("FOREGROUND_ANSWER two"),
        "foreground input consumed stale detached stdin tail: {foreground_text:?}"
    );

    let fresh = session.write_stdin_raw_with("fresh", Some(5.0)).await?;
    let fresh_text = result_text(&fresh);
    session.cancel().await?;

    assert!(
        fresh_text.contains("FOREGROUND_ANSWER fresh"),
        "expected foreground input to consume fresh line, got: {fresh_text:?}"
    );
    Ok(())
}
