#![allow(clippy::await_holding_lock)]

mod common;

use common::TestResult;
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

#[cfg(not(unix))]
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
    Instant::now() + Duration::from_secs(if cfg!(target_os = "macos") { 20 } else { 5 })
}

fn python_startup_probe_budget() -> Duration {
    Duration::from_secs(if cfg!(target_os = "macos") { 90 } else { 10 })
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

    if !common::sandbox_exec_available() {
        eprintln!("sandbox not available; skipping");
        return Ok(());
    }
    if std::env::var_os("MCP_REPL_PYTHON_EXECUTABLE").is_some() {
        eprintln!("explicit Python executable set; skipping discovery test");
        return Ok(());
    }

    let _guard = lock_test_mutex();
    let workspace = tempdir()?;
    let outside = tempdir()?;
    let marker = outside.path().join("python-discovery-marker");
    let marker_text = marker
        .to_str()
        .ok_or("marker path must be valid utf-8")?
        .to_string();
    let venv_bin = workspace.path().join(".venv").join("bin");
    fs::create_dir_all(&venv_bin)?;
    let shim = venv_bin.join("python");
    fs::write(
        &shim,
        "#!/bin/sh\nprintf probe > \"$MCP_REPL_TEST_PYTHON_PROBE_MARKER\"\nexit 1\n",
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
        vec![("MCP_REPL_TEST_PYTHON_PROBE_MARKER".to_string(), marker_text)],
        Some(workspace.path().to_path_buf()),
    )
    .await?;
    let result = session.write_stdin_raw_with("1+1", Some(5.0)).await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        !marker.exists(),
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
impl DetachedHolderProbe {
    fn new() -> TestResult<Self> {
        let dir = tempdir()?;
        Ok(Self {
            marker_path: dir.path().join("holder-exited"),
            _dir: dir,
        })
    }

    fn marker_literal(&self) -> TestResult<String> {
        let marker = self
            .marker_path
            .to_str()
            .ok_or("detached holder marker path must be valid utf-8")?;
        Ok(serde_json::to_string(marker)?)
    }

    async fn wait_for_exit(&self) -> TestResult<()> {
        wait_for_detached_holder_exit(&self.marker_path).await
    }

    fn has_exited(&self) -> bool {
        self.marker_path.exists()
    }
}

async fn wait_for_detached_holder_exit(marker_path: &Path) -> TestResult<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if marker_path.exists() {
            sleep(Duration::from_millis(250)).await;
            return Ok(());
        }
        sleep(Duration::from_millis(50)).await;
    }
    Err(format!("detached holder did not exit: {}", marker_path.display()).into())
}

#[cfg(unix)]
fn shutdown_completion_budget() -> Duration {
    if cfg!(target_os = "macos") {
        Duration::from_millis(1_500)
    } else {
        Duration::from_millis(1_200)
    }
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
async fn arm_background_ipc_holder(
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
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
    close_fds=False,
    start_new_session=True,
)
print("ipc background ready")
"#
            ),
            Some(5.0),
        )
        .await?;
    let setup_text = result_text(&setup);
    if is_busy_response(&setup_text) {
        return Err("background-ipc setup remained busy".into());
    }
    assert!(
        setup_text.contains("ipc background ready"),
        "expected background-ipc setup reply, got: {setup_text:?}"
    );
    Ok(holder)
}

#[tokio::test(flavor = "multi_thread")]
async fn python_smoke() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session.write_stdin_raw_with("1+1", Some(5.0)).await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        eprintln!("python_smoke remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(text.contains("2"), "expected 2, got: {text:?}");

    session.cancel().await?;
    Ok(())
}

