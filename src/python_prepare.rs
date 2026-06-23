use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use schemars::JsonSchema;
use serde::Deserialize;

use crate::python_runtime::{
    PythonRuntimeConfig, find_program_on_path, query_python_runtime_config,
    resolve_python_runtime_config,
};

const DEFAULT_PACKAGES: &[&str] = &["numpy"];
const UV_PROGRAM: &str = "uv";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PythonRequirementsManifest {
    pub(crate) packages: Vec<String>,
    pub(crate) python_version: Option<String>,
}

impl Default for PythonRequirementsManifest {
    fn default() -> Self {
        Self {
            packages: DEFAULT_PACKAGES
                .iter()
                .map(|package| package.to_string())
                .collect(),
            python_version: None,
        }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReplPrepareArgs {
    #[serde(default)]
    requirements: Option<PrepareRequirements>,
    #[serde(default)]
    python: Option<PreparePython>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PrepareRequirements {
    #[serde(default)]
    packages: Option<Vec<String>>,
    #[serde(default)]
    python_version: Option<String>,
    #[serde(default)]
    action: Option<PrepareRequirementsAction>,
    #[serde(default)]
    restart: Option<PrepareRestartPolicy>,
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PrepareRequirementsAction {
    Add,
    Remove,
    Set,
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PrepareRestartPolicy {
    IfNeeded,
    Yes,
    No,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreparePython {
    #[serde(default)]
    executable: Option<PathBuf>,
    #[serde(default)]
    venv: Option<PathBuf>,
}

pub(crate) struct PythonPrepareTarget {
    pub(crate) executable: PathBuf,
    pub(crate) module_search_paths: Vec<PathBuf>,
}

pub(crate) enum ValidatedPrepareRequest {
    Requirements(PrepareRequirementsOperation),
    PythonExecutable(PathBuf),
}

pub(crate) struct PrepareRequirementsOperation {
    packages: Option<Vec<String>>,
    python_version: Option<String>,
    pub(crate) action: PrepareRequirementsAction,
    pub(crate) restart: PrepareRestartPolicy,
}

pub(crate) fn uv_available() -> bool {
    find_program_on_path(UV_PROGRAM).is_some()
}

pub(crate) fn validate_prepare_args(
    args: ReplPrepareArgs,
) -> Result<ValidatedPrepareRequest, String> {
    match (args.requirements, args.python) {
        (Some(_), Some(_)) => {
            Err("repl_prepare accepts either `requirements` or `python`, not both".to_string())
        }
        (None, None) => Ok(ValidatedPrepareRequest::Requirements(
            PrepareRequirementsOperation {
                packages: None,
                python_version: None,
                action: PrepareRequirementsAction::Add,
                restart: PrepareRestartPolicy::IfNeeded,
            },
        )),
        (Some(requirements), None) => validate_requirements(requirements),
        (None, Some(python)) => validate_python(python),
    }
}

pub(crate) fn resolve_prepare_target(
    request: &ValidatedPrepareRequest,
) -> Result<PythonPrepareTarget, String> {
    let config = match request {
        ValidatedPrepareRequest::Requirements(_) => {
            return Err(
                "requirements requests must be applied to the current manifest".to_string(),
            );
        }
        ValidatedPrepareRequest::PythonExecutable(executable) => {
            query_python_runtime_config(executable)?
        }
    };
    Ok(PythonPrepareTarget {
        executable: config.executable,
        module_search_paths: config.module_search_paths,
    })
}

pub(crate) fn apply_requirements_operation(
    manifest: &PythonRequirementsManifest,
    operation: &PrepareRequirementsOperation,
) -> PythonRequirementsManifest {
    let mut manifest = manifest.clone();
    match operation.action {
        PrepareRequirementsAction::Add => {
            if let Some(packages) = operation.packages.as_ref() {
                for package in packages {
                    if !manifest.packages.iter().any(|existing| existing == package) {
                        manifest.packages.push(package.clone());
                    }
                }
            }
            if let Some(python_version) = operation.python_version.as_ref() {
                manifest.python_version =
                    add_python_version_constraint(manifest.python_version, python_version);
            }
        }
        PrepareRequirementsAction::Remove => {
            if let Some(packages) = operation.packages.as_ref() {
                manifest
                    .packages
                    .retain(|package| !packages.iter().any(|remove| remove == package));
            }
            if operation.python_version == manifest.python_version {
                manifest.python_version = None;
            }
        }
        PrepareRequirementsAction::Set => {
            manifest.packages = operation.packages.clone().unwrap_or_default();
            manifest.python_version = operation.python_version.clone();
        }
    }
    manifest
}

fn add_python_version_constraint(current: Option<String>, requested: &str) -> Option<String> {
    match current {
        None => Some(requested.to_string()),
        Some(current) if current == requested => Some(current),
        Some(current) => Some(format!("{current},{requested}")),
    }
}

pub(crate) fn resolve_requirements_manifest(
    manifest: &PythonRequirementsManifest,
) -> Result<PythonPrepareTarget, String> {
    let config = resolve_requirements(&manifest.packages, manifest.python_version.as_deref())?;
    Ok(PythonPrepareTarget {
        executable: config.executable,
        module_search_paths: config.module_search_paths,
    })
}

pub(crate) fn format_requirements_manifest(manifest: &PythonRequirementsManifest) -> String {
    let packages = serde_json::to_string(&manifest.packages).unwrap_or_else(|_| "[]".to_string());
    let python_version =
        serde_json::to_string(&manifest.python_version).unwrap_or_else(|_| "null".to_string());
    format!("managed requirements manifest: packages={packages}, python_version={python_version}")
}

fn resolve_requirements(
    packages: &[String],
    python_version: Option<&str>,
) -> Result<PythonRuntimeConfig, String> {
    if let Some(config) = current_runtime_satisfies_requirements(packages, python_version) {
        return Ok(config);
    }
    resolve_uv_requirements(packages, python_version)
}

fn current_runtime_satisfies_requirements(
    packages: &[String],
    python_version: Option<&str>,
) -> Option<PythonRuntimeConfig> {
    if packages.is_empty() || python_version.is_some() {
        return None;
    }
    let config = resolve_python_runtime_config().ok()?;
    installed_distributions_satisfy(&config.executable, packages).then_some(config)
}

pub(crate) fn current_python_executable() -> Option<PathBuf> {
    resolve_python_runtime_config()
        .ok()
        .map(|config| config.executable)
}

pub(crate) fn same_python_executable(left: &Path, right: &Path) -> bool {
    left == right || strip_private_prefix(left) == strip_private_prefix(right)
}

#[cfg(target_os = "macos")]
fn strip_private_prefix(path: &Path) -> PathBuf {
    path.strip_prefix("/private")
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(not(target_os = "macos"))]
fn strip_private_prefix(path: &Path) -> PathBuf {
    path.to_path_buf()
}

fn validate_requirements(
    requirements: PrepareRequirements,
) -> Result<ValidatedPrepareRequest, String> {
    if let Some(packages) = requirements.packages.as_ref() {
        for package in packages {
            if package.trim().is_empty() {
                return Err("requirements.packages must not contain empty strings".to_string());
            }
        }
    }

    if let Some(python_version) = requirements.python_version.as_deref()
        && python_version.trim().is_empty()
    {
        return Err("requirements.python_version must not be empty".to_string());
    }

    Ok(ValidatedPrepareRequest::Requirements(
        PrepareRequirementsOperation {
            packages: requirements.packages,
            python_version: requirements.python_version,
            action: requirements
                .action
                .unwrap_or(PrepareRequirementsAction::Add),
            restart: requirements
                .restart
                .unwrap_or(PrepareRestartPolicy::IfNeeded),
        },
    ))
}

fn validate_python(python: PreparePython) -> Result<ValidatedPrepareRequest, String> {
    match (python.executable, python.venv) {
        (Some(_), Some(_)) => {
            Err("python must contain exactly one of `executable` or `venv`, not both".to_string())
        }
        (None, None) => {
            Err("python must contain exactly one of `executable` or `venv`".to_string())
        }
        (Some(executable), None) => {
            validate_absolute_path("python.executable", &executable)?;
            Ok(ValidatedPrepareRequest::PythonExecutable(executable))
        }
        (None, Some(venv)) => {
            validate_absolute_path("python.venv", &venv)?;
            Ok(ValidatedPrepareRequest::PythonExecutable(
                python_executable_for_venv(&venv),
            ))
        }
    }
}

fn validate_absolute_path(field: &str, path: &Path) -> Result<(), String> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(format!("{field} must be an absolute path"))
    }
}

fn python_executable_for_venv(venv: &Path) -> PathBuf {
    if cfg!(windows) {
        venv.join("Scripts").join("python.exe")
    } else {
        venv.join("bin").join("python")
    }
}

fn resolve_uv_requirements(
    packages: &[String],
    python_version: Option<&str>,
) -> Result<PythonRuntimeConfig, String> {
    let uv = find_program_on_path(UV_PROGRAM)
        .ok_or_else(|| "uv is required for repl_prepare but was not found on PATH".to_string())?;

    let mut command = Command::new(&uv);
    command.arg("tool").arg("run").arg("--isolated");
    if let Some(python_version) = python_version {
        command.arg("--python").arg(python_version);
    }
    for package in packages {
        command.arg("--with").arg(package);
    }
    command
        .arg("--")
        .arg("python")
        .arg("-I")
        .arg("-c")
        .arg("import sys; print(sys.executable)");
    command.stdin(Stdio::null());

    let output = command.output().map_err(|err| {
        format!(
            "failed to run uv while preparing Python requirements with {}: {err}",
            uv.display()
        )
    })?;
    if !output.status.success() {
        return Err(format!(
            "uv failed while preparing Python requirements: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let executable = stdout
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(str::trim)
        .ok_or_else(|| "uv did not report a Python executable".to_string())?;
    query_python_runtime_config(Path::new(executable))
}

fn installed_distributions_satisfy(executable: &Path, packages: &[String]) -> bool {
    let mut distribution_names = Vec::with_capacity(packages.len());
    for package in packages {
        let Some(name) = bare_requirement_distribution_name(package) else {
            return false;
        };
        distribution_names.push(name);
    }

    Command::new(executable)
        .arg("-I")
        .arg("-c")
        .arg(
            r#"
import importlib.metadata
import sys

for name in sys.argv[1:]:
    try:
        importlib.metadata.distribution(name)
    except importlib.metadata.PackageNotFoundError:
        raise SystemExit(1)
"#,
        )
        .args(distribution_names)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn bare_requirement_distribution_name(requirement: &str) -> Option<String> {
    let trimmed = requirement.trim();
    if trimmed.is_empty()
        || !trimmed
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        || !trimmed
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        || !trimmed
            .bytes()
            .last()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
    {
        return None;
    }
    Some(trimmed.to_string())
}
