use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

pub const INTERPRETER_ENV: &str = "MCP_REPL_INTERPRETER";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    R,
    Python,
}

impl Backend {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_lowercase().as_str() {
            "r" => Ok(Backend::R),
            "python" => Ok(Backend::Python),
            other => Err(format!(
                "invalid interpreter: {other} (expected 'r' or 'python')"
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub enum WorkerLaunch {
    Builtin(Backend),
    Custom(CustomWorkerSpec),
}

impl WorkerLaunch {
    pub fn builtin_backend(&self) -> Option<Backend> {
        match self {
            Self::Builtin(backend) => Some(*backend),
            Self::Custom(_) => None,
        }
    }

    pub fn stdin_transport(&self) -> WorkerStdinTransport {
        match self {
            Self::Builtin(Backend::Python)
                if cfg!(target_family = "unix") || cfg!(target_family = "windows") =>
            {
                WorkerStdinTransport::Pty
            }
            Self::Builtin(_) => WorkerStdinTransport::Pipe,
            Self::Custom(spec) => spec.stdin.transport(),
        }
    }

    pub fn label(&self) -> String {
        match self {
            Self::Builtin(Backend::R) => "r".to_string(),
            Self::Builtin(Backend::Python) => "python".to_string(),
            Self::Custom(spec) => format!("custom:{}", spec.executable.display()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerStdinTransport {
    Pipe,
    Pty,
}

impl WorkerStdinTransport {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pipe => "pipe",
            Self::Pty => "pty",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomWorkerSpec {
    pub executable: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    pub working_dir: CustomWorkerWorkingDir,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub stdin: CustomWorkerStdin,
    pub sandbox: CustomWorkerSandbox,
}

impl CustomWorkerSpec {
    pub fn from_json_file(path: &Path) -> Result<Self, String> {
        let bytes = std::fs::read(path)
            .map_err(|err| format!("failed to read worker spec {}: {err}", path.display()))?;
        let spec: Self = serde_json::from_slice(&bytes)
            .map_err(|err| format!("failed to parse worker spec {}: {err}", path.display()))?;
        spec.validate()?;
        Ok(spec)
    }

    fn validate(&self) -> Result<(), String> {
        if self.executable.as_os_str().is_empty() {
            return Err("worker spec executable must not be empty".to_string());
        }
        if matches!(
            &self.working_dir,
            CustomWorkerWorkingDir::Path { path } if path.as_os_str().is_empty()
        ) {
            return Err("worker spec working_dir path must not be empty".to_string());
        }
        match self.sandbox {
            CustomWorkerSandbox::Server => {}
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum CustomWorkerWorkingDir {
    Policy(CustomWorkerWorkingDirPolicy),
    Path { path: PathBuf },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CustomWorkerWorkingDirPolicy {
    Inherit,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CustomWorkerStdin {
    Pipe,
    Pty,
}

impl CustomWorkerStdin {
    pub fn transport(self) -> WorkerStdinTransport {
        match self {
            Self::Pipe => WorkerStdinTransport::Pipe,
            Self::Pty => WorkerStdinTransport::Pty,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CustomWorkerSandbox {
    Server,
}

pub fn backend_from_env() -> Result<Option<Backend>, String> {
    let Ok(value) = std::env::var(INTERPRETER_ENV) else {
        return Ok(None);
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    Backend::parse(trimmed).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn backend_from_env_reads_interpreter_env_var() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        unsafe {
            std::env::remove_var(INTERPRETER_ENV);
            std::env::set_var(INTERPRETER_ENV, "python");
        }
        let parsed = backend_from_env().expect("parse env var");
        assert_eq!(parsed, Some(Backend::Python));
        unsafe {
            std::env::remove_var(INTERPRETER_ENV);
        }
    }

    #[test]
    fn backend_from_env_ignores_empty_interpreter_env_var() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        unsafe {
            std::env::remove_var(INTERPRETER_ENV);
            std::env::set_var(INTERPRETER_ENV, "   ");
        }
        let parsed = backend_from_env().expect("parse env var");
        assert_eq!(parsed, None);
        unsafe {
            std::env::remove_var(INTERPRETER_ENV);
        }
    }

    #[test]
    fn builtin_worker_launches_default_to_pipe_stdin_transport() {
        assert_eq!(
            WorkerLaunch::Builtin(Backend::R).stdin_transport(),
            WorkerStdinTransport::Pipe
        );
        #[cfg(not(target_family = "unix"))]
        {
            #[cfg(target_family = "windows")]
            assert_eq!(
                WorkerLaunch::Builtin(Backend::Python).stdin_transport(),
                WorkerStdinTransport::Pty
            );
            #[cfg(not(target_family = "windows"))]
            assert_eq!(
                WorkerLaunch::Builtin(Backend::Python).stdin_transport(),
                WorkerStdinTransport::Pipe
            );
        }
        #[cfg(target_family = "unix")]
        assert_eq!(
            WorkerLaunch::Builtin(Backend::Python).stdin_transport(),
            WorkerStdinTransport::Pty
        );
    }
}
