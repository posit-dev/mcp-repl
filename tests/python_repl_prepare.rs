mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::TestResult;
use serde_json::{Value, json};

fn current_python_executable() -> TestResult<PathBuf> {
    let program =
        common::python_program().ok_or("python is required for python repl_prepare tests")?;
    let output = Command::new(program)
        .args(["-c", "import sys; print(sys.executable)"])
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "failed to resolve python executable: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(PathBuf::from(
        String::from_utf8(output.stdout)?.trim().to_string(),
    ))
}

fn venv_python(venv: &Path) -> PathBuf {
    if cfg!(windows) {
        venv.join("Scripts").join("python.exe")
    } else {
        venv.join("bin").join("python")
    }
}

fn create_venv(root: &Path, name: &str, packages: &[&str]) -> TestResult<PathBuf> {
    let python = current_python_executable()?;
    let venv = root.join(name);
    let status = Command::new(&python)
        .args(["-m", "venv"])
        .arg(&venv)
        .status()?;
    if !status.success() {
        return Err(format!("failed to create test venv at {}", venv.display()).into());
    }

    let venv_python = venv_python(&venv);
    let output = Command::new(&venv_python)
        .args([
            "-c",
            "import sysconfig; print(sysconfig.get_paths()['purelib'])",
        ])
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "failed to locate test venv site-packages: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    let site_packages = PathBuf::from(String::from_utf8(output.stdout)?.trim().to_string());
    for package in packages {
        let package_dir = site_packages.join(package);
        fs::create_dir_all(&package_dir)?;
        fs::write(
            package_dir.join("__init__.py"),
            format!("MARKER = {package:?}\n"),
        )?;
        let dist_info_dir = site_packages.join(format!("{package}-1.0.dist-info"));
        fs::create_dir_all(&dist_info_dir)?;
        fs::write(
            dist_info_dir.join("METADATA"),
            format!("Name: {package}\nVersion: 1.0\n"),
        )?;
    }

    Ok(venv)
}

#[cfg(unix)]
fn make_executable(path: &Path) -> TestResult<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> TestResult<()> {
    Ok(())
}

fn write_fake_uv(bin_dir: &Path) -> TestResult<PathBuf> {
    fs::create_dir_all(bin_dir)?;
    let uv = if cfg!(windows) {
        bin_dir.join("uv.cmd")
    } else {
        bin_dir.join("uv")
    };

    if cfg!(windows) {
        fs::write(
            &uv,
            r#"@echo off
set venv=%MCP_REPL_TEST_STDLIB_VENV%
:loop
if "%~1"=="" goto run
if "%~1"=="--with" (
  shift
  if "%~1"=="numpy" set venv=%MCP_REPL_TEST_NUMPY_VENV%
  if "%~1"=="plotnine" set venv=%MCP_REPL_TEST_PLOTNINE_VENV%
  shift
  goto loop
)
if "%~1"=="--" (
  shift
  goto run
)
shift
goto loop
:run
if "%~1"=="python" shift
call "%venv%\Scripts\python.exe" %*
"#,
        )?;
    } else {
        fs::write(
            &uv,
            r#"#!/bin/sh
set -eu
if [ -n "${MCP_REPL_TEST_UV_LOG:-}" ]; then
  printf '%s\n' "$*" >> "$MCP_REPL_TEST_UV_LOG"
fi
venv="$MCP_REPL_TEST_STDLIB_VENV"
while [ "$#" -gt 0 ]; do
  case "$1" in
    --with)
      shift
      case "${1:-}" in
        numpy) venv="$MCP_REPL_TEST_NUMPY_VENV" ;;
        plotnine) venv="$MCP_REPL_TEST_PLOTNINE_VENV" ;;
      esac
      ;;
    --)
      shift
      break
      ;;
  esac
  shift
done
if [ -n "${MCP_REPL_TEST_UV_LOG:-}" ]; then
  printf 'venv=%s\n' "$venv" >> "$MCP_REPL_TEST_UV_LOG"
fi
if [ "${1:-}" = "python" ]; then
  shift
fi
exec "$venv/bin/python" "$@"
"#,
        )?;
        make_executable(&uv)?;
    }

    Ok(uv)
}

struct FakeUv {
    _tempdir: tempfile::TempDir,
    bin_dir: PathBuf,
    stdlib_venv: PathBuf,
    numpy_venv: PathBuf,
    plotnine_venv: PathBuf,
    log_path: PathBuf,
}

