use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::ffi::CString;
#[cfg(target_os = "windows")]
use std::ffi::OsStr;
#[cfg(target_os = "windows")]
use std::ffi::OsString;
use std::io::Write;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::process;
#[cfg(target_os = "macos")]
use url::Url;

use serde::{Deserialize, Serialize};
use tempfile::Builder;

pub const SANDBOX_STATE_CAPABILITY: &str = "codex/sandbox-state";
pub const SANDBOX_STATE_METHOD: &str = "codex/sandbox-state/update";
pub const MANAGED_ALLOWED_DOMAINS_ENV_KEY: &str = "MCP_REPL_ALLOWED_DOMAINS";
pub const MANAGED_DENIED_DOMAINS_ENV_KEY: &str = "MCP_REPL_DENIED_DOMAINS";
#[cfg(target_os = "macos")]
pub const CODEX_SANDBOX_ENV_VAR: &str = "CODEX_SANDBOX";
pub const CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR: &str = "CODEX_SANDBOX_NETWORK_DISABLED";
pub const R_SESSION_TMPDIR_ENV: &str = "MCP_REPL_R_SESSION_TMPDIR";
#[cfg(target_os = "macos")]
pub const SANDBOX_LOG_DENIALS_ENV: &str = "MCP_REPL_SANDBOX_LOG_DENIALS";
pub const INITIAL_SANDBOX_STATE_ENV: &str = "MCP_REPL_INITIAL_SANDBOX_STATE";
#[cfg(target_os = "linux")]
pub const LINUX_BWRAP_ENABLED_ENV: &str = "MCP_REPL_USE_LINUX_BWRAP";
#[cfg(target_os = "linux")]
pub const LINUX_BWRAP_NO_PROC_ENV: &str = "MCP_REPL_LINUX_BWRAP_NO_PROC";

#[derive(Debug, Clone)]
pub enum SandboxError {
    SessionTempDir(String),
    #[cfg(target_os = "macos")]
    SeatbeltMissing,
    #[cfg(target_os = "windows")]
    WindowsSandbox(String),
}

impl std::fmt::Display for SandboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SandboxError::SessionTempDir(message) => {
                write!(f, "failed to create session temp dir: {message}")
            }
            #[cfg(target_os = "macos")]
            SandboxError::SeatbeltMissing => {
                write!(f, "seatbelt sandbox executable not found")
            }
            #[cfg(target_os = "windows")]
            SandboxError::WindowsSandbox(message) => {
                write!(f, "windows sandbox error: {message}")
            }
        }
    }
}

impl std::error::Error for SandboxError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum NetworkAccess {
    #[default]
    Restricted,
    Enabled,
}