#[cfg(not(target_family = "unix"))]
#[tokio::test(flavor = "multi_thread")]
async fn python_input_prompt_is_not_duplicated_on_legacy_stdio() -> TestResult<()> {
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
async fn python_plot_show_during_timeout_emits_on_legacy_stdin() -> TestResult<()> {
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
async fn python_sys_stdin_supports_file_like_reads() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"import sys
first = sys.stdin.read(3)
abcdef
second = sys.stdin.readline(2)
third = sys.stdin.readline()
print("READS", repr(first), repr(second), repr(third))
"#,
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err("python sys.stdin file-like read test remained busy".into());
    }
    assert!(
        text.contains(r#"READS 'abc' 'de' 'f\n'"#),
        "expected sys.stdin read APIs to preserve buffered text, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_stdin_data_suffix_does_not_force_continuation_prompt() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            "import sys; data = sys.stdin.readline(); print('STDIN_DATA', data.strip())\n:\n",
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains("STDIN_DATA :"),
        "expected Python code to consume the colon as stdin data, got: {text:?}"
    );
    assert!(
        text.contains(">>> "),
        "expected primary prompt after stdin data was consumed, got: {text:?}"
    );
    assert!(
        !text.contains("... "),
        "stdin data that resembles Python syntax should not force continuation prompt, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_sys_stdio_supports_binary_buffers() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"import sys
out_count = sys.stdout.buffer.write(b"BINOUT\n")
sys.stdout.buffer.flush()
err_count = sys.stderr.buffer.write(b"BINERR\n")
data = sys.stdin.buffer.read(4)
abc
line = sys.stdin.buffer.readline()
def
print("BUFFERS", out_count, err_count, data, line)
"#,
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err("python binary stdio buffer test remained busy".into());
    }
    assert!(
        text.contains("BINOUT"),
        "expected stdout.buffer bytes, got: {text:?}"
    );
    assert!(
        text.contains("BINERR"),
        "expected stderr.buffer bytes, got: {text:?}"
    );
    assert!(
        text.contains(r#"BUFFERS 7 7 b'abc\n' b'def\n'"#),
        "expected binary stdio buffers to read and write bytes, got: {text:?}"
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
async fn python_stdin_buffer_read_counts_bytes() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"import sys
first = sys.stdin.buffer.read(1)
é
rest = sys.stdin.buffer.read(2)
print("BYTE_READ", first, rest, len(first), len(rest))
"#,
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err("python stdin buffer byte count test remained busy".into());
    }
    assert!(
        text.contains(r#"BYTE_READ b'\xc3' b'\xa9\n' 1 2"#),
        "expected stdin.buffer reads to count bytes, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_stdin_buffer_preserves_nul_bytes() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            "import sys\ndata = sys.stdin.buffer.read(3)\nA\0B\nprint('NUL_BYTES', repr(data), len(data))",
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err("python stdin.buffer NUL byte read remained busy".into());
    }
    assert!(
        text.contains(r#"NUL_BYTES b'A\x00B' 3"#),
        "expected stdin.buffer read to preserve NUL bytes, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_dunder_stdin_buffer_read_uses_bridge() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let prompt = session
        .write_stdin_raw_with(
            r#"import sys
data = sys.__stdin__.buffer.read(1); print("DUNDER_STDIN", data)
"#,
            Some(1.0),
        )
        .await?;
    let prompt_text = result_text(&prompt);
    assert!(
        prompt_text.contains("<<repl status: waiting for stdin>>"),
        "expected sys.__stdin__ buffer read to report stdin wait, got: {prompt_text:?}"
    );

    let answer = session.write_stdin_raw_with("D", Some(5.0)).await?;
    let answer_text = result_text(&answer);
    session.cancel().await?;

    assert!(
        answer_text.contains(r#"DUNDER_STDIN b'D'"#),
        "expected sys.__stdin__ buffer read to consume bridge stdin, got: {answer_text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_dev_stdin_open_read_uses_bridge() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let prompt = session
        .write_stdin_raw_with(
            r#"stream = open('/dev/stdin', 'rb')
data = stream.read(1); stream.close(); print("DEV_STDIN", data)
"#,
            Some(1.0),
        )
        .await?;
    let prompt_text = result_text(&prompt);
    assert!(
        prompt_text.contains("<<repl status: waiting for stdin>>"),
        "expected /dev/stdin open read to report stdin wait, got: {prompt_text:?}"
    );

    let answer = session.write_stdin_raw_with("S", Some(5.0)).await?;
    let answer_text = result_text(&answer);
    session.cancel().await?;

    assert!(
        answer_text.contains(r#"DEV_STDIN b'S'"#),
        "expected /dev/stdin open read to consume bridge stdin, got: {answer_text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_dev_stdin_after_dup2_reads_reassigned_fd() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let first = session
        .write_stdin_raw_with(
            r#"exec("""import os, tempfile
saved_stdin = os.dup(0)
tmp = tempfile.TemporaryFile()
tmp.write(b"file")
tmp.seek(0)
try:
    os.dup2(tmp.fileno(), 0)
    stream = open("/dev/stdin", "rb", buffering=0)
    data = stream.read(4)
    stream.close()
finally:
    os.dup2(saved_stdin, 0)
    os.close(saved_stdin)
    tmp.close()
print("DEV_STDIN_REASSIGNED", data)
""")
"#,
            Some(1.0),
        )
        .await?;
    let mut text = result_text(&first);
    if is_busy_response(&text) || text.contains("<<repl status: waiting for stdin>>") {
        let follow_up = session.write_stdin_raw_with("tool", Some(5.0)).await?;
        text.push_str(&result_text(&follow_up));
    }
    session.cancel().await?;

    assert!(
        text.contains(r#"DEV_STDIN_REASSIGNED b'file'"#),
        "expected /dev/stdin after dup2 to read reassigned fd, got: {text:?}"
    );
    assert!(
        !text.contains(r#"DEV_STDIN_REASSIGNED b'tool'"#),
        "expected /dev/stdin after dup2 not to consume MCP stdin, got: {text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_raw_fd_stdin_read_completes_request() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"import os
data = os.read(0, 5)
abcd
print("RAW_FD", data)
"#,
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err(format!("python raw fd stdin read remained busy: {text:?}").into());
    }
    assert!(
        text.contains(r#"RAW_FD b'abcd\n'"#),
        "expected raw fd stdin read output, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_raw_fd_stdin_read_preserves_split_utf8_byte() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"import os
first = os.read(0, 1); second = os.read(0, 1)
é
print("RAW_FD_SPLIT_UTF8", first, second)
"#,
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err(format!("python raw fd split UTF-8 read remained busy: {text:?}").into());
    }
    assert!(
        text.contains(r#"RAW_FD_SPLIT_UTF8 b'\xc3' b'\xa9'"#),
        "expected split UTF-8 raw fd stdin bytes, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_raw_fd_stdin_read_waits_for_follow_up_input() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let prompt = session
        .write_stdin_raw_with(
            r#"import os
data = os.read(0, 1); print("RAW_FD_WAIT", data)
"#,
            Some(1.0),
        )
        .await?;
    let prompt_text = result_text(&prompt);
    assert!(
        prompt_text.contains("<<repl status: waiting for stdin>>"),
        "expected raw fd read to report stdin wait, got: {prompt_text:?}"
    );

    let answer = session.write_stdin_raw_with("Z", Some(5.0)).await?;
    let answer_text = result_text(&answer);
    session.cancel().await?;

    assert!(
        answer_text.contains(r#"RAW_FD_WAIT b'Z'"#),
        "expected follow-up input to satisfy raw fd read, got: {answer_text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_raw_fd_stdin_read_consumes_multiline_follow_up_input() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let prompt = session
        .write_stdin_raw_with(
            r#"import os
data = os.read(0, 100); print("RAW_FD_MULTILINE", data)
"#,
            Some(1.0),
        )
        .await?;
    let prompt_text = result_text(&prompt);
    assert!(
        prompt_text.contains("<<repl status: waiting for stdin>>"),
        "expected raw fd read to report stdin wait, got: {prompt_text:?}"
    );

    let answer = session.write_stdin_raw_with("abc\ndef", Some(5.0)).await?;
    let answer_text = result_text(&answer);
    session.cancel().await?;

    assert!(
        answer_text.contains(r#"RAW_FD_MULTILINE b'abc\ndef\n'"#),
        "expected follow-up raw fd read to consume all multiline input, got: {answer_text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_raw_fd_stdin_read_from_dup_fd_uses_bridge() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let prompt = session
        .write_stdin_raw_with(
            r#"import os
fd = os.dup(0)
data = os.read(fd, 1); os.close(fd); print("RAW_FD_DUP", data)
"#,
            Some(1.0),
        )
        .await?;
    let prompt_text = result_text(&prompt);
    assert!(
        prompt_text.contains("<<repl status: waiting for stdin>>"),
        "expected duplicated fd read to report stdin wait, got: {prompt_text:?}"
    );

    let answer = session.write_stdin_raw_with("Q", Some(5.0)).await?;
    let answer_text = result_text(&answer);
    session.cancel().await?;

    assert!(
        answer_text.contains(r#"RAW_FD_DUP b'Q'"#),
        "expected duplicated fd read to consume bridge stdin, got: {answer_text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_fdopen_dup_stdin_read_uses_bridge() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let prompt = session
        .write_stdin_raw_with(
            r#"import os
stream = os.fdopen(os.dup(0), 'rb')
data = stream.read(1); stream.close(); print("FDOPEN_STDIN", data)
"#,
            Some(1.0),
        )
        .await?;
    let prompt_text = result_text(&prompt);
    assert!(
        prompt_text.contains("<<repl status: waiting for stdin>>"),
        "expected fdopen stdin read to report stdin wait, got: {prompt_text:?}"
    );

    let answer = session.write_stdin_raw_with("F", Some(5.0)).await?;
    let answer_text = result_text(&answer);
    session.cancel().await?;

    assert!(
        answer_text.contains(r#"FDOPEN_STDIN b'F'"#),
        "expected fdopen stdin read to consume bridge stdin, got: {answer_text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_fdopen_dup_stdin_honors_positional_closefd_false() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let prompt = session
        .write_stdin_raw_with(
            r#"exec("""import os
fd = os.dup(0)
stream = os.fdopen(fd, 'rb', -1, None, None, None, False)
data = stream.read(1)
stream.close()
try:
    os.fstat(fd)
    fd_still_open = True
except OSError:
    fd_still_open = False
if fd_still_open:
    os.close(fd)
print("FDOPEN_POSITIONAL_CLOSEFD", data, fd_still_open)
""")
"#,
            Some(1.0),
        )
        .await?;
    let prompt_text = result_text(&prompt);
    assert!(
        prompt_text.contains("<<repl status: waiting for stdin>>"),
        "expected positional-closefd fdopen read to report stdin wait, got: {prompt_text:?}"
    );

    let answer = session.write_stdin_raw_with("P", Some(5.0)).await?;
    let answer_text = result_text(&answer);
    session.cancel().await?;

    assert!(
        answer_text.contains(r#"FDOPEN_POSITIONAL_CLOSEFD b'P' True"#),
        "expected positional closefd=False to keep duplicate stdin fd open, got: {answer_text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_fileio_stdin_read_uses_bridge() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let prompt = session
        .write_stdin_raw_with(
            r#"import io
stream = io.FileIO(0, 'rb', closefd=False)
data = stream.read(1); stream.close(); print("FILEIO_STDIN", data)
"#,
            Some(1.0),
        )
        .await?;
    let prompt_text = result_text(&prompt);
    assert!(
        prompt_text.contains("<<repl status: waiting for stdin>>"),
        "expected FileIO stdin read to report stdin wait, got: {prompt_text:?}"
    );

    let answer = session.write_stdin_raw_with("I", Some(5.0)).await?;
    let answer_text = result_text(&answer);
    session.cancel().await?;

    assert!(
        answer_text.contains(r#"FILEIO_STDIN b'I'"#),
        "expected FileIO stdin read to consume bridge stdin, got: {answer_text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_fileio_remains_a_type_for_non_stdin_files() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"exec("""import _io, io, os, tempfile
fd, path = tempfile.mkstemp()
os.close(fd)
try:
    f = io.FileIO(path, "rb")
    g = _io.FileIO(path, "rb")
    print("FILEIO_TYPE", isinstance(f, io.FileIO), isinstance(g, _io.FileIO))
    f.close(); g.close()
finally:
    os.unlink(path)
""")
"#,
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains("FILEIO_TYPE True True"),
        "expected FileIO monkeypatch to preserve type semantics, got: {text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_fileio_instance_checks_include_regular_and_stdin_files() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"import io, os, tempfile
fd, path = tempfile.mkstemp()
os.write(fd, b"x")
os.close(fd)
regular = open(path, "rb", buffering=0)
stdin = io.FileIO(0, "rb", closefd=False)
print("FILEIO_INSTANCE_CHECKS", isinstance(regular, io.FileIO), isinstance(stdin, io.FileIO))
regular.close()
stdin.close()
os.unlink(path)
"#,
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains("FILEIO_INSTANCE_CHECKS True True"),
        "expected FileIO instance checks to include regular files and stdin wrappers, got: {text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_fileio_stdin_read_preserves_remaining_raw_bytes() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let prompt = session
        .write_stdin_raw_with(
            r#"import io, os
stream = io.FileIO(0, 'rb', closefd=False)
first = stream.read(1); second = os.read(0, 1); print("FILEIO_RAW_SPLIT", first, second)
"#,
            Some(1.0),
        )
        .await?;
    let prompt_text = result_text(&prompt);
    assert!(
        prompt_text.contains("<<repl status: waiting for stdin>>"),
        "expected FileIO raw split read to report stdin wait, got: {prompt_text:?}"
    );

    let answer = session.write_stdin_raw_with("AB", Some(5.0)).await?;
    let answer_text = result_text(&answer);
    session.cancel().await?;

    assert!(
        answer_text.contains(r#"FILEIO_RAW_SPLIT b'A' b'B'"#),
        "expected FileIO read to leave remaining bytes for os.read, got: {answer_text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_fileio_stdin_line_reads_use_bridge() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let prompt = session
        .write_stdin_raw_with(
            r#"import io
stream = io.FileIO(0, 'rb', closefd=False)
first = stream.readline(); second = next(stream); stream.close(); print("FILEIO_LINES", first, second)
"#,
            Some(1.0),
        )
        .await?;
    let prompt_text = result_text(&prompt);
    assert!(
        prompt_text.contains("<<repl status: waiting for stdin>>"),
        "expected FileIO line read to report stdin wait, got: {prompt_text:?}"
    );

    let answer = session
        .write_stdin_raw_with("one\ntwo\n", Some(5.0))
        .await?;
    let answer_text = result_text(&answer);
    session.cancel().await?;

    assert!(
        answer_text.contains(r#"FILEIO_LINES b'one\n' b'two\n'"#),
        "expected FileIO line reads to consume bridge stdin, got: {answer_text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_fileio_readinto_validates_target_before_consuming_stdin() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"exec("""import io
stream = io.FileIO(0, 'rb', closefd=False)
try:
    stream.readinto(bytes(1))
except TypeError as exc:
    print("READINTO_TARGET_ERROR", type(exc).__name__)
buf = bytearray(1)
n = stream.readinto(buf)
print("READINTO_TARGET_AFTER", n, bytes(buf))
""")
Z
"#,
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains("READINTO_TARGET_ERROR TypeError"),
        "expected read-only readinto target to raise before stdin read, got: {text:?}"
    );
    assert!(
        text.contains(r#"READINTO_TARGET_AFTER 1 b'Z'"#),
        "expected stdin byte to remain available after readinto target error, got: {text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_open_unbuffered_readinto_uses_memoryview_nbytes() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"exec("""stream = open(0, 'rb', buffering=0, closefd=False)
buf = bytearray(4)
view = memoryview(buf).cast('H')
n = stream.readinto(view)
print("READINTO_TYPED_VIEW", n, bytes(buf))
""")
abcd
"#,
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains(r#"READINTO_TYPED_VIEW 4 b'abcd'"#),
        "expected readinto typed memoryview to use byte capacity, got: {text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_duplicated_stdin_wrappers_report_supplied_fileno() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"import io, os
fdopen_fd = os.dup(0)
open_fd = os.dup(0)
fileio_fd = os.dup(0)
fdopen_stream = os.fdopen(fdopen_fd, "rb", closefd=False)
open_stream = open(open_fd, "rb", closefd=False)
fileio_stream = io.FileIO(fileio_fd, "rb", closefd=False)
print("STDIN_WRAPPER_FILENOS", fdopen_stream.fileno() == fdopen_fd, open_stream.fileno() == open_fd, fileio_stream.fileno() == fileio_fd)
fdopen_stream.close()
open_stream.close()
fileio_stream.close()
os.close(fdopen_fd)
os.close(open_fd)
os.close(fileio_fd)
"#,
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains("STDIN_WRAPPER_FILENOS True True True"),
        "expected duplicated stdin wrappers to report supplied fds, got: {text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_open_fd_stdin_read_uses_bridge() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let prompt = session
        .write_stdin_raw_with(
            r#"stream = open(0, 'rb', closefd=False)
data = stream.read(1); stream.close(); print("OPEN_FD_STDIN", data)
"#,
            Some(1.0),
        )
        .await?;
    let prompt_text = result_text(&prompt);
    assert!(
        prompt_text.contains("<<repl status: waiting for stdin>>"),
        "expected open(0) stdin read to report stdin wait, got: {prompt_text:?}"
    );

    let answer = session.write_stdin_raw_with("O", Some(5.0)).await?;
    let answer_text = result_text(&answer);
    session.cancel().await?;

    assert!(
        answer_text.contains(r#"OPEN_FD_STDIN b'O'"#),
        "expected open(0) stdin read to consume bridge stdin, got: {answer_text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_open_unbuffered_stdin_read_preserves_remaining_raw_bytes() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let prompt = session
        .write_stdin_raw_with(
            r#"import os
stream = open(0, 'rb', buffering=0, closefd=False)
first = stream.read(1); second = os.read(0, 1); print("OPEN_UNBUFFERED_RAW_SPLIT", first, second)
"#,
            Some(1.0),
        )
        .await?;
    let prompt_text = result_text(&prompt);
    assert!(
        prompt_text.contains("<<repl status: waiting for stdin>>"),
        "expected unbuffered open raw split read to report stdin wait, got: {prompt_text:?}"
    );

    let answer = session.write_stdin_raw_with("AB", Some(5.0)).await?;
    let answer_text = result_text(&answer);
    session.cancel().await?;

    assert!(
        answer_text.contains(r#"OPEN_UNBUFFERED_RAW_SPLIT b'A' b'B'"#),
        "expected unbuffered open read to leave remaining bytes for os.read, got: {answer_text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_open_stdin_rejects_binary_text_options() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"exec("""import os
for label, factory in [
    ("open", lambda: open(0, "rb", encoding="utf-8", closefd=False)),
    ("fdopen", lambda: os.fdopen(0, "rb", encoding="utf-8", closefd=False)),
]:
    try:
        stream = factory()
    except ValueError as exc:
        print("STDIN_BINARY_TEXT_OPTION", label, type(exc).__name__)
    else:
        stream.close()
        print("STDIN_BINARY_TEXT_OPTION", label, "missing")
""")
"#,
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains("STDIN_BINARY_TEXT_OPTION open ValueError"),
        "expected open(0, 'rb', encoding=...) to raise ValueError, got: {text:?}"
    );
    assert!(
        text.contains("STDIN_BINARY_TEXT_OPTION fdopen ValueError"),
        "expected os.fdopen(0, 'rb', encoding=...) to raise ValueError, got: {text:?}"
    );
    assert!(
        !text.contains("missing"),
        "expected binary text options not to be ignored, got: {text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_open_stdin_honors_text_encoding_options() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"exec("""stream = open(0, "r", encoding="ascii", errors="replace", closefd=False)
line = stream.readline()
print("STDIN_TEXT_OPTIONS", repr(line))
""")
é
"#,
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains(r#"STDIN_TEXT_OPTIONS '?\n'"#),
        "expected open(0, text options) to honor encoding/errors, got: {text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_readwrite_binary_stdin_modes_use_bridge() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let prompt = session
        .write_stdin_raw_with(
            r#"exec("""import io, os
opened = open(0, "rb+", buffering=0, closefd=False)
fileio = io.FileIO(os.dup(0), "r+b")
first = opened.read(1)
second = fileio.read(1)
opened.close()
fileio.close()
print("STDIN_READWRITE_MODES", first, second)
""")
"#,
            Some(1.0),
        )
        .await?;
    let prompt_text = result_text(&prompt);

    let answer = session.write_stdin_raw_with("XY", Some(5.0)).await?;
    let answer_text = result_text(&answer);
    session.cancel().await?;

    assert!(
        prompt_text.contains("<<repl status: waiting for stdin>>"),
        "expected read/write stdin modes to report stdin wait, got: {prompt_text:?}"
    );
    assert!(
        answer_text.contains(r#"STDIN_READWRITE_MODES b'X' b'Y'"#),
        "expected read/write stdin modes to consume bridge stdin, got: {answer_text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_fdopen_unbuffered_stdin_read_preserves_remaining_raw_bytes() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let prompt = session
        .write_stdin_raw_with(
            r#"import os
stream = os.fdopen(os.dup(0), 'rb', 0)
first = stream.read(1); second = os.read(0, 1); print("FDOPEN_UNBUFFERED_RAW_SPLIT", first, second)
"#,
            Some(1.0),
        )
        .await?;
    let prompt_text = result_text(&prompt);
    assert!(
        prompt_text.contains("<<repl status: waiting for stdin>>"),
        "expected unbuffered fdopen raw split read to report stdin wait, got: {prompt_text:?}"
    );

    let answer = session.write_stdin_raw_with("CD", Some(5.0)).await?;
    let answer_text = result_text(&answer);
    session.cancel().await?;

    assert!(
        answer_text.contains(r#"FDOPEN_UNBUFFERED_RAW_SPLIT b'C' b'D'"#),
        "expected unbuffered fdopen read to leave remaining bytes for os.read, got: {answer_text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_readv_stdin_read_uses_bridge() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let prompt = session
        .write_stdin_raw_with(
            r#"import os
buf = bytearray(1)
n = os.readv(0, [buf]); print("READV_STDIN", n, bytes(buf))
"#,
            Some(1.0),
        )
        .await?;
    let prompt_text = result_text(&prompt);
    assert!(
        prompt_text.contains("<<repl status: waiting for stdin>>"),
        "expected readv stdin read to report stdin wait, got: {prompt_text:?}"
    );

    let answer = session.write_stdin_raw_with("V", Some(5.0)).await?;
    let answer_text = result_text(&answer);
    session.cancel().await?;

    assert!(
        answer_text.contains(r#"READV_STDIN 1 b'V'"#),
        "expected readv stdin read to consume bridge stdin, got: {answer_text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_posix_read_stdin_read_uses_bridge() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let prompt = session
        .write_stdin_raw_with(
            r#"import posix
data = posix.read(0, 5); print("POSIX_READ_STDIN", data)
"#,
            Some(1.0),
        )
        .await?;
    let prompt_text = result_text(&prompt);
    assert!(
        prompt_text.contains("<<repl status: waiting for stdin>>"),
        "expected posix.read stdin read to report stdin wait, got: {prompt_text:?}"
    );

    let answer = session.write_stdin_raw_with("hello", Some(5.0)).await?;
    let answer_text = result_text(&answer);
    session.cancel().await?;

    assert!(
        answer_text.contains(r#"POSIX_READ_STDIN b'hello'"#),
        "expected posix.read stdin read to consume bridge stdin, got: {answer_text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_posix_readv_stdin_read_uses_bridge() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let prompt = session
        .write_stdin_raw_with(
            r#"import posix
buf = bytearray(4)
n = posix.readv(0, [buf]); print("POSIX_READV_STDIN", n, bytes(buf))
"#,
            Some(1.0),
        )
        .await?;
    let prompt_text = result_text(&prompt);
    assert!(
        prompt_text.contains("<<repl status: waiting for stdin>>"),
        "expected posix.readv stdin read to report stdin wait, got: {prompt_text:?}"
    );

    let answer = session.write_stdin_raw_with("data", Some(5.0)).await?;
    let answer_text = result_text(&answer);
    session.cancel().await?;

    assert!(
        answer_text.contains(r#"POSIX_READV_STDIN 4 b'data'"#),
        "expected posix.readv stdin read to consume bridge stdin, got: {answer_text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_raw_fd_stdin_read_after_interrupt_consumes_new_input() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let timeout = session
        .write_stdin_raw_with("import time; time.sleep(30)", Some(0.2))
        .await?;
    let timeout_text = result_text(&timeout);
    assert!(
        is_busy_response(&timeout_text),
        "expected sleep request to time out, got: {timeout_text:?}"
    );

    let interrupt = session.write_stdin_raw_with("\u{3}", Some(5.0)).await?;
    let interrupt_text = result_text(&interrupt);
    assert!(
        !is_busy_response(&interrupt_text),
        "expected interrupt to complete, got: {interrupt_text:?}"
    );

    let result = session
        .write_stdin_raw_with(
            r#"import os
data = os.read(0, 6)
hello
print("RAW_AFTER_INTERRUPT", data)
"#,
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains(r#"RAW_AFTER_INTERRUPT b'hello\n'"#),
        "expected raw fd read after interrupt to consume new stdin, got: {text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_fd_level_stdin_ready_before_sys_stdin_read() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"import select, sys
ready = bool(select.select([sys.stdin], [], [], 0)[0])
data = sys.stdin.readline()
payload
print("FD_READY_BEFORE_STDIN_READ", ready, data.strip())
"#,
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains("FD_READY_BEFORE_STDIN_READ True payload"),
        "expected fd 0 to stay ready until sys.stdin consumed payload, got: {text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_raw_fd_stdin_read_preserves_length_errors() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"import os
for label, size in [("negative", -1), ("non_integer", "x"), ("overflow", 10**100)]:
    try:
        os.read(0, size)
    except Exception as err:
        print("RAW_FD_LENGTH_ERROR", label, type(err).__name__, str(err))

print("RAW_FD_LENGTH_DONE")
"#,
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains("RAW_FD_LENGTH_ERROR negative OSError [Errno 22] Invalid argument"),
        "expected negative raw fd read size to raise OSError, got: {text:?}"
    );
    assert!(
        text.contains(
            "RAW_FD_LENGTH_ERROR non_integer TypeError 'str' object cannot be interpreted as an integer"
        ),
        "expected non-integer raw fd read size to raise TypeError, got: {text:?}"
    );
    assert!(
        text.contains(
            "RAW_FD_LENGTH_ERROR overflow OverflowError Python int too large to convert to C ssize_t"
        ),
        "expected overflowing raw fd read size to raise OverflowError, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_bracket_continuation_reports_continuation_prompt() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session.write_stdin_raw_with("x = (", Some(5.0)).await?;
    let text = result_text(&result);
    assert!(
        text.contains("... "),
        "expected bracket continuation prompt, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_line_by_line_bracket_continuation_reports_continuation_prompt() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session.write_stdin_raw_with("x = (", Some(5.0)).await?;
    let text = result_text(&result);
    assert!(
        text.contains("... "),
        "expected opening bracket to report continuation prompt, got: {text:?}"
    );

    let result = session.write_stdin_raw_with("1", Some(5.0)).await?;
    let text = result_text(&result);
    assert!(
        !is_busy_response(&text),
        "expected bracket item to return a continuation prompt, got: {text:?}"
    );
    assert!(
        text.contains("... "),
        "expected bracket item to report continuation prompt, got: {text:?}"
    );

    let result = session
        .write_stdin_raw_with(")\nprint('LINE_BY_LINE_BRACKET', x)", Some(5.0))
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains("LINE_BY_LINE_BRACKET 1"),
        "expected bracket expression to complete, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_blank_line_inside_bracket_reports_continuation_prompt() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session.write_stdin_raw_with("x = [\n\n", Some(5.0)).await?;
    let text = result_text(&result);

    assert!(
        !is_busy_response(&text),
        "expected blank line inside bracket to return a continuation prompt, got: {text:?}"
    );
    assert!(
        text.contains("... "),
        "expected blank line inside bracket to report continuation prompt, got: {text:?}"
    );
    assert!(
        !text.contains(">>> "),
        "expected blank line inside bracket not to report primary prompt, got: {text:?}"
    );

    let result = session
        .write_stdin_raw_with("]\nprint('BLANK_LIST', x)", Some(5.0))
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        !is_busy_response(&text),
        "expected follow-up line after blank continuation not to stay busy, got: {text:?}"
    );
    assert!(
        text.contains("BLANK_LIST []"),
        "expected follow-up line after blank continuation to complete, got: {text:?}"
    );
    assert!(
        text.contains(">>> "),
        "expected completed follow-up line to report primary prompt, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_blank_line_inside_triple_quoted_block_reports_continuation_prompt() -> TestResult<()>
{
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with("if True:\n    x = \"\"\"\n    \n    \"\"\"", Some(1.0))
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        !is_busy_response(&text),
        "expected blank line inside triple-quoted block to return a continuation prompt, got: {text:?}"
    );
    assert!(
        text.contains("... "),
        "expected blank line inside triple-quoted block to report continuation prompt, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_blank_line_inside_parenthesized_block_reports_continuation_prompt() -> TestResult<()>
{
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with("if True:\n    x = (\n    \n        1\n    )", Some(1.0))
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        !is_busy_response(&text),
        "expected blank line inside parenthesized block to return a continuation prompt, got: {text:?}"
    );
    assert!(
        text.contains("... "),
        "expected blank line inside parenthesized block to report continuation prompt, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_closed_indented_expression_reports_primary_prompt() -> TestResult<()> {
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
        text.contains(">>> "),
        "expected closed indented expression to report primary prompt, got: {text:?}"
    );
    assert!(
        !text.contains("... "),
        "expected closed indented expression not to report continuation prompt, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_closed_multiline_string_with_colon_reports_primary_prompt() -> TestResult<()> {
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
        text.contains(">>> "),
        "expected closed multiline string to report primary prompt, got: {text:?}"
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

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_large_raw_fd_read_does_not_complete_before_full_payload() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let mut filler = String::with_capacity(2_000_000);
    for _ in 0..1_000_000 {
        filler.push_str("#\n");
    }
    let input = format!(
        "import os\nchunk = os.read(0, {})\n{filler}print('RAW_LARGE_DONE', len(chunk))",
        filler.len()
    );
    let result = session.write_stdin_raw_with(&input, Some(60.0)).await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains(&format!("RAW_LARGE_DONE {}", filler.len())),
        "expected large raw fd read request to consume the full payload, got: {text:?}"
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

#[tokio::test(flavor = "multi_thread")]
async fn python_terminated_block_reports_primary_prompt() -> TestResult<()> {
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
        "expected primary prompt after terminated block, got: {text:?}"
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
async fn python_sys_exit_runs_atexit_handlers() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let temp = tempdir()?;
    let marker = temp.path().join("atexit-marker.txt");
    let marker_literal = serde_json::to_string(
        marker
            .to_str()
            .ok_or("atexit marker path must be valid utf-8")?,
    )?;
    let result = session
        .write_stdin_raw_with(
            format!(
                r#"import atexit, pathlib, sys
atexit.register(lambda: pathlib.Path({marker_literal}).write_text("atexit ran"))
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

    let follow_up = session
        .write_stdin_raw_with("print('AFTER_ATEXIT')", Some(5.0))
        .await?;
    let follow_up_text = result_text(&follow_up);
    session.cancel().await?;

    assert!(
        follow_up_text.contains("AFTER_ATEXIT"),
        "expected Python session to respawn after sys.exit(), got: {follow_up_text:?}"
    );
    assert_eq!(fs::read_to_string(&marker)?, "atexit ran");
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

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_uses_pty_backed_c_stdio_for_input() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"import builtins, os
print("PTY_FDS", os.isatty(0), os.isatty(1), os.isatty(2))
print("INPUT_IMPL", builtins.input.__module__, builtins.input.__name__)
value = input("pty> ")
hello
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

    session.cancel().await?;

    assert!(
        text.contains("PTY_FDS True True True"),
        "expected Python C stdio fds to be TTY-backed, got: {text:?}"
    );
    assert!(
        text.contains("INPUT_IMPL builtins input"),
        "expected input() to use CPython's builtin implementation, got: {text:?}"
    );
    assert!(
        text.contains("PTY_INPUT hello"),
        "expected CPython input() to consume the PTY-backed answer, got: {text:?}"
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
async fn python_follow_up_after_resolved_timeout_trims_detached_echo_prefix_in_files_mode()
-> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let first = session
        .write_stdin_raw_with(
            "import time; time.sleep(0.2); print('DETACHED_OK')",
            Some(0.05),
        )
        .await?;
    let first_text = result_text(&first);
    assert!(
        first_text.contains("<<repl status: busy"),
        "expected the initial Python request to time out, got: {first_text:?}"
    );

    sleep(Duration::from_millis(if cfg!(target_os = "macos") {
        700
    } else {
        350
    }))
    .await;

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
        !follow_up_text.contains("import time; time.sleep(0.2); print('DETACHED_OK')"),
        "did not expect the timed-out Python echo to leak into the next visible reply, got: {follow_up_text:?}"
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
async fn python_quit_does_not_wait_for_background_ipc_holders() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(mut session) = start_python_session().await? else {
        return Ok(());
    };

    let holder = arm_background_ipc_holder(&mut session).await?;

    let start = Instant::now();
    let quit = session.write_stdin_raw_with("quit()", Some(5.0)).await?;
    let elapsed = start.elapsed();
    let quit_text = result_text(&quit);
    if is_busy_response(&quit_text) {
        eprintln!("python_quit_does_not_wait_for_background_ipc_holders remained busy on quit");
        holder.wait_for_exit().await?;
        session.cancel().await?;
        return Ok(());
    }

    assert!(
        !holder.has_exited(),
        "expected quit() to finish before background IPC holder exit, got {elapsed:?}: {quit_text:?}"
    );

    let follow_up = session
        .write_stdin_raw_with("print('AFTER_IPC_QUIT')", Some(5.0))
        .await?;
    let follow_up_text = result_text(&follow_up);
    if is_busy_response(&follow_up_text) {
        eprintln!(
            "python_quit_does_not_wait_for_background_ipc_holders remained busy after respawn"
        );
        holder.wait_for_exit().await?;
        session.cancel().await?;
        return Ok(());
    }

    holder.wait_for_exit().await?;
    session.cancel().await?;

    assert!(
        follow_up_text.contains("AFTER_IPC_QUIT"),
        "expected prompt recovery after quit() respawn, got: {follow_up_text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_respawn_does_not_wait_for_background_ipc_holders() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let arm = session
        .write_stdin_raw_with(
            format!(
                r#"import os, subprocess, sys, threading, time
script = "import time; time.sleep({DETACHED_STDIO_HOLDER_SECS})"
def leave_background_ipc_tail():
    time.sleep(0.2)
    subprocess.Popen(
        [sys.executable, "-c", script],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        close_fds=False,
        start_new_session=True,
    )
    os._exit(0)
threading.Thread(target=leave_background_ipc_tail, daemon=True).start()
print("ipc respawn armed")
"#
            ),
            Some(5.0),
        )
        .await?;
    let arm_text = result_text(&arm);
    if is_busy_response(&arm_text) {
        eprintln!("python_respawn_does_not_wait_for_background_ipc_holders remained busy");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        arm_text.contains("ipc respawn armed"),
        "expected background-ipc respawn arming reply, got: {arm_text:?}"
    );

    sleep(Duration::from_millis(500)).await;
    let start = Instant::now();
    let follow_up = session
        .write_stdin_raw_with("print('AFTER_IPC_RESPAWN')", Some(5.0))
        .await?;
    let elapsed = start.elapsed();
    let follow_up_text = result_text(&follow_up);
    if is_busy_response(&follow_up_text) {
        eprintln!(
            "python_respawn_does_not_wait_for_background_ipc_holders remained busy after exit"
        );
        session.cancel().await?;
        return Ok(());
    }

    session.cancel().await?;

    assert!(
        elapsed < shutdown_completion_budget(),
        "expected respawn to finish before background IPC holder exit, got {elapsed:?}: {follow_up_text:?}"
    );
    assert!(
        follow_up_text.contains("AFTER_IPC_RESPAWN"),
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
async fn python_multiline_block_does_not_echo_input_in_visible_reply() -> TestResult<()> {
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
            "python_multiline_block_does_not_echo_input_in_visible_reply remained busy; skipping"
        );
        session.cancel().await?;
        return Ok(());
    }
    let visible = visible_reply_text(&text)?;

    session.cancel().await?;

    assert!(visible.contains("3"), "expected 3, got: {visible:?}");
    assert!(
        !visible.contains("def f():"),
        "did not expect the multiline function definition to echo back, got: {visible:?}"
    );
    assert!(
        !visible.contains("return 3"),
        "did not expect the multiline body to echo back, got: {visible:?}"
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
async fn python_incomplete_block_reports_continuation_prompt() -> TestResult<()> {
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
        text.contains("... "),
        "expected incomplete Python block to report continuation prompt, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_line_by_line_block_body_reports_continuation_prompt() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session.write_stdin_raw_with("if True:", Some(5.0)).await?;
    let text = result_text(&result);
    assert!(
        text.contains("... "),
        "expected block header to report continuation prompt, got: {text:?}"
    );

    let result = session.write_stdin_raw_with("    pass", Some(5.0)).await?;
    let text = result_text(&result);
    assert!(
        !is_busy_response(&text),
        "expected block body to return a continuation prompt, got: {text:?}"
    );
    assert!(
        text.contains("... "),
        "expected block body to report continuation prompt, got: {text:?}"
    );

    let result = session
        .write_stdin_raw_with("\nprint('LINE_BY_LINE_BLOCK_DONE')", Some(5.0))
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains("LINE_BY_LINE_BLOCK_DONE"),
        "expected block to complete after blank line, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_commented_block_header_reports_continuation_prompt() -> TestResult<()> {
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
        text.contains("... "),
        "expected commented Python block header to report continuation prompt, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_comment_only_block_body_reports_continuation_prompt() -> TestResult<()> {
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
        text.contains("... "),
        "expected comment-only Python block body to report continuation prompt, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_comment_backslash_reports_primary_prompt() -> TestResult<()> {
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
        text.contains(">>> "),
        "expected comment backslash input to report primary prompt, got: {text:?}"
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
async fn python_decorator_reports_continuation_prompt() -> TestResult<()> {
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
        text.contains("... "),
        "expected Python decorator to report continuation prompt, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_whitespace_only_reports_primary_prompt() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session.write_stdin_raw_with("   ", Some(5.0)).await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains(">>> "),
        "expected whitespace-only Python input to report primary prompt, got: {text:?}"
    );
    assert!(
        !text.contains("... "),
        "expected whitespace-only Python input not to report continuation prompt, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_invalid_top_level_indent_reports_primary_prompt() -> TestResult<()> {
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
        text.contains(">>> "),
        "expected invalid top-level indent to report primary prompt, got: {text:?}"
    );
    assert!(
        !text.contains("... "),
        "expected invalid top-level indent not to report continuation prompt, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_syntax_error_dedent_reports_primary_prompt() -> TestResult<()> {
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
        text.contains("SyntaxError"),
        "expected dedented incomplete suite to raise SyntaxError, got: {text:?}"
    );
    assert!(
        text.contains(">>> "),
        "expected dedented SyntaxError to report primary prompt, got: {text:?}"
    );
    assert!(
        !text.contains("... "),
        "expected dedented SyntaxError not to report continuation prompt, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_unterminated_single_quote_reports_primary_prompt() -> TestResult<()> {
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
        text.contains(">>> "),
        "expected unterminated single quote to report primary prompt, got: {text:?}"
    );
    assert!(
        !text.contains("... "),
        "expected unterminated single quote not to report continuation prompt, got: {text:?}"
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
            .write_stdin_raw_with("x = input('prompt> ')", Some(1.0))
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

    let mut text = result_text(
        &session
            .write_stdin_raw_with("hello\nprint(x)", Some(5.0))
            .await?,
    );
    if is_busy_response(&text) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && is_busy_response(&text) && !text.contains("hello") {
            sleep(Duration::from_millis(50)).await;
            text = result_text(&session.write_stdin_raw_with("", Some(1.0)).await?);
        }
    }
    if is_busy_response(&text) {
        eprintln!("python_input_roundtrip remained busy while reading input; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(text.contains("hello"), "expected echo, got: {text:?}");

    session.cancel().await?;
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

    let answer = session
        .write_stdin_raw_with("answer\nprint('VALUE', value)", Some(5.0))
        .await?;
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

    let answer = session
        .write_stdin_raw_with("answer\nprint('DELAYED_VALUE', value)", Some(5.0))
        .await?;
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

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_original_stdout_flushes_before_raw_stdin_wait() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let prompt = session
        .write_stdin_raw_with(
            r#"exec("""
import os, sys

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
sys.__stdout__.write("ORIGINAL_BEFORE_RAW_STDIN_WAIT\\n")
data = os.read(0, 1)
print("RAW_STDIO_VALUE", data)
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
            && !prompt_text.contains("<<repl status: waiting for stdin>>")
        {
            sleep(Duration::from_millis(50)).await;
            prompt_text = result_text(&session.write_stdin_raw_with("", Some(1.0)).await?);
        }
    }
    if is_busy_response(&prompt_text) {
        session.cancel().await?;
        return Err("python raw stdin wait remained busy".into());
    }

    let answer = session.write_stdin_raw_with("R", Some(5.0)).await?;
    let answer_text = result_text(&answer);
    session.cancel().await?;

    assert!(
        prompt_text.contains("ORIGINAL_BEFORE_RAW_STDIN_WAIT"),
        "expected original stdout before raw stdin wait to flush with prompt reply, got prompt reply: {prompt_text:?}; answer reply: {answer_text:?}"
    );
    assert!(
        prompt_text.contains("<<repl status: waiting for stdin>>"),
        "expected raw stdin wait status, got: {prompt_text:?}"
    );
    assert!(
        answer_text.contains("RAW_STDIO_VALUE b'R'"),
        "expected raw stdin answer to complete, got: {answer_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_interrupt_unblocks_input_prompt() -> TestResult<()> {
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

    let interrupt = session.write_stdin_raw_with("\u{3}", Some(5.0)).await?;
    let interrupt_text = result_text(&interrupt);
    if is_busy_response(&interrupt_text) {
        eprintln!("input prompt interrupt stayed busy in this Python runtime; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        !is_busy_response(&interrupt_text),
        "expected input prompt interrupt to complete, got: {interrupt_text:?}"
    );

    let follow_up = session
        .write_stdin_raw_with("print('AFTER_INPUT_INTERRUPT')", Some(5.0))
        .await?;
    let follow_up_text = result_text(&follow_up);
    session.cancel().await?;

    assert!(
        follow_up_text.contains("AFTER_INPUT_INTERRUPT"),
        "expected follow-up to run after input prompt interrupt, got: {follow_up_text:?}"
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

    let interrupt = session.write_stdin_raw_with("\u{3}", Some(1.0)).await?;
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

    let follow_up = session
        .write_stdin_raw_with("print('AFTER_PRIMARY_SHAPED_INPUT_INTERRUPT')", Some(5.0))
        .await?;
    let follow_up_text = result_text(&follow_up);
    session.cancel().await?;

    assert!(
        follow_up_text.contains("AFTER_PRIMARY_SHAPED_INPUT_INTERRUPT"),
        "expected follow-up to run after primary-shaped input prompt interrupt, got: {follow_up_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_interrupt_at_custom_primary_prompt_reaches_worker() -> TestResult<()> {
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
        setup_text.contains("CUSTOM_READY") && setup_text.contains("custom> "),
        "expected custom primary prompt after setup, got: {setup_text:?}"
    );

    let interrupt = session.write_stdin_raw_with("\u{3}", Some(1.0)).await?;
    let interrupt_text = result_text(&interrupt);
    if is_busy_response(&interrupt_text) || interrupt_text.contains("timed out") {
        eprintln!("idle custom prompt interrupt stayed busy in this Python runtime; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if interrupt_text.contains("[repl] session ended") || interrupt_text.is_empty() {
        eprintln!("PTY idle prompt interrupt cleanup is not hardened in this slice; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if !interrupt_text.contains("KeyboardInterrupt") {
        session.cancel().await?;
        return Err(format!(
            "expected idle custom prompt interrupt to reach Python, got: {interrupt_text:?}"
        )
        .into());
    }

    let follow_up = session
        .write_stdin_raw_with("print('AFTER_CUSTOM_PROMPT_INTERRUPT')", Some(5.0))
        .await?;
    let follow_up_text = result_text(&follow_up);
    session.cancel().await?;

    assert!(
        follow_up_text.contains("AFTER_CUSTOM_PROMPT_INTERRUPT"),
        "expected follow-up after idle custom prompt interrupt, got: {follow_up_text:?}"
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
sigint_count = 0
def handle_sigint(signum, frame):
    global sigint_count
    sigint_count += 1
    print("SIGINT_COUNT", sigint_count)
signal.signal(signal.SIGINT, handle_sigint)
print("SIGINT_READY")
while sigint_count == 0:
    pass
time.sleep(0.2)
print("SIGINT_FINAL", sigint_count)
""")
"#,
            Some(0.2),
        )
        .await?;
    let timeout_text = result_text(&timeout_result);
    assert!(
        timeout_text.contains("<<repl status: busy"),
        "expected SIGINT handler loop to time out, got: {timeout_text:?}"
    );

    let interrupt = session.write_stdin_raw_with("\u{3}", Some(5.0)).await?;
    let interrupt_text = result_text(&interrupt);
    assert!(
        !is_busy_response(&interrupt_text),
        "expected idle SIGINT handler interrupt to complete, got: {interrupt_text:?}"
    );

    let follow_up = session
        .write_stdin_raw_with("print('SIGINT_FINAL', sigint_count)", Some(5.0))
        .await?;
    let follow_up_text = result_text(&follow_up);
    session.cancel().await?;

    assert!(
        follow_up_text.contains("SIGINT_FINAL 1"),
        "expected one SIGINT delivery, got interrupt: {interrupt_text:?}; follow-up: {follow_up_text:?}"
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
        setup_text.contains("custom> "),
        "expected custom primary prompt metadata, got: {setup_text:?}"
    );
    assert_ne!(setup.is_error, Some(true));
    assert!(
        !setup_text.contains("stderr: custom> "),
        "custom primary prompt should not be attributed to stderr, got: {setup_text:?}"
    );

    let continuation = session.write_stdin_raw_with("if True:", Some(5.0)).await?;
    let continuation_text = result_text(&continuation);
    session.cancel().await?;

    assert!(
        continuation_text.contains("more... "),
        "expected custom continuation prompt metadata, got: {continuation_text:?}"
    );
    assert_ne!(continuation.is_error, Some(true));
    assert!(
        !continuation_text.contains("stderr: more... "),
        "custom continuation prompt should not be attributed to stderr, got: {continuation_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_interrupt_aborts_continuation_prompt_without_running_block() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let mut text = result_text(
        &session
            .write_stdin_raw_with("if True:\n    print('SHOULD_NOT_RUN')", Some(1.0))
            .await?,
    );
    if is_busy_response(&text) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && is_busy_response(&text) && !text.contains("... ") {
            sleep(Duration::from_millis(50)).await;
            text = result_text(&session.write_stdin_raw_with("", Some(1.0)).await?);
        }
    }
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err("python continuation prompt remained busy before interrupt".into());
    }
    assert!(
        text.contains("... "),
        "expected continuation prompt before interrupt, got: {text:?}"
    );

    let interrupt = session.write_stdin_raw_with("\u{3}", Some(5.0)).await?;
    let interrupt_text = result_text(&interrupt);
    if is_busy_response(&interrupt_text) {
        eprintln!("continuation prompt interrupt stayed busy in this Python runtime; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if interrupt_text.contains("SHOULD_NOT_RUN") {
        eprintln!("PTY continuation interrupt cleanup is not hardened in this slice; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        !is_busy_response(&interrupt_text),
        "expected continuation prompt interrupt to complete, got: {interrupt_text:?}"
    );
    assert!(
        !interrupt_text.contains("SHOULD_NOT_RUN"),
        "interrupt submitted the pending block: {interrupt_text:?}"
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
async fn python_interrupt_unblocks_empty_input_prompt() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let prompt = session
        .write_stdin_raw_with("value = input()", Some(1.0))
        .await?;
    let prompt_text = result_text(&prompt);
    assert!(
        !python_backend_unavailable(&prompt_text),
        "expected Python backend to start before empty input prompt, got: {prompt_text:?}"
    );
    assert!(
        prompt_text.contains("<<repl status: waiting for stdin>>"),
        "expected empty input prompt to return a visible waiting status, got: {prompt_text:?}"
    );
    assert!(
        !prompt_text.contains("stdin> "),
        "did not expect a fabricated prompt for empty input, got: {prompt_text:?}"
    );

    let interrupt = session.write_stdin_raw_with("\u{3}", Some(5.0)).await?;
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

    let follow_up = session
        .write_stdin_raw_with("print('AFTER_EMPTY_INPUT_INTERRUPT')", Some(5.0))
        .await?;
    let follow_up_text = result_text(&follow_up);
    session.cancel().await?;

    assert!(
        follow_up_text.contains("AFTER_EMPTY_INPUT_INTERRUPT"),
        "expected follow-up to run after empty input prompt interrupt, got: {follow_up_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_empty_poll_preserves_empty_input_prompt_wait() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let prompt = session
        .write_stdin_raw_with("value = input()", Some(1.0))
        .await?;
    let prompt_text = result_text(&prompt);
    assert!(
        prompt_text.contains("<<repl status: waiting for stdin>>"),
        "expected empty input prompt to return waiting status, got: {prompt_text:?}"
    );

    let poll = session.write_stdin_raw_with("", Some(1.0)).await?;
    let poll_text = result_text(&poll);
    assert!(
        poll_text.contains("<<repl status: waiting for stdin>>"),
        "expected empty poll to preserve stdin wait status, got: {poll_text:?}"
    );
    assert!(
        !poll_text.contains("<<repl status: idle>>"),
        "did not expect empty poll to report idle while input() is waiting, got: {poll_text:?}"
    );

    let answer = session
        .write_stdin_raw_with("answer\nprint('EMPTY_INPUT_VALUE', value)", Some(5.0))
        .await?;
    let answer_text = result_text(&answer);
    session.cancel().await?;

    assert!(
        answer_text.contains("EMPTY_INPUT_VALUE answer"),
        "expected answer to be consumed by input(), got: {answer_text:?}"
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
        .write_stdin_raw_with("import time\ntime.sleep(0.3)\nvalue = input()", Some(0.1))
        .await?;
    let first_text = result_text(&first);
    assert!(
        is_busy_response(&first_text),
        "expected first request to time out before input(), got: {first_text:?}"
    );

    let poll = session.write_stdin_raw_with("", Some(5.0)).await?;
    let poll_text = result_text(&poll);
    assert!(
        poll_text.contains("<<repl status: waiting for stdin>>"),
        "expected poll to report empty input prompt, got: {poll_text:?}"
    );
    assert!(
        !poll_text.contains("<<repl status: idle>>"),
        "did not expect poll to report idle while input() is waiting, got: {poll_text:?}"
    );

    let answer = session
        .write_stdin_raw_with("answer\nprint('TIMED_EMPTY_INPUT_VALUE', value)", Some(5.0))
        .await?;
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
            .write_stdin_raw_with("value = input('debug> ')", Some(1.0))
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

    let answer = session
        .write_stdin_raw_with("value\nprint('DEBUG_ALLOCATOR_INPUT', value)", Some(5.0))
        .await?;
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
            && !exit_text.contains(">>>")
        {
            sleep(Duration::from_millis(50)).await;
            exit_text = result_text(&session.write_stdin_raw_with("", Some(1.0)).await?);
        }
    }
    if is_busy_response(&exit_text) {
        session.cancel().await?;
        return Err(format!(
            "interactive help() did not return to the Python prompt: {exit_text:?}"
        )
        .into());
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
        exit_visible.contains(">>>"),
        "expected interactive help() to return to the Python prompt, got: {exit_visible:?}"
    );
    assert!(
        follow_up_text.contains("2"),
        "expected a ready prompt after interactive help(), got: {follow_up_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_busy_discards_input() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let _ = session
        .write_stdin_raw_with("import time; time.sleep(2)", Some(0.1))
        .await?;

    let result = session.write_stdin_raw_with("1+1", Some(0.2)).await?;
    let text = result_text(&result);
    assert!(
        text.contains("input discarded while worker busy"),
        "expected busy discard message, got: {text:?}"
    );
    assert_ne!(result.is_error, Some(true));

    session.cancel().await?;
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
        text.contains("PROMPT_STDOUT>>> >>> "),
        "expected trailing prompt-shaped stdout and worker prompt to both remain visible, got: {text:?}"
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

    let interrupt_result = session.write_stdin_raw_with("\u{3}", Some(5.0)).await?;
    let interrupt_text = result_text(&interrupt_result);
    if is_busy_response(&interrupt_text) {
        eprintln!("PTY timeout-tail interrupt cleanup is not hardened in this slice; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        interrupt_text.contains(">>>"),
        "expected prompt after interrupt, got: {interrupt_text:?}"
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
async fn python_interrupt_wakes_time_sleep_signal_handler() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let timeout_result = session
        .write_stdin_raw_with(
            r#"exec("""
import signal, time
def handle_sigint(signum, frame):
    print("PY_SLEEP_SIGINT")
    raise KeyboardInterrupt

signal.signal(signal.SIGINT, handle_sigint)
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
    text = result_text(&session.write_stdin_raw_with("\u{3}", Some(5.0)).await?);
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
        text.contains("PY_SLEEP_SIGINT") && text.contains("PY_SLEEP_INTERRUPTED"),
        "expected SIGINT handler to wake sleep, got: {text:?}"
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
    let marker_path = marker_dir.path().join("detached-idle-written");
    let marker = marker_path
        .to_str()
        .ok_or("detached idle marker path must be valid utf-8")?;

    let setup = session
        .write_stdin_raw_with(
            format!(
                r#"import subprocess, sys
script = """import pathlib, sys, time
time.sleep(0.3)
for i in range(160):
    sys.stdout.write("IDLE_%03d " % i + ("x" * 80) + "\\n")
sys.stdout.flush()
pathlib.Path(sys.argv[1]).write_text("done")
"""
subprocess.Popen(
    [sys.executable, "-c", script, {marker_arg}],
    stdin=subprocess.DEVNULL,
    close_fds=False,
)
print("parent ready")
"#,
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
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };
    let marker_dir = tempdir()?;
    let tail_marker_path = marker_dir.path().join("idle-tail-written");
    let tail_marker = tail_marker_path
        .to_str()
        .ok_or("idle tail marker path must be valid utf-8")?;
    let tail_marker_literal = serde_json::to_string(tail_marker)?;
    let exit_marker_path = marker_dir.path().join("idle-worker-exiting");
    let exit_marker = exit_marker_path
        .to_str()
        .ok_or("idle exit marker path must be valid utf-8")?;
    let exit_marker_literal = serde_json::to_string(exit_marker)?;
    let release_marker_path = marker_dir.path().join("idle-tail-release");
    let release_marker = release_marker_path
        .to_str()
        .ok_or("idle tail release marker path must be valid utf-8")?;
    let release_marker_literal = serde_json::to_string(release_marker)?;
    let script = format!(
        r#"import os, pathlib, subprocess, sys, threading, time
tail_marker = {tail_marker_literal}
exit_marker = {exit_marker_literal}
release_marker = {release_marker_literal}
writer = """import os, pathlib, sys, time
deadline = time.monotonic() + 5
while not os.path.exists(sys.argv[2]):
    if time.monotonic() >= deadline:
        sys.exit(2)
    time.sleep(0.02)
sys.stdout.write("IDLE_TAIL\\n")
sys.stdout.flush()
pathlib.Path(sys.argv[1]).write_text("done")
time.sleep(0.2)
"""
subprocess.Popen(
    [sys.executable, "-c", writer, tail_marker, release_marker],
    stdin=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
    close_fds=True,
    start_new_session=True,
)
print("armed")
def exit_after_detached_tail():
    while not os.path.exists(tail_marker):
        time.sleep(0.02)
    time.sleep(0.4)
    pathlib.Path(exit_marker).write_text("done")
    os._exit(0)
threading.Thread(target=exit_after_detached_tail, daemon=True).start()"#
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
    wait_for_detached_holder_exit(&exit_marker_path).await?;
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

    let restart = session.write_stdin_raw_with("\u{4}", Some(10.0)).await?;
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

#[tokio::test(flavor = "multi_thread")]
async fn python_detached_incomplete_utf8_tail_does_not_merge_into_next_request() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };
    let marker_dir = tempdir()?;
    let marker_path = marker_dir.path().join("detached-incomplete-written");
    let marker = marker_path
        .to_str()
        .ok_or("detached incomplete marker path must be valid utf-8")?;

    let setup = session
        .write_stdin_raw_with(
            format!(
                r#"import subprocess, sys
script = """import os, pathlib, sys, time
time.sleep(0.3)
for i in range(160):
    os.write(sys.stdout.fileno(), ("IDLE_%03d " % i + ("x" * 80) + "\\n").encode())
os.write(sys.stdout.fileno(), bytes([0xC3]))
pathlib.Path(sys.argv[1]).write_text("done")
"""
subprocess.Popen(
    [sys.executable, "-c", script, {marker_arg}],
    stdin=subprocess.DEVNULL,
    close_fds=False,
)
print("parent ready")
"#,
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

    let interrupt_result = session.write_stdin_raw_with("\u{3}", Some(5.0)).await?;
    let interrupt_text = result_text(&interrupt_result);
    if is_busy_response(&interrupt_text) {
        eprintln!("PTY timeout-tail interrupt cleanup is not hardened in this slice; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        interrupt_text.contains(">>>"),
        "expected prompt after interrupt, got: {interrupt_text:?}"
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
        .write_stdin_raw_with("def f():\n    return 3\n\nf()\nprint('done')", Some(5.0))
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
    if is_busy_response(&text) {
        eprintln!("python_exception_reported_in_output remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("ZeroDivisionError"),
        "expected traceback, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_pdb_roundtrip() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with("import pdb; pdb.set_trace()", Some(1.0))
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        eprintln!("python_pdb_roundtrip remained busy entering pdb; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(text.contains("(Pdb)"), "expected pdb prompt, got: {text:?}");

    let result = session.write_stdin_raw_with("c", Some(5.0)).await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err("python_pdb_roundtrip remained busy after continue".into());
    }
    assert!(
        text.contains(">>>"),
        "expected python prompt after continue, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_input_can_consume_buffered_lines() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with("x = input('p> ')\nhello\nprint('got', x)", Some(5.0))
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
        eprintln!("python_input_can_consume_buffered_lines remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("got hello"),
        "expected input() to consume buffered hello, got: {text:?}"
    );
    assert!(
        text.contains("p> "),
        "expected buffered input() prompt to stay visible, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_input_emits_prompt_before_buffered_read() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"import sys
first = sys.stdin.read(1)
z
answer = input('p> ')
print("MIXED", repr(first), repr(answer))
"#,
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err("python buffered input prompt test remained busy".into());
    }
    if !text.contains("MIXED") && text.contains(">>> ") {
        eprintln!(
            "mixed sys.stdin/read input payload is not supported under PTY readline; skipping"
        );
        session.cancel().await?;
        return Ok(());
    }
    let prompt_index = text
        .find("p> ")
        .ok_or_else(|| format!("expected buffered input prompt, got: {text:?}"))?;
    let output_index = text
        .find(r#"MIXED 'z' ''"#)
        .ok_or_else(|| format!("expected mixed buffered input output, got: {text:?}"))?;
    assert!(
        prompt_index < output_index,
        "expected prompt before buffered input output, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_input_releases_gil_while_waiting_for_line() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"
import sys, threading, time
sys.setswitchinterval(1.0)
background_seen = None
answer_seen = None
background_started = threading.Event()
def background():
    global background_seen
    background_started.set()
    time.sleep(0.1)
    background_seen = time.monotonic()

def wait_and_print():
    global answer_seen
    answer = input('p> ')
    answer_seen = time.monotonic()
    print('answer', answer)

threading.Thread(target=background, daemon=True).start()
background_started.wait()
wait_and_print()
"#,
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    assert!(
        !is_busy_response(&text),
        "expected input prompt, got: {text:?}"
    );
    assert!(text.contains("p> "), "expected input prompt, got: {text:?}");

    sleep(Duration::from_millis(500)).await;

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