impl FakeUv {
    fn new() -> TestResult<Self> {
        let tempdir = tempfile::tempdir()?;
        let root = tempdir.path();
        let stdlib_venv = create_venv(root, "stdlib", &[])?;
        let numpy_venv = create_venv(root, "numpy", &["numpy"])?;
        let plotnine_venv = create_venv(root, "plotnine", &["plotnine"])?;
        let bin_dir = root.join("bin");
        let log_path = root.join("uv.log");
        write_fake_uv(&bin_dir)?;
        Ok(Self {
            _tempdir: tempdir,
            bin_dir,
            stdlib_venv,
            numpy_venv,
            plotnine_venv,
            log_path,
        })
    }

    fn env_vars(&self) -> TestResult<Vec<(String, String)>> {
        self.env_vars_with_python(current_python_executable()?)
    }

    fn env_vars_with_python(&self, python: PathBuf) -> TestResult<Vec<(String, String)>> {
        let mut path_entries = vec![self.bin_dir.clone()];
        if let Some(path) = std::env::var_os("PATH") {
            path_entries.extend(std::env::split_paths(&path));
        }
        let path = std::env::join_paths(path_entries)?;
        Ok(vec![
            ("PATH".to_string(), path.to_string_lossy().to_string()),
            (
                "MCP_REPL_PYTHON_EXECUTABLE".to_string(),
                python.to_string_lossy().to_string(),
            ),
            (
                "MCP_REPL_TEST_STDLIB_VENV".to_string(),
                self.stdlib_venv.to_string_lossy().to_string(),
            ),
            (
                "MCP_REPL_TEST_NUMPY_VENV".to_string(),
                self.numpy_venv.to_string_lossy().to_string(),
            ),
            (
                "MCP_REPL_TEST_PLOTNINE_VENV".to_string(),
                self.plotnine_venv.to_string_lossy().to_string(),
            ),
            (
                "MCP_REPL_TEST_UV_LOG".to_string(),
                self.log_path.to_string_lossy().to_string(),
            ),
        ])
    }

    fn uv_log(&self) -> TestResult<String> {
        Ok(fs::read_to_string(&self.log_path).unwrap_or_default())
    }
}

fn no_uv_env_vars() -> TestResult<Vec<(String, String)>> {
    let tempdir = tempfile::tempdir()?;
    let path_dir = tempdir.path().to_path_buf();
    let python = current_python_executable()?;
    Ok(vec![
        ("PATH".to_string(), path_dir.to_string_lossy().to_string()),
        (
            "MCP_REPL_PYTHON_EXECUTABLE".to_string(),
            python.to_string_lossy().to_string(),
        ),
    ])
}

fn assert_call_error(result: Result<rmcp::model::CallToolResult, rmcp::service::ServiceError>) {
    if let Ok(result) = result {
        assert_eq!(
            result.is_error,
            Some(true),
            "expected tool result error, got {result:?}"
        );
    }
}

async fn call_prepare(
    session: &common::McpTestSession,
    arguments: Value,
) -> Result<rmcp::model::CallToolResult, rmcp::service::ServiceError> {
    session.call_tool_raw("repl_prepare", arguments).await
}

