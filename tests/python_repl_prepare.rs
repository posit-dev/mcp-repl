mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;

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

const EXTRA_PACKAGE: &str = "packaging";

fn run_uv(mut command: Command, context: impl Into<String>) -> TestResult<Output> {
    let context = context.into();
    let output = command
        .stdin(Stdio::null())
        .output()
        .map_err(|err| format!("{context}: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "{context}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(output)
}

fn ensure_uv_available() -> TestResult<()> {
    let mut command = Command::new("uv");
    command.arg("--version");
    run_uv(command, "uv is required for python repl_prepare tests")?;
    Ok(())
}

fn create_venv(root: &Path, name: &str) -> TestResult<PathBuf> {
    let venv = root.join(name);
    let mut command = Command::new("uv");
    command.args(["venv", "--no-project"]).arg(&venv);
    run_uv(
        command,
        format!("failed to create test venv at {}", venv.display()),
    )?;
    Ok(venv)
}

fn resolve_uv_python(packages: &[&str], python_version: Option<&str>) -> TestResult<PathBuf> {
    let mut command = Command::new("uv");
    command.args(["tool", "run", "--isolated"]);
    if let Some(python_version) = python_version {
        command.arg("--python").arg(python_version);
    }
    for package in packages {
        command.arg("--with").arg(package);
    }
    command.args([
        "--",
        "python",
        "-I",
        "-c",
        "import sys; print(sys.executable)",
    ]);
    let output = run_uv(command, "failed to resolve uv-managed Python")?;
    let stdout = String::from_utf8(output.stdout)?;
    let executable = stdout
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .ok_or("uv did not report a Python executable")?;
    Ok(PathBuf::from(executable.trim()))
}

fn current_python_version() -> TestResult<(u32, u32, u32)> {
    let output = Command::new(current_python_executable()?)
        .args(["-c", "import sys; print('%d.%d.%d' % sys.version_info[:3])"])
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "failed to resolve current Python version: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    let stdout = String::from_utf8(output.stdout)?;
    let parts = stdout
        .trim()
        .split('.')
        .map(str::parse::<u32>)
        .collect::<Result<Vec<_>, _>>()?;
    if let [major, minor, patch] = parts.as_slice() {
        Ok((*major, *minor, *patch))
    } else {
        Err(format!("unexpected Python version output: {stdout:?}").into())
    }
}

struct RealUv {
    _tempdir: tempfile::TempDir,
    stdlib_venv: PathBuf,
}

fn real_uv_test_mutex() -> &'static tokio::sync::Mutex<()> {
    static TEST_MUTEX: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    TEST_MUTEX.get_or_init(|| tokio::sync::Mutex::new(()))
}

impl RealUv {
    fn new() -> TestResult<Self> {
        ensure_uv_available()?;
        let tempdir = tempfile::tempdir()?;
        let root = tempdir.path();
        let stdlib_venv = create_venv(root, "stdlib")?;
        Ok(Self {
            _tempdir: tempdir,
            stdlib_venv,
        })
    }

    async fn locked_new() -> TestResult<(tokio::sync::MutexGuard<'static, ()>, Self)> {
        let guard = real_uv_test_mutex().lock().await;
        let uv = Self::new()?;
        Ok((guard, uv))
    }

    fn env_vars(&self) -> TestResult<Vec<(String, String)>> {
        self.env_vars_with_python(current_python_executable()?)
    }

    fn env_vars_with_python(&self, python: PathBuf) -> TestResult<Vec<(String, String)>> {
        Ok(vec![(
            "MCP_REPL_PYTHON_EXECUTABLE".to_string(),
            python.to_string_lossy().to_string(),
        )])
    }

    fn managed_python(&self, packages: &[&str]) -> TestResult<PathBuf> {
        resolve_uv_python(packages, None)
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

fn assert_manifest_packages(text: &str, packages: &[&str]) {
    let expected = format!("packages={}", serde_json::to_string(packages).unwrap());
    assert!(
        text.contains(&expected),
        "expected manifest {expected}, got: {text:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn python_tool_listing_is_gated_by_uv() -> TestResult<()> {
    let (_uv_guard, uv_env) = RealUv::locked_new().await?;
    let session = common::spawn_python_server_with_files_env_vars(uv_env.env_vars()?).await?;
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
async fn repl_prepare_schema_exposes_manifest_action_and_restart_enums() -> TestResult<()> {
    let (_uv_guard, uv_env) = RealUv::locked_new().await?;
    let session = common::spawn_python_server_with_files_env_vars(uv_env.env_vars()?).await?;
    let tools = session.list_tools().await?;
    session.cancel().await?;

    let prepare = tools
        .iter()
        .find(|tool| tool.name == "repl_prepare")
        .ok_or("missing repl_prepare tool")?;
    let schema = serde_json::to_string(&prepare.input_schema)?;
    for expected in [
        "\"action\"",
        "\"add\"",
        "\"remove\"",
        "\"set\"",
        "\"restart\"",
        "\"if_needed\"",
        "\"yes\"",
        "\"no\"",
    ] {
        assert!(
            schema.contains(expected),
            "expected schema to contain {expected}, got: {schema}"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn repl_prepare_rejects_invalid_shapes() -> TestResult<()> {
    let (_uv_guard, uv_env) = RealUv::locked_new().await?;
    let session = common::spawn_python_server_with_files_env_vars(uv_env.env_vars()?).await?;
    let absolute = current_python_executable()?;
    let absolute = absolute.to_string_lossy().to_string();

    assert_call_error(
        call_prepare(
            &session,
            json!({
                "requirements": {},
                "python": { "executable": absolute.clone() }
            }),
        )
        .await,
    );
    assert_call_error(
        call_prepare(
            &session,
            json!({
                "python": {
                    "executable": absolute.clone(),
                    "venv": uv_env.stdlib_venv.to_string_lossy()
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
    assert_call_error(call_prepare(&session, json!({ "restart": "no" })).await);
    assert_call_error(
        call_prepare(&session, json!({ "requirements": { "action": "replace" } })).await,
    );
    assert_call_error(
        call_prepare(&session, json!({ "requirements": { "restart": "maybe" } })).await,
    );
    assert_call_error(
        call_prepare(
            &session,
            json!({ "python": { "executable": absolute, "restart": "no" } }),
        )
        .await,
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn repl_prepare_accepts_executable_and_venv_paths() -> TestResult<()> {
    let (_uv_guard, uv_env) = RealUv::locked_new().await?;
    let session = common::spawn_python_server_with_files_env_vars(uv_env.env_vars()?).await?;
    let executable = current_python_executable()?;

    let executable_result =
        call_prepare(&session, json!({ "python": { "executable": executable } })).await?;
    let executable_text = common::result_text(&executable_result);
    assert!(
        executable_text.contains("session unchanged")
            || executable_text.contains("session restarted"),
        "expected prepare status, got: {executable_text:?}"
    );
    assert_manifest_packages(&executable_text, &["numpy"]);

    let venv_result = call_prepare(
        &session,
        json!({ "python": { "venv": uv_env.stdlib_venv.to_string_lossy() } }),
    )
    .await?;
    let venv_text = common::result_text(&venv_result);
    assert!(
        venv_text.contains("session unchanged") || venv_text.contains("session restarted"),
        "expected prepare status, got: {venv_text:?}"
    );
    assert_manifest_packages(&venv_text, &["numpy"]);

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn repl_prepare_forwards_python_version_values_to_uv() -> TestResult<()> {
    let (_uv_guard, uv_env) = RealUv::locked_new().await?;
    let session = common::spawn_python_server_with_files_env_vars(uv_env.env_vars()?).await?;
    let (major, minor, patch) = current_python_version()?;
    let major_minor = format!("{major}.{minor}");
    let exact = format!("{major}.{minor}.{patch}");
    let range = format!(">={major_minor},<{major}.{}", minor + 1);

    for (python_version, expected_version) in [
        (major_minor.clone(), format!("PY_VERSION:{major}.{minor}.")),
        (exact.clone(), format!("PY_VERSION:{exact}")),
        (range, format!("PY_VERSION:{major}.{minor}.")),
    ] {
        let result = call_prepare(
            &session,
            json!({
                "requirements": {
                    "packages": [],
                    "python_version": python_version.as_str(),
                    "action": "set"
                }
            }),
        )
        .await?;
        assert_ne!(
            result.is_error,
            Some(true),
            "prepare failed for {python_version}"
        );
        let probe = session
            .write_stdin_raw_with(
                "import sys; print('PY_VERSION:%d.%d.%d' % sys.version_info[:3])",
                Some(5.0),
            )
            .await?;
        let probe_text = common::result_text(&probe);
        assert!(
            probe_text.contains(&expected_version),
            "expected {python_version} to produce {expected_version}, got: {probe_text:?}"
        );
    }
    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn repl_prepare_requirement_actions_update_persistent_manifest() -> TestResult<()> {
    let (_uv_guard, uv_env) = RealUv::locked_new().await?;
    let session = common::spawn_python_server_with_files_env_vars(
        uv_env.env_vars_with_python(venv_python(&uv_env.stdlib_venv))?,
    )
    .await?;

    let default_result = call_prepare(&session, json!({})).await?;
    let default_text = common::result_text(&default_result);
    assert!(
        default_text.contains("session unchanged") || default_text.contains("session restarted"),
        "expected prepare status, got: {default_text:?}"
    );
    assert_manifest_packages(&default_text, &["numpy"]);
    let numpy = session
        .write_stdin_raw_with(
            "import numpy; print('NUMPY_OK:' + str(hasattr(numpy, '__version__')))",
            Some(5.0),
        )
        .await?;
    let numpy_text = common::result_text(&numpy);
    assert!(
        numpy_text.contains("NUMPY_OK:True"),
        "default prepare should make numpy available, got: {numpy_text:?}"
    );

    let noop_result = call_prepare(&session, json!({ "requirements": {} })).await?;
    let noop_text = common::result_text(&noop_result);
    assert!(
        noop_text.contains("session restarted") || noop_text.contains("session unchanged"),
        "expected prepare status, got: {noop_text:?}"
    );
    assert_manifest_packages(&noop_text, &["numpy"]);
    let noop_probe = session
        .write_stdin_raw_with(
            "import numpy; print('NUMPY_OK:' + str(hasattr(numpy, '__version__')))",
            Some(5.0),
        )
        .await?;
    let noop_probe_text = common::result_text(&noop_probe);
    assert!(
        noop_probe_text.contains("NUMPY_OK:True"),
        "requirements no-op should keep numpy, got: {noop_probe_text:?}"
    );

    let add_result = call_prepare(
        &session,
        json!({ "requirements": { "packages": [EXTRA_PACKAGE] } }),
    )
    .await?;
    let add_text = common::result_text(&add_result);
    assert!(
        add_text.contains("session restarted") || add_text.contains("session unchanged"),
        "expected prepare status, got: {add_text:?}"
    );
    assert_manifest_packages(&add_text, &["numpy", EXTRA_PACKAGE]);
    let add_probe = session
        .write_stdin_raw_with(
            "import numpy, packaging; print('NUMPY_OK:' + str(hasattr(numpy, '__version__'))); print('PACKAGING_OK:' + str(hasattr(packaging, '__version__')))",
            Some(5.0),
        )
        .await?;
    let add_probe_text = common::result_text(&add_probe);
    assert!(
        add_probe_text.contains("NUMPY_OK:True") && add_probe_text.contains("PACKAGING_OK:True"),
        "add should make both manifest packages available, got: {add_probe_text:?}"
    );

    let remove_result = call_prepare(
        &session,
        json!({ "requirements": { "packages": [EXTRA_PACKAGE], "action": "remove" } }),
    )
    .await?;
    let remove_text = common::result_text(&remove_result);
    assert_manifest_packages(&remove_text, &["numpy"]);
    let remove_probe = session
        .write_stdin_raw_with(
            "import importlib.util, numpy; print('NUMPY_OK:' + str(hasattr(numpy, '__version__'))); print('PACKAGING_SPEC:' + str(importlib.util.find_spec('packaging') is not None))",
            Some(5.0),
        )
        .await?;
    let remove_probe_text = common::result_text(&remove_probe);
    assert!(
        remove_probe_text.contains("NUMPY_OK:True")
            && remove_probe_text.contains("PACKAGING_SPEC:False"),
        "remove should keep only numpy, got: {remove_probe_text:?}"
    );

    let set_result = call_prepare(
        &session,
        json!({ "requirements": { "packages": [], "action": "set", "restart": "yes" } }),
    )
    .await?;
    let set_text = common::result_text(&set_result);
    assert_manifest_packages(&set_text, &[]);
    let set_probe = session
        .write_stdin_raw_with(
            "import importlib.util; print('NUMPY_SPEC:' + str(importlib.util.find_spec('numpy') is not None))",
            Some(5.0),
        )
        .await?;
    let set_probe_text = common::result_text(&set_probe);
    assert!(
        set_probe_text.contains("NUMPY_SPEC:False"),
        "set empty should produce stdlib-only environment, got: {set_probe_text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn repl_prepare_preserves_current_python_when_manifest_available() -> TestResult<()> {
    let (_uv_guard, uv_env) = RealUv::locked_new().await?;
    let numpy_python = uv_env.managed_python(&["numpy"])?;
    let session =
        common::spawn_python_server_with_files_env_vars(uv_env.env_vars_with_python(numpy_python)?)
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
    assert_manifest_packages(&text, &["numpy"]);

    let probe = session
        .write_stdin_raw_with(
            "import numpy; print('NUMPY_OK:' + str(hasattr(numpy, '__version__'))); print('MARKER:' + _prepare_marker)",
            Some(5.0),
        )
        .await?;
    let probe_text = common::result_text(&probe);
    assert!(
        probe_text.contains("NUMPY_OK:True") && probe_text.contains("MARKER:kept"),
        "expected preserved numpy session, got: {probe_text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn repl_prepare_restart_no_fails_without_discarding_user_state() -> TestResult<()> {
    let (_uv_guard, uv_env) = RealUv::locked_new().await?;
    let session = common::spawn_python_server_with_files_env_vars(
        uv_env.env_vars_with_python(venv_python(&uv_env.stdlib_venv))?,
    )
    .await?;

    let initial = call_prepare(&session, json!({ "requirements": { "restart": "no" } })).await?;
    let initial_text = common::result_text(&initial);
    assert_manifest_packages(&initial_text, &["numpy"]);

    let seed = session
        .write_stdin_raw_with("_prepare_marker = 'kept'", Some(5.0))
        .await?;
    let seed_text = common::result_text(&seed);
    assert!(
        !common::is_busy_response(&seed_text),
        "expected initial assignment to complete, got: {seed_text:?}"
    );

    let result = call_prepare(
        &session,
        json!({ "requirements": { "packages": [EXTRA_PACKAGE], "restart": "no" } }),
    )
    .await?;
    assert_eq!(
        result.is_error,
        Some(true),
        "expected restart=no to fail unchanged when restart is required"
    );
    let text = common::result_text(&result);
    assert!(
        text.contains("session unchanged") && text.contains("no user state discarded"),
        "expected unchanged no-discard status, got: {text:?}"
    );
    assert_manifest_packages(&text, &["numpy"]);

    let probe = session
        .write_stdin_raw_with(
            "import importlib.util, numpy; print('MARKER:' + _prepare_marker); print('NUMPY_OK:' + str(hasattr(numpy, '__version__'))); print('PACKAGING_SPEC:' + str(importlib.util.find_spec('packaging') is not None))",
            Some(5.0),
        )
        .await?;
    let probe_text = common::result_text(&probe);
    assert!(
        probe_text.contains("MARKER:kept")
            && probe_text.contains("NUMPY_OK:True")
            && probe_text.contains("PACKAGING_SPEC:False"),
        "restart=no should preserve old session and manifest, got: {probe_text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn repl_prepare_default_restart_if_needed_restarts_and_commits_manifest() -> TestResult<()> {
    let (_uv_guard, uv_env) = RealUv::locked_new().await?;
    let session = common::spawn_python_server_with_files_env_vars(
        uv_env.env_vars_with_python(venv_python(&uv_env.stdlib_venv))?,
    )
    .await?;

    call_prepare(&session, json!({})).await?;
    let seed = session
        .write_stdin_raw_with("_prepare_marker = 'discarded'", Some(5.0))
        .await?;
    let seed_text = common::result_text(&seed);
    assert!(
        !common::is_busy_response(&seed_text),
        "expected initial assignment to complete, got: {seed_text:?}"
    );

    let result = call_prepare(
        &session,
        json!({ "requirements": { "packages": [EXTRA_PACKAGE] } }),
    )
    .await?;
    let text = common::result_text(&result);
    assert!(
        text.contains("session restarted") && text.contains("user state discarded"),
        "expected restart status, got: {text:?}"
    );
    assert_manifest_packages(&text, &["numpy", EXTRA_PACKAGE]);

    let probe = session
        .write_stdin_raw_with(
            "import numpy, packaging; print('NUMPY_OK:' + str(hasattr(numpy, '__version__'))); print('PACKAGING_OK:' + str(hasattr(packaging, '__version__'))); print('MARKER_EXISTS:' + str('_prepare_marker' in globals()))",
            Some(5.0),
        )
        .await?;
    let probe_text = common::result_text(&probe);
    assert!(
        probe_text.contains("NUMPY_OK:True")
            && probe_text.contains("PACKAGING_OK:True")
            && probe_text.contains("MARKER_EXISTS:False"),
        "default restart policy should commit manifest in fresh session, got: {probe_text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn repl_prepare_restart_yes_restarts_even_when_manifest_available() -> TestResult<()> {
    let (_uv_guard, uv_env) = RealUv::locked_new().await?;
    let numpy_python = uv_env.managed_python(&["numpy"])?;
    let session =
        common::spawn_python_server_with_files_env_vars(uv_env.env_vars_with_python(numpy_python)?)
            .await?;

    let seed = session
        .write_stdin_raw_with("_prepare_marker = 'discarded'", Some(5.0))
        .await?;
    let seed_text = common::result_text(&seed);
    assert!(
        !common::is_busy_response(&seed_text),
        "expected initial assignment to complete, got: {seed_text:?}"
    );

    let result = call_prepare(&session, json!({ "requirements": { "restart": "yes" } })).await?;
    let text = common::result_text(&result);
    assert!(
        text.contains("session restarted") && text.contains("user state discarded"),
        "expected forced restart status, got: {text:?}"
    );
    assert_manifest_packages(&text, &["numpy"]);

    let probe = session
        .write_stdin_raw_with(
            "import numpy; print('NUMPY_OK:' + str(hasattr(numpy, '__version__'))); print('MARKER_EXISTS:' + str('_prepare_marker' in globals()))",
            Some(5.0),
        )
        .await?;
    let probe_text = common::result_text(&probe);
    assert!(
        probe_text.contains("NUMPY_OK:True") && probe_text.contains("MARKER_EXISTS:False"),
        "forced restart should discard previous state, got: {probe_text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn repl_prepare_can_replace_active_session_and_reports_discarded_work() -> TestResult<()> {
    let (_uv_guard, uv_env) = RealUv::locked_new().await?;
    let session = common::spawn_python_server_with_files_env_vars(
        uv_env.env_vars_with_python(venv_python(&uv_env.stdlib_venv))?,
    )
    .await?;

    call_prepare(&session, json!({})).await?;

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

    let result = call_prepare(
        &session,
        json!({ "requirements": { "packages": [EXTRA_PACKAGE] } }),
    )
    .await?;
    let text = common::result_text(&result);
    assert!(
        text.contains("session restarted"),
        "expected replacement status, got: {text:?}"
    );
    assert!(
        text.contains("pending work discarded"),
        "expected discarded-work status, got: {text:?}"
    );
    assert_manifest_packages(&text, &["numpy", EXTRA_PACKAGE]);

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
async fn repl_prepare_explicit_python_does_not_mutate_managed_manifest() -> TestResult<()> {
    let (_uv_guard, uv_env) = RealUv::locked_new().await?;
    let session = common::spawn_python_server_with_files_env_vars(
        uv_env.env_vars_with_python(venv_python(&uv_env.stdlib_venv))?,
    )
    .await?;

    let explicit = call_prepare(
        &session,
        json!({ "python": { "venv": uv_env.stdlib_venv.to_string_lossy() } }),
    )
    .await?;
    let explicit_text = common::result_text(&explicit);
    assert_manifest_packages(&explicit_text, &["numpy"]);
    let explicit_probe = session
        .write_stdin_raw_with(
            "import importlib.util; print('NUMPY_SPEC:' + str(importlib.util.find_spec('numpy') is not None))",
            Some(5.0),
        )
        .await?;
    let explicit_probe_text = common::result_text(&explicit_probe);
    assert!(
        explicit_probe_text.contains("NUMPY_SPEC:False"),
        "explicit stdlib venv should be used as-is, got: {explicit_probe_text:?}"
    );

    let managed = call_prepare(&session, json!({})).await?;
    let managed_text = common::result_text(&managed);
    assert_manifest_packages(&managed_text, &["numpy"]);
    let managed_probe = session
        .write_stdin_raw_with(
            "import numpy; print('NUMPY_OK:' + str(hasattr(numpy, '__version__')))",
            Some(5.0),
        )
        .await?;
    let managed_probe_text = common::result_text(&managed_probe);
    assert!(
        managed_probe_text.contains("NUMPY_OK:True"),
        "managed manifest should survive explicit Python selection, got: {managed_probe_text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn repl_prepare_preserves_matching_executable_session() -> TestResult<()> {
    let (_uv_guard, uv_env) = RealUv::locked_new().await?;
    let session = common::spawn_python_server_with_files_env_vars(uv_env.env_vars()?).await?;
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
    assert_manifest_packages(&text, &["numpy"]);
    assert!(
        text.contains("no user state discarded"),
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