impl NetworkAccess {
    pub fn is_enabled(self) -> bool {
        matches!(self, NetworkAccess::Enabled)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ManagedNetworkPolicy {
    pub allowed_domains: Vec<String>,
    pub denied_domains: Vec<String>,
    pub allow_local_binding: bool,
}

impl ManagedNetworkPolicy {
    pub fn has_domain_restrictions(&self) -> bool {
        !self.allowed_domains.is_empty() || !self.denied_domains.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum SandboxPolicy {
    #[serde(rename = "danger-full-access")]
    DangerFullAccess,
    #[serde(rename = "read-only")]
    ReadOnly,
    #[serde(rename = "external-sandbox")]
    ExternalSandbox {
        #[serde(default)]
        network_access: NetworkAccess,
    },
    #[serde(rename = "workspace-write")]
    WorkspaceWrite {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        writable_roots: Vec<PathBuf>,
        #[serde(default)]
        network_access: bool,
        #[serde(default)]
        exclude_tmpdir_env_var: bool,
        #[serde(default)]
        exclude_slash_tmp: bool,
    },
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WritableRoot {
    pub root: PathBuf,
    pub read_only_subpaths: Vec<PathBuf>,
}

impl SandboxPolicy {
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    pub fn has_full_disk_write_access(&self) -> bool {
        match self {
            SandboxPolicy::DangerFullAccess => true,
            SandboxPolicy::ExternalSandbox { .. } => true,
            SandboxPolicy::ReadOnly => false,
            SandboxPolicy::WorkspaceWrite { .. } => false,
        }
    }

    #[cfg(target_os = "macos")]
    pub fn has_full_disk_read_access(&self) -> bool {
        match self {
            SandboxPolicy::DangerFullAccess => true,
            SandboxPolicy::ExternalSandbox { .. } => true,
            SandboxPolicy::ReadOnly => true,
            SandboxPolicy::WorkspaceWrite { .. } => true,
        }
    }

    pub fn has_full_network_access(&self) -> bool {
        match self {
            SandboxPolicy::DangerFullAccess => true,
            SandboxPolicy::ExternalSandbox { network_access } => network_access.is_enabled(),
            SandboxPolicy::ReadOnly => false,
            SandboxPolicy::WorkspaceWrite { network_access, .. } => *network_access,
        }
    }

    pub fn requires_sandbox(&self) -> bool {
        !matches!(
            self,
            SandboxPolicy::DangerFullAccess | SandboxPolicy::ExternalSandbox { .. }
        )
    }

    #[cfg(target_os = "macos")]
    pub fn get_writable_roots_with_cwd(
        &self,
        cwd: &Path,
        session_temp_dir: Option<&Path>,
    ) -> Vec<WritableRoot> {
        match self {
            SandboxPolicy::ReadOnly => {
                let roots = temp_writable_roots(false, false, session_temp_dir);
                roots
                    .into_iter()
                    .map(|root| WritableRoot {
                        read_only_subpaths: compute_read_only_subpaths(&root),
                        root,
                    })
                    .collect()
            }
            SandboxPolicy::WorkspaceWrite {
                writable_roots,
                exclude_tmpdir_env_var,
                exclude_slash_tmp,
                network_access: _,
            } => {
                let mut roots = Vec::new();

                for root in writable_roots {
                    if let Some(path) = ensure_absolute(root.clone()) {
                        roots.push(path);
                    }
                }

                if let Some(path) = ensure_absolute(cwd.to_path_buf()) {
                    roots.push(path);
                }

                roots.extend(temp_writable_roots(
                    *exclude_tmpdir_env_var,
                    *exclude_slash_tmp,
                    session_temp_dir,
                ));

                roots.sort();
                roots.dedup();

                roots
                    .into_iter()
                    .map(|root| WritableRoot {
                        read_only_subpaths: compute_read_only_subpaths(&root),
                        root,
                    })
                    .collect()
            }
            _ => Vec::new(),
        }
    }
}

#[cfg_attr(target_os = "windows", allow(dead_code))]
fn ensure_absolute(path: PathBuf) -> Option<PathBuf> {
    if path.is_absolute() { Some(path) } else { None }
}

fn env_var_truthy(key: &str) -> bool {
    std::env::var(key).ok().is_some_and(|value| {
        let trimmed = value.trim();
        trimmed == "1" || trimmed.eq_ignore_ascii_case("true")
    })
}

#[cfg_attr(target_os = "windows", allow(dead_code))]
fn temp_roots_from_system(exclude_tmpdir_env_var: bool, exclude_slash_tmp: bool) -> Vec<PathBuf> {
    let mut roots = Vec::new();

    if cfg!(unix) && !exclude_slash_tmp {
        let slash_tmp = PathBuf::from("/tmp");
        if slash_tmp.is_dir() {
            roots.push(slash_tmp);
        }
    }

    if !exclude_tmpdir_env_var
        && let Some(tmpdir) = std::env::var_os("TMPDIR")
        && !tmpdir.is_empty()
        && let Some(path) = ensure_absolute(PathBuf::from(tmpdir))
    {
        roots.push(path);
    }

    roots
}

#[cfg(target_os = "linux")]
pub fn invoked_as_codex_linux_sandbox() -> bool {
    std::env::args_os()
        .next()
        .and_then(|arg0| {
            PathBuf::from(arg0)
                .file_name()
                .map(|name| name.to_os_string())
        })
        .as_deref()
        == Some(std::ffi::OsStr::new("codex-linux-sandbox"))
}

#[cfg(target_os = "macos")]
fn temp_writable_roots(
    exclude_tmpdir_env_var: bool,
    exclude_slash_tmp: bool,
    session_temp_dir: Option<&Path>,
) -> Vec<PathBuf> {
    // Match Codex behavior: keep the session temp dir writable, but also allow
    // system temp roots like /tmp and TMPDIR so native libraries can use them.
    let mut roots = temp_roots_from_system(exclude_tmpdir_env_var, exclude_slash_tmp);
    if let Some(session_temp_dir) = session_temp_dir
        && let Some(path) = ensure_absolute(session_temp_dir.to_path_buf())
    {
        roots.push(path);
    }
    roots
}

#[cfg(target_os = "macos")]
fn compute_read_only_subpaths(root: &Path) -> Vec<PathBuf> {
    let mut subpaths = Vec::new();

    let dot_git = root.join(".git");
    if dot_git.is_dir() || dot_git.is_file() {
        if dot_git.is_file()
            && let Some(gitdir) = resolve_gitdir_from_file(&dot_git)
            && !subpaths.iter().any(|path| path == &gitdir)
        {
            subpaths.push(gitdir);
        }
        subpaths.push(dot_git);
    }

    let dot_codex = root.join(".codex");
    if dot_codex.is_dir() {
        subpaths.push(dot_codex);
    }

    let dot_agents = root.join(".agents");
    if dot_agents.is_dir() {
        subpaths.push(dot_agents);
    }

    subpaths
}

#[cfg(target_os = "linux")]
fn compute_linux_read_only_subpaths(root: &Path) -> Vec<PathBuf> {
    let mut subpaths = Vec::new();

    let dot_git = root.join(".git");
    if dot_git.is_dir() || dot_git.is_file() {
        if dot_git.is_file()
            && let Some(gitdir) = resolve_gitdir_from_file(&dot_git)
            && !subpaths.iter().any(|path| path == &gitdir)
        {
            subpaths.push(gitdir);
        }
        subpaths.push(dot_git);
    }

    let dot_codex = root.join(".codex");
    if dot_codex.is_dir() {
        subpaths.push(dot_codex);
    }

    let dot_agents = root.join(".agents");
    if dot_agents.is_dir() {
        subpaths.push(dot_agents);
    }

    subpaths
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn resolve_gitdir_from_file(dot_git: &Path) -> Option<PathBuf> {
    let contents = std::fs::read_to_string(dot_git).ok()?;
    let trimmed = contents.trim();
    let (_, gitdir_raw) = trimmed.split_once(':')?;
    let gitdir_raw = gitdir_raw.trim();
    if gitdir_raw.is_empty() {
        return None;
    }
    let base = dot_git.parent()?;
    let gitdir_path = if Path::new(gitdir_raw).is_absolute() {
        PathBuf::from(gitdir_raw)
    } else {
        base.join(gitdir_raw)
    };
    if gitdir_path.exists() {
        Some(gitdir_path)
    } else {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxState {
    pub sandbox_policy: SandboxPolicy,
    pub sandbox_cwd: PathBuf,
    pub codex_linux_sandbox_exe: Option<PathBuf>,
    pub use_linux_sandbox_bwrap: bool,
    pub managed_network_policy: ManagedNetworkPolicy,
    pub session_temp_dir: PathBuf,
}

pub fn log_sandbox_policy_update(policy: &SandboxPolicy) {
    crate::event_log::log(
        "sandbox_policy_update_received",
        serde_json::json!({
            "policy": policy,
        }),
    );
    let Some(path) = crate::debug_logs::log_path("sandbox-state.jsonl") else {
        return;
    };
    let payload = serde_json::to_string(policy).unwrap_or_else(|_| format!("{policy:?}"));
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{payload}");
    }
}

pub fn log_sandbox_state_event(method: &str, params: Option<&serde_json::Value>) {
    crate::event_log::log(
        "sandbox_state_event_received",
        serde_json::json!({
            "method": method,
            "params": params,
        }),
    );
    let Some(path) = crate::debug_logs::log_path("sandbox-state.jsonl") else {
        return;
    };
    let payload = serde_json::json!({
        "method": method,
        "params": params,
    });
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{payload}");
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxStateUpdate {
    pub sandbox_policy: SandboxPolicy,
    #[serde(default)]
    pub sandbox_cwd: Option<PathBuf>,
    #[serde(default)]
    pub codex_linux_sandbox_exe: Option<PathBuf>,
    #[serde(default)]
    pub use_linux_sandbox_bwrap: Option<bool>,
}

pub fn initial_sandbox_state_update() -> Option<SandboxStateUpdate> {
    let raw = std::env::var(INITIAL_SANDBOX_STATE_ENV).ok()?;
    match serde_json::from_str::<SandboxStateUpdate>(&raw) {
        Ok(update) => Some(update),
        Err(err) => {
            eprintln!("Invalid initial sandbox state: {err}");
            None
        }
    }
}

impl SandboxState {
    pub fn apply_update(&mut self, update: SandboxStateUpdate) -> bool {
        let mut next = self.clone();
        next.sandbox_policy = update.sandbox_policy;
        if let Some(cwd) = update.sandbox_cwd {
            next.sandbox_cwd = cwd;
        }
        if let Some(exe) = update.codex_linux_sandbox_exe {
            next.codex_linux_sandbox_exe = Some(exe);
        }
        if let Some(use_bwrap) = update.use_linux_sandbox_bwrap {
            next.use_linux_sandbox_bwrap = use_bwrap;
        }
        let changed = next != *self;
        *self = next;
        changed
    }
}

impl Default for SandboxState {
    fn default() -> Self {
        let sandbox_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let codex_linux_sandbox_exe = None;
        let session_temp_dir = build_session_temp_dir_path();
        Self {
            sandbox_policy: SandboxPolicy::WorkspaceWrite {
                writable_roots: Vec::new(),
                network_access: false,
                exclude_tmpdir_env_var: false,
                exclude_slash_tmp: false,
            },
            sandbox_cwd,
            codex_linux_sandbox_exe,
            use_linux_sandbox_bwrap: false,
            managed_network_policy: ManagedNetworkPolicy::default(),
            session_temp_dir,
        }
    }
}

#[cfg_attr(target_os = "windows", allow(dead_code))]
pub struct PreparedCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub arg0: Option<String>,
    #[cfg(target_os = "macos")]
    pub denial_logger: Option<DenialLogger>,
}

pub fn prepare_worker_command(
    program: &Path,
    args: Vec<String>,
    state: &SandboxState,
) -> Result<PreparedCommand, SandboxError> {
    let mut env = HashMap::new();
    if !state.sandbox_policy.has_full_network_access() {
        env.insert(
            CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR.to_string(),
            "1".to_string(),
        );
    }
    env.insert(
        ALLOW_LOCAL_BINDING_ENV_KEY.to_string(),
        if state.managed_network_policy.allow_local_binding {
            "1".to_string()
        } else {
            "0".to_string()
        },
    );
    env.insert(
        MANAGED_ALLOWED_DOMAINS_ENV_KEY.to_string(),
        state.managed_network_policy.allowed_domains.join(","),
    );
    env.insert(
        MANAGED_DENIED_DOMAINS_ENV_KEY.to_string(),
        state.managed_network_policy.denied_domains.join(","),
    );
    env.insert(
        MANAGED_NETWORK_ENV_KEY.to_string(),
        if state.managed_network_policy.has_domain_restrictions() {
            "1".to_string()
        } else {
            "0".to_string()
        },
    );

    prepare_session_temp_dir(&state.session_temp_dir)?;
    {
        let temp_dir = state.session_temp_dir.to_string_lossy().to_string();
        env.insert("TMPDIR".to_string(), temp_dir.clone());
        env.insert(R_SESSION_TMPDIR_ENV.to_string(), temp_dir);
        #[cfg(target_os = "windows")]
        {
            // Ensure Windows sandbox policy and runtime temp resolution both target the
            // per-session temp directory instead of the full user TEMP tree.
            env.insert(
                "TEMP".to_string(),
                state.session_temp_dir.to_string_lossy().to_string(),
            );
            env.insert(
                "TMP".to_string(),
                state.session_temp_dir.to_string_lossy().to_string(),
            );
        }
    }

    if !state.sandbox_policy.requires_sandbox() {
        return Ok(PreparedCommand {
            program: program.to_path_buf(),
            args,
            env,
            arg0: None,
            #[cfg(target_os = "macos")]
            denial_logger: None,
        });
    }

    #[cfg(target_os = "macos")]
    {
        if !Path::new(MACOS_PATH_TO_SEATBELT_EXECUTABLE).exists() {
            return Err(SandboxError::SeatbeltMissing);
        }

        let mut network_env = sandbox_network_env_snapshot();
        for key in [
            ALLOW_LOCAL_BINDING_ENV_KEY,
            MANAGED_NETWORK_ENV_KEY,
            MANAGED_ALLOWED_DOMAINS_ENV_KEY,
            MANAGED_DENIED_DOMAINS_ENV_KEY,
        ] {
            if let Some(value) = env.get(key) {
                network_env.insert(key.to_string(), value.clone());
            }
        }
        let command = build_command_vec(program, &args);
        let mut seatbelt_args = create_seatbelt_command_args(
            command,
            &state.sandbox_policy,
            &state.managed_network_policy,
            &network_env,
            &state.sandbox_cwd,
            &state.session_temp_dir,
        );
        let mut full_command = Vec::with_capacity(1 + seatbelt_args.len());
        full_command.push(MACOS_PATH_TO_SEATBELT_EXECUTABLE.to_string());
        full_command.append(&mut seatbelt_args);
        env.insert(CODEX_SANDBOX_ENV_VAR.to_string(), "seatbelt".to_string());
        let denial_logger = log_denials_enabled().then(DenialLogger::new).flatten();
        Ok(PreparedCommand {
            program: PathBuf::from(MACOS_PATH_TO_SEATBELT_EXECUTABLE),
            args: full_command[1..].to_vec(),
            env,
            arg0: None,
            denial_logger,
        })
    }

    #[cfg(target_os = "linux")]
    {
        let mut policy = state.sandbox_policy.clone();
        let mut policy_cwd = state.sandbox_cwd.clone();
        match &mut policy {
            SandboxPolicy::ReadOnly => {
                let temp_root = state.session_temp_dir.clone();
                policy = SandboxPolicy::WorkspaceWrite {
                    writable_roots: vec![temp_root.clone()],
                    network_access: false,
                    exclude_tmpdir_env_var: true,
                    exclude_slash_tmp: true,
                };
                policy_cwd = temp_root;
            }
            SandboxPolicy::WorkspaceWrite {
                writable_roots,
                exclude_tmpdir_env_var,
                exclude_slash_tmp,
                network_access: _,
            } => {
                if !writable_roots
                    .iter()
                    .any(|root| root == &state.session_temp_dir)
                {
                    writable_roots.push(state.session_temp_dir.clone());
                }
                *exclude_tmpdir_env_var = true;
                *exclude_slash_tmp = true;
            }
            _ => {}
        }
        let policy = sanitize_linux_sandbox_policy(&policy);
        let command = build_command_vec(program, &args);
        let sandbox_args = create_linux_sandbox_command_args(
            command,
            &policy,
            &policy_cwd,
            state.use_linux_sandbox_bwrap,
            env_var_truthy(LINUX_BWRAP_NO_PROC_ENV),
        );
        let sandbox_program = state
            .codex_linux_sandbox_exe
            .clone()
            .unwrap_or_else(|| program.to_path_buf());
        Ok(PreparedCommand {
            program: sandbox_program,
            args: sandbox_args,
            env,
            arg0: Some("codex-linux-sandbox".to_string()),
        })
    }

    #[cfg(target_os = "windows")]
    {
        let command = build_command_vec(program, &args);
        let sandbox_args =
            create_windows_sandbox_command_args(command, &state.sandbox_policy, &state.sandbox_cwd)
                .map_err(SandboxError::WindowsSandbox)?;
        let sandbox_program = std::env::current_exe().map_err(|err| {
            SandboxError::WindowsSandbox(format!("failed to resolve current executable: {err}"))
        })?;
        Ok(PreparedCommand {
            program: sandbox_program,
            args: sandbox_args,
            env,
            arg0: None,
        })
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        Ok(PreparedCommand {
            program: program.to_path_buf(),
            args,
            env,
            arg0: None,
        })
    }
}

fn build_command_vec(program: &Path, args: &[String]) -> Vec<String> {
    let mut command = Vec::with_capacity(1 + args.len());
    command.push(program.to_string_lossy().to_string());
    command.extend(args.iter().cloned());
    command
}

#[cfg(target_os = "linux")]
fn create_linux_sandbox_command_args(
    command: Vec<String>,
    sandbox_policy: &SandboxPolicy,
    sandbox_policy_cwd: &Path,
    use_bwrap_sandbox: bool,
    no_proc: bool,
) -> Vec<String> {
    let sandbox_policy_cwd = sandbox_policy_cwd.to_string_lossy().to_string();
    let sanitized_policy = sanitize_linux_sandbox_policy(sandbox_policy);
    let sandbox_policy_json =
        serde_json::to_string(&sanitized_policy).expect("failed to serialize Linux sandbox policy");
    let mut linux_cmd: Vec<String> = vec![
        "--sandbox-policy-cwd".to_string(),
        sandbox_policy_cwd,
        "--sandbox-policy".to_string(),
        sandbox_policy_json,
    ];
    if use_bwrap_sandbox {
        linux_cmd.push("--use-bwrap-sandbox".to_string());
    }
    if no_proc {
        linux_cmd.push("--no-proc".to_string());
    }
    linux_cmd.extend(["--".to_string()]);
    linux_cmd.extend(command);
    linux_cmd
}

#[cfg(target_os = "windows")]
fn create_windows_sandbox_command_args(
    command: Vec<String>,
    sandbox_policy: &SandboxPolicy,
    sandbox_policy_cwd: &Path,
) -> Result<Vec<String>, String> {
    let sandbox_policy_cwd = sandbox_policy_cwd.to_string_lossy().to_string();
    let sandbox_policy_json =
        serde_json::to_string(sandbox_policy).map_err(|err| err.to_string())?;
    let mut windows_cmd: Vec<String> = vec![
        "--windows-sandbox".to_string(),
        "--sandbox-policy-cwd".to_string(),
        sandbox_policy_cwd,
        "--sandbox-policy".to_string(),
        sandbox_policy_json,
        "--".to_string(),
    ];
    windows_cmd.extend(command);
    Ok(windows_cmd)
}

#[cfg(target_os = "linux")]
fn sanitize_linux_sandbox_policy(policy: &SandboxPolicy) -> SandboxPolicy {
    match policy {
        SandboxPolicy::WorkspaceWrite {
            writable_roots,
            network_access,
            exclude_tmpdir_env_var,
            exclude_slash_tmp,
        } => {
            let writable_roots = writable_roots
                .iter()
                .filter_map(|root| ensure_absolute(root.clone()))
                .collect();
            SandboxPolicy::WorkspaceWrite {
                writable_roots,
                network_access: *network_access,
                exclude_tmpdir_env_var: *exclude_tmpdir_env_var,
                exclude_slash_tmp: *exclude_slash_tmp,
            }
        }
        SandboxPolicy::ExternalSandbox { network_access } => SandboxPolicy::ExternalSandbox {
            network_access: *network_access,
        },
        SandboxPolicy::DangerFullAccess => SandboxPolicy::DangerFullAccess,
        SandboxPolicy::ReadOnly => SandboxPolicy::ReadOnly,
    }
}

fn build_session_temp_dir_path() -> PathBuf {
    Builder::new()
        .prefix("mcp-repl-session-")
        .tempdir()
        .map(|dir| dir.keep())
        .unwrap_or_else(|err| {
            eprintln!("Failed to create session temp dir: {err}");
            let mut path = std::env::temp_dir();
            let pid = std::process::id();
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            path.push(format!("mcp-repl-session-{pid}-{nanos}"));
            path
        })
}

fn prepare_session_temp_dir(path: &Path) -> Result<(), SandboxError> {
    if !path.is_absolute() {
        return Err(SandboxError::SessionTempDir(format!(
            "session temp dir is not absolute: {}",
            path.to_string_lossy()
        )));
    }
    let base_tmp = std::env::temp_dir();
    if !path.starts_with(&base_tmp) {
        return Err(SandboxError::SessionTempDir(format!(
            "session temp dir outside system temp: {} (base: {})",
            path.to_string_lossy(),
            base_tmp.to_string_lossy()
        )));
    }
    if path.parent().is_none() {
        return Err(SandboxError::SessionTempDir(
            "refusing to use a temp dir without parent".to_string(),
        ));
    }
    if let Err(err) = std::fs::remove_dir_all(path)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        return Err(SandboxError::SessionTempDir(err.to_string()));
    }
    std::fs::create_dir_all(path).map_err(|err| SandboxError::SessionTempDir(err.to_string()))?;
    Ok(())
}

#[cfg(target_os = "macos")]
const MACOS_PATH_TO_SEATBELT_EXECUTABLE: &str = "/usr/bin/sandbox-exec";

#[cfg(target_os = "macos")]
const MACOS_SEATBELT_BASE_POLICY: &str = include_str!("sandbox/seatbelt_base_policy.sbpl");
#[cfg(target_os = "macos")]
const MACOS_SEATBELT_NETWORK_POLICY: &str = include_str!("sandbox/seatbelt_network_policy.sbpl");
#[cfg(target_os = "macos")]
const PROXY_URL_ENV_KEYS: [&str; 6] = [
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
];
const MANAGED_NETWORK_ENV_KEY: &str = "MCP_REPL_MANAGED_NETWORK";
const ALLOW_LOCAL_BINDING_ENV_KEY: &str = "ALLOW_LOCAL_BINDING";

pub fn sandbox_state_defaults_with_environment() -> SandboxState {
    let mut defaults = SandboxState::default();
    defaults.managed_network_policy.allow_local_binding =
        env_var_truthy(ALLOW_LOCAL_BINDING_ENV_KEY);
    #[cfg(target_os = "linux")]
    {
        defaults.use_linux_sandbox_bwrap = env_var_truthy(LINUX_BWRAP_ENABLED_ENV);
    }
    defaults
}

#[cfg(target_os = "macos")]
#[derive(Debug, Default)]
struct ProxyPolicyInputs {
    ports: Vec<u16>,
    has_proxy_config: bool,
}

#[cfg(target_os = "macos")]
fn env_bool(value: Option<&str>) -> bool {
    value.is_some_and(|v| {
        let trimmed = v.trim();
        trimmed == "1" || trimmed.eq_ignore_ascii_case("true")
    })
}

#[cfg(target_os = "macos")]
fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1"
}

#[cfg(target_os = "macos")]
fn proxy_scheme_default_port(scheme: &str) -> u16 {
    match scheme {
        "https" => 443,
        "socks5" | "socks5h" | "socks4" | "socks4a" => 1080,
        _ => 80,
    }
}

#[cfg(target_os = "macos")]
fn has_proxy_url_env_vars(env: &HashMap<String, String>) -> bool {
    PROXY_URL_ENV_KEYS
        .iter()
        .filter_map(|key| env.get(*key))
        .any(|value| !value.trim().is_empty())
}

#[cfg(target_os = "macos")]
fn proxy_loopback_ports_from_env(env: &HashMap<String, String>) -> Vec<u16> {
    let mut ports = std::collections::BTreeSet::<u16>::new();
    for key in PROXY_URL_ENV_KEYS {
        let Some(proxy_url) = env.get(key) else {
            continue;
        };
        let trimmed = proxy_url.trim();
        if trimmed.is_empty() {
            continue;
        }

        let candidate = if trimmed.contains("://") {
            trimmed.to_string()
        } else {
            format!("http://{trimmed}")
        };
        let Ok(parsed) = Url::parse(&candidate) else {
            continue;
        };
        let Some(host) = parsed.host_str() else {
            continue;
        };
        if !is_loopback_host(host) {
            continue;
        }

        let scheme = parsed.scheme().to_ascii_lowercase();
        let port = parsed
            .port()
            .unwrap_or_else(|| proxy_scheme_default_port(scheme.as_str()));
        ports.insert(port);
    }
    ports.into_iter().collect()
}

#[cfg(target_os = "macos")]
fn proxy_policy_inputs_from_env(env: &HashMap<String, String>) -> ProxyPolicyInputs {
    ProxyPolicyInputs {
        ports: proxy_loopback_ports_from_env(env),
        has_proxy_config: has_proxy_url_env_vars(env),
    }
}

#[cfg(target_os = "macos")]
fn managed_network_enabled(env: &HashMap<String, String>) -> bool {
    env_bool(env.get(MANAGED_NETWORK_ENV_KEY).map(String::as_str))
}

#[cfg(target_os = "macos")]
fn dynamic_network_policy(
    sandbox_policy: &SandboxPolicy,
    enforce_managed_network: bool,
    allow_local_binding: bool,
    proxy: &ProxyPolicyInputs,
) -> String {
    if !sandbox_policy.has_full_network_access() {
        return String::new();
    }

    if !proxy.ports.is_empty() {
        let mut policy =
            String::from("; allow outbound access only to configured loopback proxy endpoints\n");
        if allow_local_binding {
            policy.push_str("; allow localhost-only binding and loopback traffic\n");
            policy.push_str("(allow network-bind (local ip \"localhost:*\"))\n");
            policy.push_str("(allow network-inbound (local ip \"localhost:*\"))\n");
            policy.push_str("(allow network-outbound (remote ip \"localhost:*\"))\n");
        }
        for port in &proxy.ports {
            policy.push_str(&format!(
                "(allow network-outbound (remote ip \"localhost:{port}\"))\n"
            ));
        }
        return format!("{policy}{MACOS_SEATBELT_NETWORK_POLICY}");
    }

    if proxy.has_proxy_config || enforce_managed_network {
        return String::new();
    }

    format!("(allow network-outbound)\n(allow network-inbound)\n{MACOS_SEATBELT_NETWORK_POLICY}")
}

#[cfg(target_os = "macos")]
fn sandbox_network_env_snapshot() -> HashMap<String, String> {
    let mut env = HashMap::new();
    for key in PROXY_URL_ENV_KEYS {
        if let Ok(value) = std::env::var(key) {
            env.insert(key.to_string(), value);
        }
    }
    for key in [MANAGED_NETWORK_ENV_KEY, ALLOW_LOCAL_BINDING_ENV_KEY] {
        if let Ok(value) = std::env::var(key) {
            env.insert(key.to_string(), value);
        }
    }
    env
}

#[cfg(target_os = "macos")]
fn create_seatbelt_command_args(
    command: Vec<String>,
    sandbox_policy: &SandboxPolicy,
    managed_network_policy: &ManagedNetworkPolicy,
    network_env: &HashMap<String, String>,
    sandbox_policy_cwd: &Path,
    session_temp_dir: &Path,
) -> Vec<String> {
    let (file_write_policy, file_write_dir_params) = {
        if sandbox_policy.has_full_disk_write_access() {
            (
                r#"(allow file-write* (regex #"^/"))"#.to_string(),
                Vec::new(),
            )
        } else {
            let writable_roots = sandbox_policy
                .get_writable_roots_with_cwd(sandbox_policy_cwd, Some(session_temp_dir));
            let mut writable_folder_policies = Vec::new();
            let mut file_write_params = Vec::new();

            for (index, wr) in writable_roots.iter().enumerate() {
                // NOTE: macOS has multiple common path spellings for the same locations:
                // - `/tmp` vs `/private/tmp`
                // - `/var/...` vs `/private/var/...`
                //
                // Seatbelt path matching is sensitive to these differences in practice, so we
                // include both the original and canonicalized paths for each writable root (and
                // any read-only exclusions) to avoid accidental denials.
                let mut root_candidates = Vec::new();
                root_candidates.push(wr.root.clone());
                if let Ok(canonical_root) = wr.root.canonicalize() {
                    root_candidates.push(canonical_root);
                }
                let mut seen_root = std::collections::HashSet::<String>::new();
                let mut root_params = Vec::new();
                for (variant, root_path) in root_candidates.into_iter().enumerate() {
                    let key = root_path.to_string_lossy().to_string();
                    if !seen_root.insert(key) {
                        continue;
                    }
                    let root_param = if variant == 0 {
                        format!("WRITABLE_ROOT_{index}")
                    } else {
                        format!("WRITABLE_ROOT_{index}_{variant}")
                    };
                    file_write_params.push((root_param.clone(), root_path));
                    root_params.push(root_param);
                }

                if wr.read_only_subpaths.is_empty() {
                    for root_param in root_params {
                        writable_folder_policies
                            .push(format!("(subpath (param \"{root_param}\"))"));
                    }
                } else {
                    for root_param in root_params {
                        let mut require_parts = Vec::new();
                        require_parts.push(format!("(subpath (param \"{root_param}\"))"));

                        for (subpath_index, ro) in wr.read_only_subpaths.iter().enumerate() {
                            let mut ro_candidates = Vec::new();
                            ro_candidates.push(ro.to_path_buf());
                            if let Ok(canonical_ro) = ro.canonicalize() {
                                ro_candidates.push(canonical_ro);
                            }
                            let mut seen_ro = std::collections::HashSet::<String>::new();
                            for (ro_variant, ro_path) in ro_candidates.into_iter().enumerate() {
                                let key = ro_path.to_string_lossy().to_string();
                                if !seen_ro.insert(key) {
                                    continue;
                                }
                                let ro_param = if ro_variant == 0 {
                                    format!("WRITABLE_ROOT_{index}_RO_{subpath_index}")
                                } else {
                                    format!("WRITABLE_ROOT_{index}_RO_{subpath_index}_{ro_variant}")
                                };
                                require_parts.push(format!(
                                    "(require-not (subpath (param \"{ro_param}\")))"
                                ));
                                file_write_params.push((ro_param, ro_path));
                            }
                        }

                        writable_folder_policies
                            .push(format!("(require-all {} )", require_parts.join(" ")));
                    }
                }
            }

            if writable_folder_policies.is_empty() {
                ("".to_string(), Vec::new())
            } else {
                let file_write_policy = format!(
                    "(allow file-write*\n{}\n)",
                    writable_folder_policies.join(" ")
                );
                (file_write_policy, file_write_params)
            }
        }
    };

    let file_read_policy = if sandbox_policy.has_full_disk_read_access() {
        "; allow read-only file operations\n(allow file-read*)"
    } else {
        ""
    };

    let proxy = proxy_policy_inputs_from_env(network_env);
    let allow_local_binding = managed_network_policy.allow_local_binding;
    let enforce_managed_network =
        managed_network_enabled(network_env) || managed_network_policy.has_domain_restrictions();
    let network_policy = dynamic_network_policy(
        sandbox_policy,
        enforce_managed_network,
        allow_local_binding,
        &proxy,
    );

    let full_policy = format!(
        "{MACOS_SEATBELT_BASE_POLICY}\n{file_read_policy}\n{file_write_policy}\n{network_policy}"
    );

    let dir_params = [file_write_dir_params, macos_dir_params()].concat();

    let mut seatbelt_args = vec!["-p".to_string(), full_policy];
    let definition_args = dir_params
        .into_iter()
        .map(|(key, value)| format!("-D{key}={value}", value = value.to_string_lossy()));
    seatbelt_args.extend(definition_args);
    seatbelt_args.push("--".to_string());
    seatbelt_args.extend(command);
    seatbelt_args
}

#[cfg(target_os = "linux")]
pub fn run_linux_sandbox_main() -> ! {
    match linux_sandbox_main_impl() {
        Ok(()) => process::exit(0),
        Err(err) => {
            eprintln!("{err}");
            process::exit(1);
        }
    }
}

#[cfg(target_os = "linux")]
struct LinuxSandboxArgs {
    sandbox_policy_cwd: PathBuf,
    sandbox_policy: SandboxPolicy,
    command: Vec<std::ffi::OsString>,
    use_bwrap_sandbox: bool,
    apply_seccomp_then_exec: bool,
    no_proc: bool,
}

#[cfg(target_os = "linux")]
fn linux_sandbox_main_impl() -> Result<(), String> {
    let args = linux_sandbox_parse_args()?;
    if args.apply_seccomp_then_exec {
        linux_apply_sandbox_policy_to_current_thread(
            &args.sandbox_policy,
            &args.sandbox_policy_cwd,
        )?;
        linux_execvp(args.command)?;
        return Ok(());
    }
    if args.use_bwrap_sandbox {
        linux_exec_bwrap_sandbox(args)?;
        return Ok(());
    }
    linux_apply_sandbox_policy_to_current_thread(&args.sandbox_policy, &args.sandbox_policy_cwd)?;
    linux_execvp(args.command)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_sandbox_parse_args() -> Result<LinuxSandboxArgs, String> {
    let mut sandbox_policy_cwd: Option<PathBuf> = None;
    let mut sandbox_policy: Option<SandboxPolicy> = None;
    let mut command: Vec<std::ffi::OsString> = Vec::new();
    let mut use_bwrap_sandbox = false;
    let mut apply_seccomp_then_exec = false;
    let mut no_proc = false;

    let mut args = std::env::args_os().skip(1).peekable();
    while let Some(arg) = args.next() {
        if arg == "--use-bwrap-sandbox" {
            use_bwrap_sandbox = true;
            continue;
        }
        if arg == "--apply-seccomp-then-exec" {
            apply_seccomp_then_exec = true;
            continue;
        }
        if arg == "--no-proc" {
            no_proc = true;
            continue;
        }
        if arg == "--sandbox-policy-cwd" {
            let value = args
                .next()
                .ok_or_else(|| "missing value for --sandbox-policy-cwd".to_string())?;
            sandbox_policy_cwd = Some(PathBuf::from(value));
            continue;
        }
        if arg == "--sandbox-policy" {
            let value = args
                .next()
                .ok_or_else(|| "missing value for --sandbox-policy".to_string())?;
            let value = value
                .into_string()
                .map_err(|_| "--sandbox-policy must be valid UTF-8".to_string())?;
            sandbox_policy = Some(
                serde_json::from_str(&value)
                    .map_err(|err| format!("failed to parse --sandbox-policy: {err}"))?,
            );
            continue;
        }
        if arg == "--" {
            command.extend(args);
            break;
        }
        return Err(format!("unknown argument: {}", arg.to_string_lossy()));
    }

    let sandbox_policy_cwd =
        sandbox_policy_cwd.ok_or_else(|| "missing --sandbox-policy-cwd".to_string())?;
    let sandbox_policy = sandbox_policy.ok_or_else(|| "missing --sandbox-policy".to_string())?;
    if command.is_empty() {
        return Err("no command specified to execute".to_string());
    }

    Ok(LinuxSandboxArgs {
        sandbox_policy_cwd,
        sandbox_policy,
        command,
        use_bwrap_sandbox,
        apply_seccomp_then_exec,
        no_proc,
    })
}

#[cfg(target_os = "linux")]
fn linux_find_bwrap_program() -> Option<PathBuf> {
    let absolute = PathBuf::from("/usr/bin/bwrap");
    if absolute.is_file() {
        return Some(absolute);
    }

    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("bwrap");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn linux_build_inner_seccomp_command(args: &LinuxSandboxArgs) -> Result<Vec<String>, String> {
    let current_exe = std::env::current_exe().map_err(|err| err.to_string())?;
    let policy = sanitize_linux_sandbox_policy(&args.sandbox_policy);
    let policy_json = serde_json::to_string(&policy).map_err(|err| err.to_string())?;
    let mut inner = vec![
        current_exe.to_string_lossy().to_string(),
        "--sandbox-policy-cwd".to_string(),
        args.sandbox_policy_cwd.to_string_lossy().to_string(),
        "--sandbox-policy".to_string(),
        policy_json,
        "--apply-seccomp-then-exec".to_string(),
        "--".to_string(),
    ];
    inner.extend(
        args.command
            .iter()
            .map(|arg| arg.to_string_lossy().to_string()),
    );
    Ok(inner)
}

#[cfg(target_os = "linux")]
fn linux_exec_bwrap_sandbox(args: LinuxSandboxArgs) -> Result<(), String> {
    let bwrap_program = linux_find_bwrap_program()
        .ok_or_else(|| "bwrap executable not found (tried /usr/bin/bwrap and PATH)".to_string())?;
    let inner = linux_build_inner_seccomp_command(&args)?;
    let mount_proc = !args.no_proc
        && linux_bwrap_supports_proc_mount(
            bwrap_program.as_path(),
            &args.sandbox_policy,
            &args.sandbox_policy_cwd,
        );
    let bwrap_args = create_linux_bwrap_command_args(
        inner,
        &args.sandbox_policy,
        &args.sandbox_policy_cwd,
        mount_proc,
    )?;
    let mut full_command = Vec::with_capacity(1 + bwrap_args.len());
    full_command.push(bwrap_program.into_os_string());
    full_command.extend(bwrap_args.into_iter().map(std::ffi::OsString::from));
    linux_execvp(full_command)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_bwrap_supports_proc_mount(
    bwrap_program: &Path,
    sandbox_policy: &SandboxPolicy,
    sandbox_policy_cwd: &Path,
) -> bool {
    let true_path = if Path::new("/usr/bin/true").is_file() {
        "/usr/bin/true"
    } else if Path::new("/bin/true").is_file() {
        "/bin/true"
    } else {
        "true"
    };
    let args = match create_linux_bwrap_command_args(
        vec![true_path.to_string()],
        sandbox_policy,
        sandbox_policy_cwd,
        true,
    ) {
        Ok(args) => args,
        Err(_) => return false,
    };
    let output = std::process::Command::new(bwrap_program)
        .args(&args)
        .output();
    let Ok(output) = output else {
        return false;
    };
    if output.status.success() {
        return true;
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if is_proc_mount_failure(stderr.as_ref()) {
        eprintln!("codex-linux-sandbox: bwrap could not mount /proc; retrying with --no-proc");
        return false;
    }
    true
}

#[cfg(target_os = "linux")]
fn create_linux_bwrap_command_args(
    command: Vec<String>,
    sandbox_policy: &SandboxPolicy,
    sandbox_policy_cwd: &Path,
    mount_proc: bool,
) -> Result<Vec<String>, String> {
    let sandbox_policy = sanitize_linux_sandbox_policy(sandbox_policy);
    let writable_roots = linux_writable_roots(&sandbox_policy, sandbox_policy_cwd);
    linux_ensure_bwrap_mount_targets_exist(&writable_roots)?;

    let mut bwrap_args = vec![
        "--die-with-parent".to_string(),
        "--new-session".to_string(),
        "--unshare-pid".to_string(),
    ];
    if !sandbox_policy.has_full_network_access() {
        bwrap_args.push("--unshare-net".to_string());
    }
    if mount_proc {
        bwrap_args.push("--proc".to_string());
        bwrap_args.push("/proc".to_string());
    }
    bwrap_args.extend(["--ro-bind".to_string(), "/".to_string(), "/".to_string()]);

    for root in &writable_roots {
        let root_str = root.to_string_lossy().to_string();
        bwrap_args.extend(["--bind".to_string(), root_str.clone(), root_str]);
    }

    let read_only_subpaths = collect_linux_read_only_subpaths(&writable_roots);
    for subpath in read_only_subpaths {
        if let Some(symlink_path) = find_symlink_in_path(&subpath, &writable_roots) {
            let target = symlink_path.to_string_lossy().to_string();
            bwrap_args.extend(["--ro-bind".to_string(), "/dev/null".to_string(), target]);
            continue;
        }

        if !subpath.exists() {
            if let Some(first_missing) = find_first_non_existent_component(&subpath)
                && is_within_allowed_write_paths(&first_missing, &writable_roots)
            {
                let target = first_missing.to_string_lossy().to_string();
                bwrap_args.extend(["--ro-bind".to_string(), "/dev/null".to_string(), target]);
            }
            continue;
        }

        if is_within_allowed_write_paths(&subpath, &writable_roots) {
            let target = subpath.to_string_lossy().to_string();
            bwrap_args.extend(["--ro-bind".to_string(), target.clone(), target]);
        }
    }

    bwrap_args.extend([
        "--dev-bind".to_string(),
        "/dev/null".to_string(),
        "/dev/null".to_string(),
    ]);

    let command_index = bwrap_args.len();
    bwrap_args.push("--".to_string());
    bwrap_args.extend(command);
    bwrap_args.splice(
        command_index..command_index,
        ["--argv0".to_string(), "codex-linux-sandbox".to_string()],
    );
    Ok(bwrap_args)
}

#[cfg(target_os = "linux")]
fn linux_ensure_bwrap_mount_targets_exist(writable_roots: &[PathBuf]) -> Result<(), String> {
    for root in writable_roots {
        if !root.exists() {
            return Err(format!(
                "sandbox expected writable root {}, but it does not exist",
                root.display()
            ));
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn collect_linux_read_only_subpaths(writable_roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut subpaths = std::collections::BTreeSet::<PathBuf>::new();
    for root in writable_roots {
        for subpath in compute_linux_read_only_subpaths(root) {
            subpaths.insert(subpath);
        }
    }
    subpaths.into_iter().collect()
}

#[cfg(target_os = "linux")]
fn is_within_allowed_write_paths(path: &Path, allowed_write_paths: &[PathBuf]) -> bool {
    allowed_write_paths
        .iter()
        .any(|root| path.starts_with(root.as_path()))
}

#[cfg(target_os = "linux")]
fn find_symlink_in_path(target_path: &Path, allowed_write_paths: &[PathBuf]) -> Option<PathBuf> {
    let mut current = PathBuf::new();
    for component in target_path.components() {
        use std::path::Component;
        match component {
            Component::RootDir => {
                current.push(Path::new("/"));
                continue;
            }
            Component::CurDir => continue,
            Component::ParentDir => {
                current.pop();
                continue;
            }
            Component::Normal(part) => current.push(part),
            Component::Prefix(_) => continue,
        }

        let metadata = match std::fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(_) => break,
        };
        if metadata.file_type().is_symlink()
            && is_within_allowed_write_paths(&current, allowed_write_paths)
        {
            return Some(current);
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn find_first_non_existent_component(target_path: &Path) -> Option<PathBuf> {
    let mut current = PathBuf::new();
    for component in target_path.components() {
        use std::path::Component;
        match component {
            Component::RootDir => {
                current.push(Path::new("/"));
                continue;
            }
            Component::CurDir => continue,
            Component::ParentDir => {
                current.pop();
                continue;
            }
            Component::Normal(part) => current.push(part),
            Component::Prefix(_) => continue,
        }
        if !current.exists() {
            return Some(current);
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn is_proc_mount_failure(stderr: &str) -> bool {
    stderr.contains("Can't mount proc") || stderr.contains("mount proc")
}

#[cfg(target_os = "linux")]
fn linux_apply_sandbox_policy_to_current_thread(
    sandbox_policy: &SandboxPolicy,
    cwd: &Path,
) -> Result<(), String> {
    if !sandbox_policy.has_full_disk_write_access() || !sandbox_policy.has_full_network_access() {
        linux_set_no_new_privs()?;
    }

    if !sandbox_policy.has_full_network_access() {
        linux_install_network_seccomp_filter_on_current_thread()?;
    }

    if !sandbox_policy.has_full_disk_write_access() {
        let writable_roots = linux_writable_roots(sandbox_policy, cwd);
        linux_install_filesystem_landlock_rules_on_current_thread(writable_roots)?;
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_set_no_new_privs() -> Result<(), String> {
    let result = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if result != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_writable_roots(policy: &SandboxPolicy, cwd: &Path) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    let Some(cwd) = ensure_absolute(cwd.to_path_buf()) else {
        return roots;
    };

    if let SandboxPolicy::WorkspaceWrite {
        writable_roots,
        exclude_tmpdir_env_var,
        exclude_slash_tmp,
        network_access: _,
    } = policy
    {
        roots.extend(writable_roots.iter().cloned().filter_map(ensure_absolute));
        roots.push(cwd);
        roots.extend(temp_roots_from_system(
            *exclude_tmpdir_env_var,
            *exclude_slash_tmp,
        ));
    }

    roots.sort();
    roots.dedup();
    roots
}

#[cfg(target_os = "linux")]
fn linux_install_filesystem_landlock_rules_on_current_thread(
    writable_roots: Vec<PathBuf>,
) -> Result<(), String> {
    use landlock::{
        ABI, Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr, RulesetCreatedAttr,
    };

    let abi = ABI::V5;
    let access_rw = AccessFs::from_all(abi);
    let access_ro = AccessFs::from_read(abi);

    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(access_rw)
        .map_err(|err| err.to_string())?
        .create()
        .map_err(|err| err.to_string())?
        .add_rules(landlock::path_beneath_rules(&["/"], access_ro))
        .map_err(|err| err.to_string())?
        .add_rules(landlock::path_beneath_rules(&["/dev/null"], access_rw))
        .map_err(|err| err.to_string())?
        .set_no_new_privs(true);

    if !writable_roots.is_empty() {
        ruleset = ruleset
            .add_rules(landlock::path_beneath_rules(&writable_roots, access_rw))
            .map_err(|err| err.to_string())?;
    }

    let status = ruleset.restrict_self().map_err(|err| err.to_string())?;
    if status.ruleset == landlock::RulesetStatus::NotEnforced {
        return Err("landlock ruleset not enforced".to_string());
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_install_network_seccomp_filter_on_current_thread() -> Result<(), String> {
    use seccompiler::{
        BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
        SeccompRule, TargetArch, apply_filter,
    };
    use std::collections::BTreeMap;

    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    let mut deny_syscall = |nr: i64| {
        rules.insert(nr, vec![]);
    };

    deny_syscall(libc::SYS_connect);
    deny_syscall(libc::SYS_accept);
    deny_syscall(libc::SYS_accept4);
    deny_syscall(libc::SYS_bind);
    deny_syscall(libc::SYS_listen);
    deny_syscall(libc::SYS_getpeername);
    deny_syscall(libc::SYS_getsockname);
    deny_syscall(libc::SYS_shutdown);
    deny_syscall(libc::SYS_sendto);
    deny_syscall(libc::SYS_sendmmsg);
    deny_syscall(libc::SYS_recvmmsg);
    deny_syscall(libc::SYS_getsockopt);
    deny_syscall(libc::SYS_setsockopt);
    deny_syscall(libc::SYS_ptrace);
    deny_syscall(libc::SYS_io_uring_setup);
    deny_syscall(libc::SYS_io_uring_enter);
    deny_syscall(libc::SYS_io_uring_register);

    let unix_only_rule = SeccompRule::new(vec![
        SeccompCondition::new(
            0,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::Ne,
            libc::AF_UNIX as u64,
        )
        .map_err(|err| err.to_string())?,
    ])
    .map_err(|err| err.to_string())?;

    rules.insert(libc::SYS_socket, vec![unix_only_rule.clone()]);
    rules.insert(libc::SYS_socketpair, vec![unix_only_rule]);

    let arch = if cfg!(target_arch = "x86_64") {
        TargetArch::x86_64
    } else if cfg!(target_arch = "aarch64") {
        TargetArch::aarch64
    } else {
        return Err("unsupported architecture for seccomp filter".to_string());
    };

    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        arch,
    )
    .map_err(|err| err.to_string())?;

    let prog: BpfProgram = filter
        .try_into()
        .map_err(|err: seccompiler::BackendError| err.to_string())?;
    apply_filter(&prog).map_err(|err: seccompiler::Error| err.to_string())?;

    Ok(())
}

#[cfg(target_os = "windows")]
pub fn invoked_as_codex_windows_sandbox() -> bool {
    std::env::args_os().nth(1).as_deref() == Some(OsStr::new("--windows-sandbox"))
}

#[cfg(target_os = "windows")]
pub fn run_windows_sandbox_main() -> ! {
    match windows_sandbox_main_impl() {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(1);
        }
    }
}

#[cfg(target_os = "windows")]
fn windows_sandbox_main_impl() -> Result<i32, String> {
    let args = windows_sandbox_parse_args()?;
    crate::windows_sandbox::run_sandboxed_command(
        &args.sandbox_policy,
        &args.sandbox_policy_cwd,
        &args.command,
        args.prepared_capability_sid.as_deref(),
    )
}

#[cfg(target_os = "windows")]
struct WindowsSandboxArgs {
    sandbox_policy_cwd: PathBuf,
    sandbox_policy: SandboxPolicy,
    prepared_capability_sid: Option<String>,
    command: Vec<String>,
}

#[cfg(target_os = "windows")]
fn windows_sandbox_parse_args() -> Result<WindowsSandboxArgs, String> {
    windows_sandbox_parse_args_from(std::env::args_os().skip(1).collect())
}

#[cfg(target_os = "windows")]
fn windows_sandbox_parse_args_from(raw_args: Vec<OsString>) -> Result<WindowsSandboxArgs, String> {
    let mut sandbox_policy_cwd: Option<PathBuf> = None;
    let mut sandbox_policy: Option<SandboxPolicy> = None;
    let mut prepared_capability_sid: Option<String> = None;
    let mut command: Vec<String> = Vec::new();

    let mut args = raw_args.into_iter().peekable();
    while let Some(arg) = args.next() {
        if arg == "--windows-sandbox" {
            continue;
        }
        if arg == "--sandbox-policy-cwd" {
            let value = args
                .next()
                .ok_or_else(|| "missing value for --sandbox-policy-cwd".to_string())?;
            sandbox_policy_cwd = Some(PathBuf::from(value));
            continue;
        }
        if arg == "--sandbox-policy" {
            let value = args
                .next()
                .ok_or_else(|| "missing value for --sandbox-policy".to_string())?;
            let value = value
                .into_string()
                .map_err(|_| "--sandbox-policy must be valid UTF-8".to_string())?;
            sandbox_policy = Some(
                serde_json::from_str(&value)
                    .map_err(|err| format!("failed to parse --sandbox-policy: {err}"))?,
            );
            continue;
        }
        if arg == "--prepared-capability-sid" {
            let value = args
                .next()
                .ok_or_else(|| "missing value for --prepared-capability-sid".to_string())?;
            prepared_capability_sid = Some(
                value
                    .into_string()
                    .map_err(|_| "--prepared-capability-sid must be valid UTF-8".to_string())?,
            );
            continue;
        }
        if arg == "--" {
            command.extend(args.map(|value| value.to_string_lossy().to_string()));
            break;
        }
        return Err(format!("unknown argument: {}", arg.to_string_lossy()));
    }

    let sandbox_policy_cwd =
        sandbox_policy_cwd.ok_or_else(|| "missing --sandbox-policy-cwd".to_string())?;
    let sandbox_policy = sandbox_policy.ok_or_else(|| "missing --sandbox-policy".to_string())?;
    if command.is_empty() {
        return Err("no command specified to execute".to_string());
    }

    Ok(WindowsSandboxArgs {
        sandbox_policy_cwd,
        sandbox_policy,
        prepared_capability_sid,
        command,
    })
}

#[cfg(target_os = "windows")]
pub fn append_windows_prepared_capability_sid(
    args: &mut Vec<String>,
    capability_sid: &str,
) -> Result<(), String> {
    let separator_index = args
        .iter()
        .position(|arg| arg == "--")
        .ok_or_else(|| "windows sandbox args missing command separator".to_string())?;
    args.insert(separator_index, "--prepared-capability-sid".to_string());
    args.insert(separator_index + 1, capability_sid.to_string());
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_execvp(command: Vec<std::ffi::OsString>) -> Result<(), String> {
    let cstrings: Vec<CString> = command
        .iter()
        .map(|arg| {
            CString::new(arg.as_os_str().as_bytes()).map_err(|_| "NUL byte in arg".to_string())
        })
        .collect::<Result<_, _>>()?;
    let mut ptrs: Vec<*const libc::c_char> = cstrings.iter().map(|arg| arg.as_ptr()).collect();
    ptrs.push(std::ptr::null());

    unsafe {
        libc::execvp(cstrings[0].as_ptr(), ptrs.as_ptr());
    }

    Err(format!(
        "failed to execvp {}: {}",
        PathBuf::from(&command[0]).display(),
        std::io::Error::last_os_error()
    ))
}

#[cfg(target_os = "macos")]
fn confstr(name: libc::c_int) -> Option<String> {
    let mut buf = vec![0_i8; (libc::PATH_MAX as usize) + 1];
    let len = unsafe { libc::confstr(name, buf.as_mut_ptr(), buf.len()) };
    if len == 0 {
        return None;
    }
    let cstr = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) };
    cstr.to_str().ok().map(ToString::to_string)
}

#[cfg(target_os = "macos")]
fn confstr_path(name: libc::c_int) -> Option<PathBuf> {
    let s = confstr(name)?;
    let path = PathBuf::from(s);
    path.canonicalize().ok().or(Some(path))
}

#[cfg(target_os = "macos")]
fn macos_dir_params() -> Vec<(String, PathBuf)> {
    if let Some(p) = confstr_path(libc::_CS_DARWIN_USER_CACHE_DIR) {
        return vec![("DARWIN_USER_CACHE_DIR".to_string(), p)];
    }
    vec![]
}

#[cfg(target_os = "macos")]
fn log_denials_enabled() -> bool {
    std::env::var_os(SANDBOX_LOG_DENIALS_ENV).is_some()
}

#[cfg(target_os = "macos")]
pub use macos_denials::{DenialLogger, SandboxDenial};

#[cfg(target_os = "macos")]
mod macos_denials {
    use std::collections::HashSet;
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};
    use std::thread::JoinHandle;

    pub struct SandboxDenial {
        pub name: String,
        pub capability: String,
    }

    pub struct DenialLogger {
        log_stream: Child,
        pid_tracker: Option<PidTracker>,
        log_reader: Option<JoinHandle<Vec<u8>>>,
    }

    impl DenialLogger {
        pub(crate) fn new() -> Option<Self> {
            let mut log_stream = start_log_stream()?;
            let stdout = log_stream.stdout.take()?;
            let log_reader = std::thread::spawn(move || {
                let mut reader = BufReader::new(stdout);
                let mut logs = Vec::new();
                let mut chunk = Vec::new();
                loop {
                    match reader.read_until(b'\n', &mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            logs.extend_from_slice(&chunk);
                            chunk.clear();
                        }
                    }
                }
                logs
            });

            Some(Self {
                log_stream,
                pid_tracker: None,
                log_reader: Some(log_reader),
            })
        }

        pub(crate) fn on_child_spawn(&mut self, child: &Child) {
            let root_pid = child.id() as i32;
            if root_pid > 0 {
                self.pid_tracker = PidTracker::new(root_pid);
            }
        }

        pub(crate) fn finish(mut self) -> Vec<SandboxDenial> {
            let pid_set = match self.pid_tracker {
                Some(tracker) => tracker.stop(),
                None => Default::default(),
            };

            if pid_set.is_empty() {
                return Vec::new();
            }

            let _ = self.log_stream.kill();
            let _ = self.log_stream.wait();

            let logs_bytes = match self.log_reader.take() {
                Some(handle) => handle.join().unwrap_or_default(),
                None => Vec::new(),
            };
            let logs = String::from_utf8_lossy(&logs_bytes);

            let mut seen: HashSet<(String, String)> = HashSet::new();
            let mut denials = Vec::new();
            for line in logs.lines() {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(line)
                    && let Some(msg) = json.get("eventMessage").and_then(|v| v.as_str())
                    && let Some((pid, name, capability)) = parse_message(msg)
                    && pid_set.contains(&pid)
                    && seen.insert((name.clone(), capability.clone()))
                {
                    denials.push(SandboxDenial { name, capability });
                }
            }
            denials
        }
    }

    fn start_log_stream() -> Option<Child> {
        const PREDICATE: &str = r#"(((processID == 0) AND (senderImagePath CONTAINS "/Sandbox")) OR (subsystem == "com.apple.sandbox.reporting"))"#;

        Command::new("log")
            .args(["stream", "--style", "ndjson", "--predicate", PREDICATE])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()
    }

    fn parse_message(msg: &str) -> Option<(i32, String, String)> {
        static RE: std::sync::OnceLock<regex_lite::Regex> = std::sync::OnceLock::new();
        let re = RE.get_or_init(|| {
            regex_lite::Regex::new(r"^Sandbox:\s*(.+?)\((\d+)\)\s+deny\(.*?\)\s*(.+)$")
                .expect("failed to compile sandbox denial regex")
        });

        let (_, [name, pid_str, capability]) = re.captures(msg)?.extract();
        let pid = pid_str.trim().parse::<i32>().ok()?;
        Some((pid, name.to_string(), capability.to_string()))
    }

    struct PidTracker {
        kq: libc::c_int,
        handle: JoinHandle<HashSet<i32>>,
    }

    impl PidTracker {
        fn new(root_pid: i32) -> Option<Self> {
            if root_pid <= 0 {
                return None;
            }

            let kq = unsafe { libc::kqueue() };
            let handle = std::thread::spawn(move || track_descendants(kq, root_pid));

            Some(Self { kq, handle })
        }

        fn stop(self) -> HashSet<i32> {
            trigger_stop_event(self.kq);
            self.handle.join().unwrap_or_default()
        }
    }

    unsafe extern "C" {
        fn proc_listchildpids(
            ppid: libc::c_int,
            buffer: *mut libc::c_void,
            buffersize: libc::c_int,
        ) -> libc::c_int;
    }

    fn list_child_pids(parent: i32) -> Vec<i32> {
        unsafe {
            let mut capacity: usize = 16;
            loop {
                let mut buf: Vec<i32> = vec![0; capacity];
                let count = proc_listchildpids(
                    parent as libc::c_int,
                    buf.as_mut_ptr() as *mut libc::c_void,
                    (buf.len() * std::mem::size_of::<i32>()) as libc::c_int,
                );
                if count <= 0 {
                    return Vec::new();
                }
                let returned = count as usize;
                if returned < capacity {
                    buf.truncate(returned);
                    return buf;
                }
                capacity = capacity.saturating_mul(2).max(returned + 16);
            }
        }
    }

    fn pid_is_alive(pid: i32) -> bool {
        if pid <= 0 {
            return false;
        }
        let res = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if res == 0 {
            true
        } else {
            matches!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EPERM)
            )
        }
    }

    enum WatchPidError {
        ProcessGone,
        Other(std::io::Error),
    }

    fn watch_pid(kq: libc::c_int, pid: i32) -> Result<(), WatchPidError> {
        if pid <= 0 {
            return Err(WatchPidError::ProcessGone);
        }

        let kev = libc::kevent {
            ident: pid as libc::uintptr_t,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_CLEAR,
            fflags: libc::NOTE_FORK | libc::NOTE_EXEC | libc::NOTE_EXIT,
            data: 0,
            udata: std::ptr::null_mut(),
        };

        let res = unsafe { libc::kevent(kq, &kev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
        if res < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ESRCH) {
                Err(WatchPidError::ProcessGone)
            } else {
                Err(WatchPidError::Other(err))
            }
        } else {
            Ok(())
        }
    }

    fn watch_children(
        kq: libc::c_int,
        parent: i32,
        seen: &mut HashSet<i32>,
        active: &mut HashSet<i32>,
    ) {
        for child_pid in list_child_pids(parent) {
            add_pid_watch(kq, child_pid, seen, active);
        }
    }

    fn add_pid_watch(
        kq: libc::c_int,
        pid: i32,
        seen: &mut HashSet<i32>,
        active: &mut HashSet<i32>,
    ) {
        if pid <= 0 {
            return;
        }

        let newly_seen = seen.insert(pid);
        let mut should_recurse = newly_seen;

        if active.insert(pid) {
            match watch_pid(kq, pid) {
                Ok(()) => {
                    should_recurse = true;
                }
                Err(WatchPidError::ProcessGone) => {
                    active.remove(&pid);
                    return;
                }
                Err(WatchPidError::Other(err)) => {
                    eprintln!("failed to watch pid {pid}: {err}");
                    active.remove(&pid);
                    return;
                }
            }
        }

        if should_recurse {
            watch_children(kq, pid, seen, active);
        }
    }

    const STOP_IDENT: libc::uintptr_t = 1;

    fn register_stop_event(kq: libc::c_int) -> bool {
        let kev = libc::kevent {
            ident: STOP_IDENT,
            filter: libc::EVFILT_USER,
            flags: libc::EV_ADD | libc::EV_CLEAR,
            fflags: 0,
            data: 0,
            udata: std::ptr::null_mut(),
        };

        let res = unsafe { libc::kevent(kq, &kev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
        res >= 0
    }

    fn trigger_stop_event(kq: libc::c_int) {
        if kq < 0 {
            return;
        }

        let kev = libc::kevent {
            ident: STOP_IDENT,
            filter: libc::EVFILT_USER,
            flags: 0,
            fflags: libc::NOTE_TRIGGER,
            data: 0,
            udata: std::ptr::null_mut(),
        };

        let _ = unsafe { libc::kevent(kq, &kev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
    }

    fn track_descendants(kq: libc::c_int, root_pid: i32) -> HashSet<i32> {
        if kq < 0 {
            let mut seen = HashSet::new();
            seen.insert(root_pid);
            return seen;
        }

        if !register_stop_event(kq) {
            let mut seen = HashSet::new();
            seen.insert(root_pid);
            let _ = unsafe { libc::close(kq) };
            return seen;
        }

        let mut seen: HashSet<i32> = HashSet::new();
        let mut active: HashSet<i32> = HashSet::new();

        add_pid_watch(kq, root_pid, &mut seen, &mut active);

        const EVENTS_CAP: usize = 32;
        let mut events: [libc::kevent; EVENTS_CAP] =
            unsafe { std::mem::MaybeUninit::zeroed().assume_init() };

        let mut stop_requested = false;
        loop {
            if active.is_empty() {
                if !pid_is_alive(root_pid) {
                    break;
                }
                add_pid_watch(kq, root_pid, &mut seen, &mut active);
                if active.is_empty() {
                    continue;
                }
            }

            let nev = unsafe {
                libc::kevent(
                    kq,
                    std::ptr::null::<libc::kevent>(),
                    0,
                    events.as_mut_ptr(),
                    EVENTS_CAP as libc::c_int,
                    std::ptr::null(),
                )
            };

            if nev < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }

            if nev == 0 {
                continue;
            }

            for ev in events.iter().take(nev as usize) {
                let pid = ev.ident as i32;

                if ev.filter == libc::EVFILT_USER && ev.ident == STOP_IDENT {
                    stop_requested = true;
                    break;
                }

                if (ev.flags & libc::EV_ERROR) != 0 {
                    if ev.data == libc::ESRCH as isize {
                        active.remove(&pid);
                    }
                    continue;
                }

                if (ev.fflags & libc::NOTE_FORK) != 0 {
                    watch_children(kq, pid, &mut seen, &mut active);
                }

                if (ev.fflags & libc::NOTE_EXIT) != 0 {
                    active.remove(&pid);
                }
            }

            if stop_requested {
                break;
            }
        }

        let _ = unsafe { libc::close(kq) };

        seen
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "macos")]
    use std::collections::HashMap;
    use std::path::Path;
    use std::path::PathBuf;

    #[test]
    fn session_temp_dir_rejects_outside_system_tmp() {
        #[cfg(target_os = "windows")]
        let outside = {
            let system_drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
            PathBuf::from(format!(r"{system_drive}\mcp-repl-test"))
        };
        #[cfg(not(target_os = "windows"))]
        let base_tmp = std::env::temp_dir();
        #[cfg(not(target_os = "windows"))]
        let outside = if base_tmp.starts_with("/tmp") {
            PathBuf::from("/var/mcp-repl-test")
        } else {
            PathBuf::from("/tmp/mcp-repl-test")
        };
        let err = prepare_session_temp_dir(&outside).expect_err("expected failure");
        match err {
            SandboxError::SessionTempDir(message) => {
                assert!(
                    message.contains("outside system temp"),
                    "unexpected error message: {message}"
                );
            }
            #[cfg(target_os = "macos")]
            SandboxError::SeatbeltMissing => {
                panic!("unexpected error: SeatbeltMissing")
            }
            #[cfg(target_os = "windows")]
            SandboxError::WindowsSandbox(message) => {
                panic!("unexpected error: {message}")
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn proxy_loopback_ports_from_env_extracts_loopback_endpoints() {
        let mut env = HashMap::new();
        env.insert(
            "HTTP_PROXY".to_string(),
            "http://127.0.0.1:8080".to_string(),
        );
        env.insert("HTTPS_PROXY".to_string(), "https://localhost".to_string());
        env.insert(
            "ALL_PROXY".to_string(),
            "http://example.com:3128".to_string(),
        );

        let ports = proxy_loopback_ports_from_env(&env);
        assert_eq!(ports, vec![443, 8080]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn dynamic_network_policy_fails_closed_when_proxy_config_has_no_loopback_endpoint() {
        let mut env = HashMap::new();
        env.insert(
            "HTTP_PROXY".to_string(),
            "http://example.com:3128".to_string(),
        );
        let proxy = proxy_policy_inputs_from_env(&env);

        let policy = SandboxPolicy::WorkspaceWrite {
            writable_roots: Vec::new(),
            network_access: true,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
        };
        let rendered = dynamic_network_policy(&policy, false, false, &proxy);
        assert!(rendered.is_empty(), "expected fail-closed policy");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn dynamic_network_policy_fails_closed_for_managed_network_without_proxy() {
        let env = HashMap::new();
        let proxy = proxy_policy_inputs_from_env(&env);
        let policy = SandboxPolicy::WorkspaceWrite {
            writable_roots: Vec::new(),
            network_access: true,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
        };

        let rendered = dynamic_network_policy(&policy, true, false, &proxy);
        assert!(rendered.is_empty(), "expected fail-closed policy");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn dynamic_network_policy_allows_proxy_only_outbound_when_configured() {
        let mut env = HashMap::new();
        env.insert(
            "HTTP_PROXY".to_string(),
            "http://127.0.0.1:8080".to_string(),
        );
        let proxy = proxy_policy_inputs_from_env(&env);
        let policy = SandboxPolicy::WorkspaceWrite {
            writable_roots: Vec::new(),
            network_access: true,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
        };

        let rendered = dynamic_network_policy(&policy, false, false, &proxy);
        assert!(rendered.contains("localhost:8080"));
        assert!(!rendered.contains("(allow network-inbound)\n"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_mount_failure_detects_expected_stderr() {
        assert!(is_proc_mount_failure(
            "bwrap: Can't mount proc on /newroot/proc: Invalid argument"
        ));
        assert!(!is_proc_mount_failure("bwrap: unrelated failure"));
    }

    #[test]
    fn prepare_worker_command_sets_allow_local_binding_one_when_enabled() {
        let mut state = SandboxState {
            sandbox_policy: SandboxPolicy::DangerFullAccess,
            ..SandboxState::default()
        };
        state.managed_network_policy.allow_local_binding = true;

        let prepared =
            prepare_worker_command(Path::new("/bin/echo"), vec!["ok".to_string()], &state)
                .expect("prepare_worker_command should succeed");
        assert_eq!(
            prepared
                .env
                .get(ALLOW_LOCAL_BINDING_ENV_KEY)
                .map(String::as_str),
            Some("1"),
            "explicit true value should enable local binding"
        );
    }

    #[test]
    fn prepare_worker_command_sets_allow_local_binding_zero_when_explicitly_disabled() {
        let mut state = SandboxState {
            sandbox_policy: SandboxPolicy::DangerFullAccess,
            ..SandboxState::default()
        };
        state.managed_network_policy.allow_local_binding = false;

        let prepared =
            prepare_worker_command(Path::new("/bin/echo"), vec!["ok".to_string()], &state)
                .expect("prepare_worker_command should succeed");
        assert_eq!(
            prepared
                .env
                .get(ALLOW_LOCAL_BINDING_ENV_KEY)
                .map(String::as_str),
            Some("0"),
            "explicit false override should disable local binding even when inherited env enables it"
        );
    }

    #[test]
    fn prepare_worker_command_clears_managed_domain_env_when_lists_are_empty() {
        let mut state = SandboxState {
            sandbox_policy: SandboxPolicy::DangerFullAccess,
            ..SandboxState::default()
        };
        state.managed_network_policy.allowed_domains = Vec::new();
        state.managed_network_policy.denied_domains = Vec::new();

        let prepared =
            prepare_worker_command(Path::new("/bin/echo"), vec!["ok".to_string()], &state)
                .expect("prepare_worker_command should succeed");

        assert_eq!(
            prepared
                .env
                .get(MANAGED_ALLOWED_DOMAINS_ENV_KEY)
                .map(String::as_str),
            Some(""),
            "allowed domains must be explicitly cleared for child processes"
        );
        assert_eq!(
            prepared
                .env
                .get(MANAGED_DENIED_DOMAINS_ENV_KEY)
                .map(String::as_str),
            Some(""),
            "denied domains must be explicitly cleared for child processes"
        );
        assert_eq!(
            prepared
                .env
                .get(MANAGED_NETWORK_ENV_KEY)
                .map(String::as_str),
            Some("0"),
            "managed network marker must be explicitly disabled when no domain restrictions exist"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn append_windows_prepared_capability_sid_inserts_before_command_separator() {
        let mut args = vec![
            "--windows-sandbox".to_string(),
            "--sandbox-policy-cwd".to_string(),
            "C:\\workspace".to_string(),
            "--sandbox-policy".to_string(),
            "{\"type\":\"workspace-write\"}".to_string(),
            "--".to_string(),
            "worker".to_string(),
        ];

        append_windows_prepared_capability_sid(&mut args, "S-1-5-21-1-2-3-4")
            .expect("prepared capability sid should insert");

        assert_eq!(
            args,
            vec![
                "--windows-sandbox".to_string(),
                "--sandbox-policy-cwd".to_string(),
                "C:\\workspace".to_string(),
                "--sandbox-policy".to_string(),
                "{\"type\":\"workspace-write\"}".to_string(),
                "--prepared-capability-sid".to_string(),
                "S-1-5-21-1-2-3-4".to_string(),
                "--".to_string(),
                "worker".to_string(),
            ]
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_sandbox_parse_args_accepts_prepared_capability_sid() {
        let args = vec![
            OsString::from("--windows-sandbox"),
            OsString::from("--sandbox-policy-cwd"),
            OsString::from("C:\\workspace"),
            OsString::from("--sandbox-policy"),
            OsString::from("{\"type\":\"workspace-write\"}"),
            OsString::from("--prepared-capability-sid"),
            OsString::from("S-1-5-21-1-2-3-4"),
            OsString::from("--"),
            OsString::from("worker"),
        ];

        let parsed = windows_sandbox_parse_args_from(args).expect("windows sandbox args");

        assert_eq!(
            parsed.prepared_capability_sid.as_deref(),
            Some("S-1-5-21-1-2-3-4")
        );
        assert_eq!(parsed.command, vec!["worker".to_string()]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn prepare_worker_command_bwrap_env_does_not_override_explicit_false() {
        let previous_env = std::env::var_os(LINUX_BWRAP_ENABLED_ENV);
        unsafe {
            std::env::set_var(LINUX_BWRAP_ENABLED_ENV, "1");
        }

        let mut state = SandboxState::default();
        state.sandbox_policy = SandboxPolicy::WorkspaceWrite {
            writable_roots: Vec::new(),
            network_access: false,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
        };
        state.use_linux_sandbox_bwrap = false;

        let prepared =
            prepare_worker_command(Path::new("/bin/echo"), vec!["ok".to_string()], &state)
                .expect("prepare_worker_command should succeed");

        match previous_env {
            Some(value) => unsafe {
                std::env::set_var(LINUX_BWRAP_ENABLED_ENV, value);
            },
            None => unsafe {
                std::env::remove_var(LINUX_BWRAP_ENABLED_ENV);
            },
        }

        assert!(
            !prepared.args.contains(&"--use-bwrap-sandbox".to_string()),
            "explicit false override should disable bwrap even when env enables it"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sandbox_state_defaults_with_environment_respects_linux_bwrap_env() {
        let previous_env = std::env::var_os(LINUX_BWRAP_ENABLED_ENV);
        unsafe {
            std::env::set_var(LINUX_BWRAP_ENABLED_ENV, "1");
        }
        let defaults = sandbox_state_defaults_with_environment();
        match previous_env {
            Some(value) => unsafe {
                std::env::set_var(LINUX_BWRAP_ENABLED_ENV, value);
            },
            None => unsafe {
                std::env::remove_var(LINUX_BWRAP_ENABLED_ENV);
            },
        }
        assert!(
            defaults.use_linux_sandbox_bwrap,
            "Linux bwrap env should be applied at defaults layer"
        );
    }
}