#[tokio::test(flavor = "multi_thread")]
async fn python_tool_listing_is_gated_by_uv() -> TestResult<()> {
    let fake_uv = FakeUv::new()?;
    let session = common::spawn_python_server_with_files_env_vars(fake_uv.env_vars()?).await?;
    let tool_names = session.list_tool_names().await?;
    session.cancel().await?;

    assert!(tool_names.contains(&"repl".to_string()));
    assert!(tool_names.contains(&"repl_prepare".to_string()));
    assert!(!tool_names.contains(&"repl_reset".to_string()));

    let session = common::spawn_python_server_with_files_env_vars(no_uv_env_vars()?).await?;
    let tool_names = session.list_tool_names().await?;
    session.cancel().await?;

    assert!(tool_names.contains(&"repl".to_string()));
    assert!(!tool_names.contains(&"repl_prepare".to_string()));
    assert!(!tool_names.contains(&"repl_reset".to_string()));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn r_tool_listing_keeps_repl_reset() -> TestResult<()> {
    let session = common::spawn_server_with_files().await?;
    let tool_names = session.list_tool_names().await?;
    session.cancel().await?;

    assert!(tool_names.contains(&"repl".to_string()));
    assert!(tool_names.contains(&"repl_reset".to_string()));
    assert!(!tool_names.contains(&"repl_prepare".to_string()));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn repl_prepare_rejects_invalid_shapes() -> TestResult<()> {
    let fake_uv = FakeUv::new()?;
    let session = common::spawn_python_server_with_files_env_vars(fake_uv.env_vars()?).await?;
    let absolute = current_python_executable()?;
    let absolute = absolute.to_string_lossy().to_string();

    assert_call_error(
        call_prepare(
            &session,
            json!({
                "requirements": {},
                "python": { "executable": absolute }
            }),
        )
        .await,
    );
    assert_call_error(
        call_prepare(
            &session,
            json!({
                "python": {
                    "executable": absolute,
                    "venv": fake_uv.stdlib_venv.to_string_lossy()
                }
            }),
        )
        .await,
    );
    assert_call_error(
        call_prepare(&session, json!({ "python": { "executable": "python" } })).await,
    );
    assert_call_error(call_prepare(&session, json!({ "python": { "venv": ".venv" } })).await);
    assert_call_error(call_prepare(&session, json!({ "timeout_ms": 1000 })).await);

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn repl_prepare_accepts_executable_and_venv_paths() -> TestResult<()> {
    let fake_uv = FakeUv::new()?;
    let session = common::spawn_python_server_with_files_env_vars(fake_uv.env_vars()?).await?;
    let executable = current_python_executable()?;

    let executable_result =
        call_prepare(&session, json!({ "python": { "executable": executable } })).await?;
    let executable_text = common::result_text(&executable_result);
    assert!(
        executable_text.contains("session unchanged")
            || executable_text.contains("session replaced"),
        "expected prepare status, got: {executable_text:?}"
    );

    let venv_result = call_prepare(
        &session,
        json!({ "python": { "venv": fake_uv.stdlib_venv.to_string_lossy() } }),
    )
    .await?;
    let venv_text = common::result_text(&venv_result);
    assert!(
        venv_text.contains("session unchanged") || venv_text.contains("session replaced"),
        "expected prepare status, got: {venv_text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn repl_prepare_forwards_python_version_values_to_uv() -> TestResult<()> {
    let fake_uv = FakeUv::new()?;
    let session = common::spawn_python_server_with_files_env_vars(fake_uv.env_vars()?).await?;

    for python_version in ["3.11", "3.11.4", ">=3.11,<3.13"] {
        let result = call_prepare(
            &session,
            json!({
                "requirements": {
                    "packages": [],
                    "python_version": python_version
                }
            }),
        )
        .await?;
        assert_ne!(
            result.is_error,
            Some(true),
            "prepare failed for {python_version}"
        );
    }
    session.cancel().await?;

    let uv_log = fake_uv.uv_log()?;
    assert!(uv_log.contains("--python 3.11"), "uv log: {uv_log:?}");
    assert!(uv_log.contains("--python 3.11.4"), "uv log: {uv_log:?}");
    assert!(
        uv_log.contains("--python >=3.11,<3.13"),
        "uv log: {uv_log:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn repl_prepare_requirements_control_ephemeral_packages() -> TestResult<()> {
    let fake_uv = FakeUv::new()?;
    let session = common::spawn_python_server_with_files_env_vars(
        fake_uv.env_vars_with_python(venv_python(&fake_uv.stdlib_venv))?,
    )
    .await?;

    let default_result = call_prepare(&session, json!({})).await?;
    let default_text = common::result_text(&default_result);
    assert!(
        default_text.contains("session unchanged") || default_text.contains("session replaced"),
        "expected prepare status, got: {default_text:?}"
    );
    let numpy = session
        .write_stdin_raw_with("import numpy; print('NUMPY:' + numpy.MARKER)", Some(5.0))
        .await?;
    let numpy_text = common::result_text(&numpy);
    assert!(
        numpy_text.contains("NUMPY:numpy"),
        "default prepare should make numpy available, got: {numpy_text:?}"
    );

    let stdlib_result = call_prepare(&session, json!({ "requirements": {} })).await?;
    let stdlib_text = common::result_text(&stdlib_result);
    assert!(
        stdlib_text.contains("session replaced") || stdlib_text.contains("session unchanged"),
        "expected prepare status, got: {stdlib_text:?}"
    );
    let stdlib_probe = session
        .write_stdin_raw_with(
            "import importlib.util; print('NUMPY_SPEC:' + str(importlib.util.find_spec('numpy') is not None))",
            Some(5.0),
        )
        .await?;
    let stdlib_probe_text = common::result_text(&stdlib_probe);
    assert!(
        stdlib_probe_text.contains("NUMPY_SPEC:False"),
        "stdlib-only prepare should not add numpy, got: {stdlib_probe_text:?}"
    );

    let exact_result = call_prepare(
        &session,
        json!({ "requirements": { "packages": ["plotnine"] } }),
    )
    .await?;
    let exact_text = common::result_text(&exact_result);
    assert!(
        exact_text.contains("session replaced") || exact_text.contains("session unchanged"),
        "expected prepare status, got: {exact_text:?}"
    );
    let exact_probe = session
        .write_stdin_raw_with(
            "import importlib.util, plotnine; print('PLOTNINE:' + plotnine.MARKER); print('NUMPY_SPEC:' + str(importlib.util.find_spec('numpy') is not None))",
            Some(5.0),
        )
        .await?;
    let exact_probe_text = common::result_text(&exact_probe);
    assert!(
        exact_probe_text.contains("PLOTNINE:plotnine"),
        "explicit package prepare should make plotnine available, got: {exact_probe_text:?}"
    );
    assert!(
        exact_probe_text.contains("NUMPY_SPEC:False"),
        "explicit package prepare should not add numpy defaults, got: {exact_probe_text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn repl_prepare_preserves_current_python_when_default_requirements_available()
-> TestResult<()> {
    let fake_uv = FakeUv::new()?;
    let session = common::spawn_python_server_with_files_env_vars(
        fake_uv.env_vars_with_python(venv_python(&fake_uv.numpy_venv))?,
    )
    .await?;

    let seed = session
        .write_stdin_raw_with("_prepare_marker = 'kept'", Some(5.0))
        .await?;
    let seed_text = common::result_text(&seed);
    assert!(
        !common::is_busy_response(&seed_text),
        "expected initial assignment to complete, got: {seed_text:?}"
    );

    let result = call_prepare(&session, json!({})).await?;
    let text = common::result_text(&result);
    assert!(
        text.contains("session unchanged"),
        "expected default requirements to preserve current Python, got: {text:?}"
    );

    let probe = session
        .write_stdin_raw_with(
            "import numpy; print('NUMPY:' + numpy.MARKER); print('MARKER:' + _prepare_marker)",
            Some(5.0),
        )
        .await?;
    let probe_text = common::result_text(&probe);
    assert!(
        probe_text.contains("NUMPY:numpy") && probe_text.contains("MARKER:kept"),
        "expected preserved numpy session, got: {probe_text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn repl_prepare_can_replace_active_session_and_reports_discarded_work() -> TestResult<()> {
    let fake_uv = FakeUv::new()?;
    let session = common::spawn_python_server_with_files_env_vars(fake_uv.env_vars()?).await?;

    let first = session
        .write_stdin_raw_with(
            "import time\nprint('PREPARE_BUSY_READY', flush=True)\ntime.sleep(60)",
            Some(0.5),
        )
        .await?;
    let first_text = common::result_text(&first);
    assert!(
        common::is_busy_response(&first_text) || first_text.contains("PREPARE_BUSY_READY"),
        "expected timed-out active work, got: {first_text:?}"
    );

    let result = call_prepare(&session, json!({ "requirements": {} })).await?;
    let text = common::result_text(&result);
    assert!(
        text.contains("session replaced"),
        "expected replacement status, got: {text:?}"
    );
    assert!(
        text.contains("pending work discarded"),
        "expected discarded-work status, got: {text:?}"
    );

    let follow_up = session
        .write_stdin_raw_with("print('AFTER_PREPARE_REPLACE')", Some(5.0))
        .await?;
    let follow_up_text = common::result_text(&follow_up);
    assert!(
        follow_up_text.contains("AFTER_PREPARE_REPLACE"),
        "expected fresh session after prepare, got: {follow_up_text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn repl_prepare_preserves_matching_executable_session() -> TestResult<()> {
    let fake_uv = FakeUv::new()?;
    let session = common::spawn_python_server_with_files_env_vars(fake_uv.env_vars()?).await?;
    let executable = current_python_executable()?;

    let seed = session
        .write_stdin_raw_with("_prepare_marker = 'kept'", Some(5.0))
        .await?;
    let seed_text = common::result_text(&seed);
    assert!(
        !common::is_busy_response(&seed_text),
        "expected initial assignment to complete, got: {seed_text:?}"
    );

    let result = call_prepare(&session, json!({ "python": { "executable": executable } })).await?;
    let text = common::result_text(&result);
    assert!(
        text.contains("session unchanged"),
        "expected unchanged status, got: {text:?}"
    );
    assert!(
        text.contains("no pending work discarded"),
        "expected no-discard status, got: {text:?}"
    );

    let probe = session
        .write_stdin_raw_with("print('MARKER:' + _prepare_marker)", Some(5.0))
        .await?;
    let probe_text = common::result_text(&probe);
    assert!(
        probe_text.contains("MARKER:kept"),
        "expected matching prepare to preserve session, got: {probe_text:?}"
    );

    session.cancel().await?;
    Ok(())
}
