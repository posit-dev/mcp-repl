mod common;

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
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
const SANDBOX_STATE_META_CAPABILITY: &str = "codex/sandbox-state-meta";

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

fn install_packages(python: &Path, packages: &[&str]) -> TestResult<()> {
    if packages.is_empty() {
        return Ok(());
    }
    let mut command = Command::new("uv");
    command.args(["pip", "install", "--python"]).arg(python);
    for package in packages {
        command.arg(package);
    }
    run_uv(
        command,
        format!("failed to install test packages into {}", python.display()),
    )?;
    Ok(())
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
    tempdir: tempfile::TempDir,
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
            tempdir,
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
        let venv = create_venv(self.tempdir.path(), "managed")?;
        let python = venv_python(&venv);
        install_packages(&python, packages)?;
        Ok(python)
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

fn sandbox_cwd_uri(sandbox_cwd: &Path) -> String {
    url::Url::from_file_path(sandbox_cwd)
        .map(|url| url.to_string())
        .unwrap_or_else(|_| panic!("failed to convert {} to file URI", sandbox_cwd.display()))
}

fn full_access_meta(sandbox_cwd: &Path) -> Value {
    json!({
        SANDBOX_STATE_META_CAPABILITY: {
            "permissionProfile": {
                "type": "disabled",
            },
            "sandboxCwd": sandbox_cwd_uri(sandbox_cwd),
            "useLegacyLandlock": false,
            "codexLinuxSandboxExe": if cfg!(target_os = "linux") {
                Value::String("/tmp/codex-linux-sandbox".to_string())
            } else {
                Value::Null
            },
        }
    })
}

async fn spawn_python_inherit_files_server(
    cwd: &Path,
    env_vars: Vec<(String, String)>,
) -> TestResult<common::McpTestSession> {
    common::spawn_server_with_args_env_and_cwd(
        vec![
            "--interpreter".to_string(),
            "python".to_string(),
            "--sandbox".to_string(),
            "inherit".to_string(),
            "--oversized-output".to_string(),
            "files".to_string(),
        ],
        env_vars,
        Some(cwd.to_path_buf()),
    )
    .await
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
#[cfg(unix)]
async fn repl_prepare_checks_non_bare_requirement_with_uv() -> TestResult<()> {
    let (_uv_guard, uv_env) = RealUv::locked_new().await?;
    let packaging_python = uv_env.managed_python(&[EXTRA_PACKAGE])?;
    let fake_uv_dir = tempfile::tempdir()?;
    let fake_uv = fake_uv_dir.path().join("uv");
    fs::write(
        &fake_uv,
        "#!/bin/sh\necho FAKE_UV_INVOKED_FOR_NON_BARE_REQUIREMENT >&2\nexit 7\n",
    )?;
    let mut permissions = fs::metadata(&fake_uv)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&fake_uv, permissions)?;

    let mut env_vars = uv_env.env_vars_with_python(packaging_python)?;
    env_vars.push((
        "PATH".to_string(),
        fake_uv_dir.path().to_string_lossy().to_string(),
    ));
    let session = common::spawn_python_server_with_files_env_vars(env_vars).await?;

    let result = call_prepare(
        &session,
        json!({
            "requirements": {
                "packages": ["packaging>=999999"],
                "action": "set",
                "restart": "no"
            }
        }),
    )
    .await?;
    let text = common::result_text(&result);
    assert_eq!(
        result.is_error,
        Some(true),
        "expected non-bare requirement to run uv and fail, got: {text:?}"
    );
    assert!(
        text.contains("FAKE_UV_INVOKED_FOR_NON_BARE_REQUIREMENT")
            && text.contains("session unchanged"),
        "expected uv failure while preserving the session, got: {text:?}"
    );
    assert_manifest_packages(&text, &["numpy"]);

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[cfg(unix)]
async fn repl_prepare_inherit_requires_meta_before_explicit_python_probe() -> TestResult<()> {
    let (_uv_guard, uv_env) = RealUv::locked_new().await?;
    let cwd = tempfile::tempdir()?;
    let fake_python_dir = tempfile::tempdir()?;
    let sentinel = fake_python_dir.path().join("probe-ran");
    let fake_python = fake_python_dir.path().join("python");
    fs::write(
        &fake_python,
        format!("#!/bin/sh\ntouch {}\nexit 7\n", sentinel.display()),
    )?;
    let mut permissions = fs::metadata(&fake_python)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&fake_python, permissions)?;

    let session = spawn_python_inherit_files_server(
        cwd.path(),
        uv_env.env_vars_with_python(venv_python(&uv_env.stdlib_venv))?,
    )
    .await?;

    let result = call_prepare(&session, json!({ "python": { "executable": fake_python } })).await?;
    let text = common::result_text(&result);
    assert_eq!(
        result.is_error,
        Some(true),
        "expected missing inherited metadata to reject prepare, got: {text:?}"
    );
    assert!(
        text.contains("sandbox inherit requested but no client sandbox state was provided"),
        "expected inherited metadata error before probing, got: {text:?}"
    );
    assert!(
        !sentinel.exists(),
        "explicit Python executable should not be probed before inherited metadata is accepted"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn repl_prepare_uses_sandbox_meta_before_inherit_replacement() -> TestResult<()> {
    let (_uv_guard, uv_env) = RealUv::locked_new().await?;
    let cwd = tempfile::tempdir()?;
    let session = spawn_python_inherit_files_server(
        cwd.path(),
        uv_env.env_vars_with_python(venv_python(&uv_env.stdlib_venv))?,
    )
    .await?;

    let result = session
        .call_tool_raw_with_meta(
            "repl_prepare",
            json!({
                "requirements": {
                    "packages": [EXTRA_PACKAGE],
                    "action": "set"
                }
            }),
            Some(full_access_meta(cwd.path())),
        )
        .await?;
    let text = common::result_text(&result);
    assert!(
        text.contains("session restarted") && !text.contains("sandbox inherit requested"),
        "expected inherited metadata to be staged before prepare replacement, got: {text:?}"
    );
    assert_manifest_packages(&text, &[EXTRA_PACKAGE]);

    let probe = session
        .write_stdin_raw_with_meta(
            "import packaging; print('PACKAGING_OK:' + str(hasattr(packaging, '__version__')))",
            Some(5.0),
            Some(full_access_meta(cwd.path())),
        )
        .await?;
    let probe_text = common::result_text(&probe);
    assert!(
        probe_text.contains("PACKAGING_OK:True"),
        "expected prepared worker to run under inherited metadata, got: {probe_text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn repl_prepare_narrowing_actions_do_not_accept_current_python_superset() -> TestResult<()> {
    let (_uv_guard, uv_env) = RealUv::locked_new().await?;
    let superset_python = uv_env.managed_python(&["numpy", EXTRA_PACKAGE])?;

    let set_session = common::spawn_python_server_with_files_env_vars(
        uv_env.env_vars_with_python(superset_python.clone())?,
    )
    .await?;
    let set_result = call_prepare(
        &set_session,
        json!({
            "requirements": {
                "packages": ["numpy"],
                "action": "set"
            }
        }),
    )
    .await?;
    let set_text = common::result_text(&set_result);
    assert_manifest_packages(&set_text, &["numpy"]);
    let set_probe = set_session
        .write_stdin_raw_with(
            "import importlib.util, numpy; print('NUMPY_OK:' + str(hasattr(numpy, '__version__'))); print('PACKAGING_SPEC:' + str(importlib.util.find_spec('packaging') is not None))",
            Some(5.0),
        )
        .await?;
    let set_probe_text = common::result_text(&set_probe);
    assert!(
        set_probe_text.contains("NUMPY_OK:True") && set_probe_text.contains("PACKAGING_SPEC:False"),
        "set should not keep packages omitted from the manifest, got: {set_probe_text:?}"
    );
    set_session.cancel().await?;

    let remove_session = common::spawn_python_server_with_files_env_vars(
        uv_env.env_vars_with_python(superset_python)?,
    )
    .await?;
    let add_result = call_prepare(
        &remove_session,
        json!({ "requirements": { "packages": [EXTRA_PACKAGE] } }),
    )
    .await?;
    let add_text = common::result_text(&add_result);
    assert_manifest_packages(&add_text, &["numpy", EXTRA_PACKAGE]);

    let remove_result = call_prepare(
        &remove_session,
        json!({ "requirements": { "packages": [EXTRA_PACKAGE], "action": "remove" } }),
    )
    .await?;
    let remove_text = common::result_text(&remove_result);
    assert_manifest_packages(&remove_text, &["numpy"]);
    let remove_probe = remove_session
        .write_stdin_raw_with(
            "import importlib.util, numpy; print('NUMPY_OK:' + str(hasattr(numpy, '__version__'))); print('PACKAGING_SPEC:' + str(importlib.util.find_spec('packaging') is not None))",
            Some(5.0),
        )
        .await?;
    let remove_probe_text = common::result_text(&remove_probe);
    assert!(
        remove_probe_text.contains("NUMPY_OK:True")
            && remove_probe_text.contains("PACKAGING_SPEC:False"),
        "remove should not keep packages removed from the manifest, got: {remove_probe_text:?}"
    );

    remove_session.cancel().await?;
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
async fn repl_prepare_restart_no_after_replacement_does_not_see_stale_user_state() -> TestResult<()>
{
    let (_uv_guard, uv_env) = RealUv::locked_new().await?;
    let session = common::spawn_python_server_with_files_env_vars(
        uv_env.env_vars_with_python(venv_python(&uv_env.stdlib_venv))?,
    )
    .await?;

    let seed = session
        .write_stdin_raw_with("_prepare_marker = 'discarded'", Some(5.0))
        .await?;
    let seed_text = common::result_text(&seed);
    assert!(
        !common::is_busy_response(&seed_text),
        "expected initial assignment to complete, got: {seed_text:?}"
    );

    let first_prepare = call_prepare(&session, json!({})).await?;
    let first_text = common::result_text(&first_prepare);
    assert!(
        first_text.contains("session restarted") && first_text.contains("user state discarded"),
        "expected first prepare to replace the user session, got: {first_text:?}"
    );

    let second_prepare = call_prepare(
        &session,
        json!({
            "requirements": {
                "packages": [EXTRA_PACKAGE],
                "action": "set",
                "restart": "no"
            }
        }),
    )
    .await?;
    let second_text = common::result_text(&second_prepare);
    assert_ne!(
        second_prepare.is_error,
        Some(true),
        "restart=no should not see stale user state after prepare replacement: {second_text:?}"
    );
    assert!(
        second_text.contains("session restarted")
            && second_text.contains("no user state discarded"),
        "expected restart=no to replace the fresh session without stale user state, got: {second_text:?}"
    );
    assert_manifest_packages(&second_text, &[EXTRA_PACKAGE]);

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
// Linux CI intermittently exits before worker_ready when this case forces a
// restart into uv's isolated target; other tests cover Linux restart behavior.
#[cfg_attr(
    target_os = "linux",
    ignore = "forced uv-managed restart is flaky on GitHub Linux runners"
)]
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
            "import time\nprint('PREPARE_BUSY_READY:' + ('x' * 5000), flush=True)\ntime.sleep(60)",
            Some(0.5),
        )
        .await?;
    let first_text = common::result_text(&first);
    assert!(
        common::is_busy_response(&first_text) || first_text.contains("PREPARE_BUSY_READY"),
        "expected timed-out active work, got: {first_text:?}"
    );
    let first_transcript_path = bundle_transcript_path(&first_text);

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
    assert!(
        !follow_up_text.contains("PREPARE_BUSY_READY"),
        "did not expect discarded timeout output in the fresh reply, got: {follow_up_text:?}"
    );
    let follow_up_transcript_path = bundle_transcript_path(&follow_up_text);
    if let (Some(first_path), Some(follow_up_path)) =
        (&first_transcript_path, &follow_up_transcript_path)
    {
        assert_ne!(
            first_path, follow_up_path,
            "expected prepare replacement to retire the discarded timeout bundle"
        );
    }
    if let Some(first_path) = &first_transcript_path {
        let first_transcript_after = fs::read_to_string(first_path)?;
        assert!(
            !first_transcript_after.contains("AFTER_PREPARE_REPLACE"),
            "did not expect fresh output appended to discarded timeout bundle: {first_transcript_after:?}"
        );
    }

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
