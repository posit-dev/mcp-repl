use std::collections::HashSet;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessesToUpdate, System};

use crate::backend::Backend;
use crate::worker_process::{WorkerError, WorkerManager};

pub const CLAUDE_SESSION_ID_ENV: &str = "MCP_REPL_CLAUDE_SESSION_ID";
pub const CLAUDE_ENV_FILE_ENV: &str = "CLAUDE_ENV_FILE";

pub const CONTROL_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const STATE_SUBDIR: &str = "mcp-repl/claude-clear";
const CLAUDE_SESSION_ID_TOKEN_PREFIX: &str = "mcp_repl_session_id_b64_";
const CLAUDE_PROJECT_DIR_ENV: &str = "CLAUDE_PROJECT_DIR";
const CLAUDE_TEST_SCOPE_KEY_ENV: &str = "MCP_REPL_CLAUDE_TEST_SCOPE_KEY";
const CLAUDE_SCOPE_KEY_VERSION: &[u8] = b"mcp-repl-claude-scope-v1";
static NEXT_TMP_FILE_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookCommand {
    SessionStart,
    SessionEnd,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeClientContext {
    scope_pid: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeBindingRefresh {
    Unchanged,
    Rebound,
    MissingCurrentSession,
}

impl HookCommand {
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "session-start" => Ok(Self::SessionStart),
            "session-end" => Ok(Self::SessionEnd),
            _ => Err(format!(
                "invalid claude-hook command: {raw} (expected session-start|session-end)"
            )),
        }
    }
}

#[derive(Debug, Deserialize)]
struct HookInput {
    #[serde(default)]
    hook_event_name: Option<String>,
    session_id: String,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ClaudeClearBinding {
    inner: Arc<ClaudeClearBindingInner>,
}

#[derive(Debug)]
struct ClaudeClearBindingInner {
    record_path: PathBuf,
    binding_session: Mutex<ClaudeSessionBinding>,
    record_template: InstanceRecordTemplate,
    last_control_seq: Mutex<u64>,
}

#[derive(Debug, Clone)]
struct InstanceRecordTemplate {
    backend: String,
    pid: u32,
    cwd: Option<String>,
    started_unix_ms: u128,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ClaudeSessionBinding {
    identity: ClaudeSessionIdentity,
    session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ClaudeSessionIdentity {
    ScopeKey(String),
    EnvFile(PathBuf),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaudeSessionState {
    session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    claude_pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    updated_unix_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InstanceRecord {
    claude_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scope_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    env_file_path: Option<String>,
    backend: String,
    pid: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    started_unix_ms: u128,
    control_seq: u64,
}

#[derive(Debug, Clone)]
struct LoadedInstanceRecord {
    path: PathBuf,
    record: InstanceRecord,
}

impl ClaudeClearBinding {
    #[cfg(test)]
    pub fn maybe_register(backend: Backend) -> Result<Option<Self>, WorkerError> {
        let Some(context) = detect_client_context() else {
            return Ok(None);
        };
        Self::maybe_register_with_initial_seq(backend, 0, &context)
    }

    pub fn maybe_register_late(
        backend: Backend,
        context: &ClaudeClientContext,
    ) -> Result<Option<Self>, WorkerError> {
        Self::maybe_register_with_initial_seq(backend, 1, context)
    }

    fn maybe_register_with_initial_seq(
        backend: Backend,
        initial_control_seq: u64,
        context: &ClaudeClientContext,
    ) -> Result<Option<Self>, WorkerError> {
        let Some(binding_session) = current_claude_session_binding(context) else {
            return Ok(None);
        };

        let state_root = claude_clear_state_dir().map_err(WorkerError::Io)?;
        let instances_dir = state_root.join("instances");
        fs::create_dir_all(&instances_dir)?;

        let pid = std::process::id();
        let started_unix_ms = unix_ms_now();
        let backend_label = match backend {
            Backend::R => "r",
            Backend::Python => "python",
        };
        let record_template = InstanceRecordTemplate {
            backend: backend_label.to_string(),
            pid,
            cwd: env::current_dir()
                .ok()
                .map(|path| path.to_string_lossy().to_string()),
            started_unix_ms,
        };
        let instance_id = unique_instance_id(&instances_dir, backend_label, pid, started_unix_ms)
            .map_err(WorkerError::Io)?;
        let record_path = instances_dir.join(format!("{instance_id}.json"));

        let binding = Self {
            inner: Arc::new(ClaudeClearBindingInner {
                record_path,
                binding_session: Mutex::new(binding_session.clone()),
                record_template,
                last_control_seq: Mutex::new(0),
            }),
        };
        binding
            .write_initial_record(
                &binding_session,
                &binding.inner.record_template,
                initial_control_seq,
            )
            .map_err(WorkerError::Io)?;
        Ok(Some(binding))
    }

    pub fn refresh(
        &self,
        context: &ClaudeClientContext,
    ) -> Result<ClaudeBindingRefresh, WorkerError> {
        let Some(binding_session) = current_claude_session_binding(context) else {
            return Ok(ClaudeBindingRefresh::MissingCurrentSession);
        };
        let mut current_binding = self
            .inner
            .binding_session
            .lock()
            .expect("claude binding session mutex poisoned");
        if *current_binding == binding_session {
            return Ok(ClaudeBindingRefresh::Unchanged);
        }

        self.rebind_and_request_restart(&binding_session, &self.inner.record_template)
            .map_err(WorkerError::Io)?;
        *current_binding = binding_session;
        Ok(ClaudeBindingRefresh::Rebound)
    }

    pub fn sync(&self, worker: &mut WorkerManager) -> Result<(), WorkerError> {
        let next_seq = read_full_instance_record(&self.inner.record_path)
            .map(|record| record.control_seq)
            .unwrap_or(0);
        let should_restart = {
            let last_seq = self
                .inner
                .last_control_seq
                .lock()
                .expect("claude control seq mutex poisoned");
            next_seq > *last_seq
        };
        if !should_restart {
            return Ok(());
        }

        let _ = worker.restart(CONTROL_REQUEST_TIMEOUT)?;
        let mut last_seq = self
            .inner
            .last_control_seq
            .lock()
            .expect("claude control seq mutex poisoned");
        if next_seq > *last_seq {
            *last_seq = next_seq;
        }
        Ok(())
    }

    fn write_record(
        &self,
        binding_session: &ClaudeSessionBinding,
        template: &InstanceRecordTemplate,
        control_seq: u64,
    ) -> io::Result<()> {
        write_json_atomic(
            &self.inner.record_path,
            &instance_record(binding_session, template, control_seq),
        )
    }

    fn write_initial_record(
        &self,
        binding_session: &ClaudeSessionBinding,
        template: &InstanceRecordTemplate,
        control_seq: u64,
    ) -> io::Result<()> {
        self.write_record(binding_session, template, control_seq)
    }

    fn rebind_and_request_restart(
        &self,
        binding_session: &ClaudeSessionBinding,
        template: &InstanceRecordTemplate,
    ) -> io::Result<()> {
        update_instance_record(&self.inner.record_path, |record| {
            let next_control_seq = record.control_seq.saturating_add(1);
            *record = instance_record(binding_session, template, next_control_seq);
            Ok(())
        })
    }
}

pub fn detect_client_context() -> Option<ClaudeClientContext> {
    if test_scope_key_env().is_some() || current_claude_env_file_path().is_some() {
        return Some(ClaudeClientContext { scope_pid: None });
    }
    current_claude_process_pid().map(|scope_pid| ClaudeClientContext {
        scope_pid: Some(scope_pid),
    })
}

pub fn prune_stale_state() -> io::Result<()> {
    prune_stale_claude_state()
}

impl Drop for ClaudeClearBindingInner {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.record_path);
    }
}

pub fn run_hook(command: HookCommand) -> Result<(), Box<dyn std::error::Error>> {
    let mut raw = String::new();
    io::stdin().read_to_string(&mut raw)?;
    let input: HookInput = serde_json::from_str(&raw)?;
    match command {
        HookCommand::SessionStart => handle_session_start(&input)?,
        HookCommand::SessionEnd => handle_session_end(&input)?,
    }
    Ok(())
}

fn handle_session_start(input: &HookInput) -> Result<(), Box<dyn std::error::Error>> {
    let session_id = input.session_id.trim();
    if session_id.is_empty() {
        return Ok(());
    }
    if input.hook_event_name.as_deref() != Some("SessionStart") {
        return Ok(());
    }
    prune_stale_claude_state()?;
    if let Some(scope_key) = current_claude_scope_key(None) {
        write_session_state(&scope_key, session_id)?;
    }
    if let Some(path) = current_claude_env_file_path() {
        append_session_id_export(&path, session_id)?;
    }
    Ok(())
}

fn handle_session_end(input: &HookInput) -> Result<(), Box<dyn std::error::Error>> {
    let session_id = input.session_id.trim();
    if session_id.is_empty() {
        return Ok(());
    }
    if input.hook_event_name.as_deref() != Some("SessionEnd") {
        return Ok(());
    }
    if input.reason.as_deref() != Some("clear") {
        return Ok(());
    }
    prune_stale_claude_state()?;

    let mut restart_paths = HashSet::new();
    if let Some(scope_key) = current_claude_scope_key(None) {
        for record in load_instance_records_for_scope(&scope_key, session_id)? {
            restart_paths.insert(record.path);
        }
    }

    if let Some(env_file_path) = current_claude_env_file_path() {
        for record in load_instance_records_for_env_file(&env_file_path, session_id)? {
            restart_paths.insert(record.path);
        }
    }

    for record_path in restart_paths {
        request_restart(&record_path)?;
    }
    Ok(())
}

fn current_claude_session_binding(context: &ClaudeClientContext) -> Option<ClaudeSessionBinding> {
    if let Some(scope_key) = current_claude_scope_key(context.scope_pid)
        && let Some(state) = read_session_state(&scope_key)
    {
        let session_id = state.session_id.trim().to_string();
        if !session_id.is_empty() {
            return Some(ClaudeSessionBinding {
                identity: ClaudeSessionIdentity::ScopeKey(scope_key),
                session_id,
            });
        }
    }

    let env_file_path = current_claude_env_file_path()?;
    let session_id = read_session_id_from_env_file(Some(&env_file_path))?
        .trim()
        .to_string();
    if session_id.is_empty() {
        return None;
    }
    Some(ClaudeSessionBinding {
        identity: ClaudeSessionIdentity::EnvFile(env_file_path),
        session_id,
    })
}

fn current_claude_env_file_path() -> Option<PathBuf> {
    env::var_os(CLAUDE_ENV_FILE_ENV)
        .filter(|raw| !raw.is_empty())
        .map(PathBuf::from)
}

fn test_scope_key_env() -> Option<String> {
    if let Ok(scope_key) = env::var(CLAUDE_TEST_SCOPE_KEY_ENV)
        && !scope_key.trim().is_empty()
    {
        return Some(scope_key);
    }
    None
}

fn current_claude_scope_key(scope_pid: Option<u32>) -> Option<String> {
    if let Some(scope_key) = test_scope_key_env() {
        return Some(scope_key);
    }
    let cwd = current_claude_project_dir()?;
    let claude_pid = scope_pid.or_else(current_claude_process_pid)?;
    let mut input = Vec::new();
    input.extend_from_slice(CLAUDE_SCOPE_KEY_VERSION);
    input.push(0);
    input.extend_from_slice(cwd.to_string_lossy().as_bytes());
    input.push(0);
    input.extend_from_slice(claude_pid.to_string().as_bytes());
    Some(blake3::hash(&input).to_hex().to_string())
}

fn current_claude_project_dir() -> Option<PathBuf> {
    env::var_os(CLAUDE_PROJECT_DIR_ENV)
        .filter(|raw| !raw.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::current_dir().ok())
        .map(canonicalize_or_identity)
}

fn current_claude_process_pid() -> Option<u32> {
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::All, true);
    let mut pid = Pid::from_u32(std::process::id());
    for _ in 0..64 {
        let process = system.process(pid)?;
        let parent = process.parent()?;
        let parent_process = system.process(parent)?;
        let name = parent_process.name().to_string_lossy();
        if is_claude_process_name(&name) {
            return Some(parent.as_u32());
        }
        pid = parent;
    }
    None
}

fn is_claude_process_name(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "claude" | "claude.exe" | "claude-code" | "claude code"
    )
}

fn read_session_state(scope_key: &str) -> Option<ClaudeSessionState> {
    let path = claude_session_state_path(scope_key).ok()?;
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_session_state(scope_key: &str, session_id: &str) -> io::Result<()> {
    let state = ClaudeSessionState {
        session_id: session_id.to_string(),
        claude_pid: current_claude_process_pid(),
        cwd: current_claude_project_dir().map(|path| path.to_string_lossy().to_string()),
        updated_unix_ms: unix_ms_now(),
    };
    let path = claude_session_state_path(scope_key)?;
    write_json_atomic(&path, &state)
}

fn claude_session_state_path(scope_key: &str) -> io::Result<PathBuf> {
    Ok(claude_clear_state_dir()?
        .join("sessions")
        .join(format!("{scope_key}.json")))
}

fn read_session_id_from_env_file(path: Option<&Path>) -> Option<String> {
    let path = path?;
    let raw = fs::read_to_string(path).ok()?;
    for line in raw.lines().rev() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(line) = line.strip_prefix("export ") else {
            continue;
        };
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != CLAUDE_SESSION_ID_ENV {
            continue;
        }
        if let Some(session_id) = decode_session_id_token(value.trim()) {
            return Some(session_id);
        }
    }
    None
}

fn decode_session_id_token(value: &str) -> Option<String> {
    let encoded = value.strip_prefix(CLAUDE_SESSION_ID_TOKEN_PREFIX)?;
    let decoded = URL_SAFE_NO_PAD.decode(encoded).ok()?;
    String::from_utf8(decoded).ok()
}

fn append_session_id_export(path: &Path, session_id: &str) -> io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    let needs_separator = env_file_needs_separator(path)?;
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    if needs_separator {
        writeln!(file)?;
    }
    let encoded_session_id = URL_SAFE_NO_PAD.encode(session_id);
    writeln!(
        file,
        "export {CLAUDE_SESSION_ID_ENV}={CLAUDE_SESSION_ID_TOKEN_PREFIX}{encoded_session_id}"
    )?;
    Ok(())
}

fn request_restart(path: &Path) -> io::Result<()> {
    match update_instance_record(path, |record| {
        record.control_seq = record.control_seq.saturating_add(1);
        Ok(())
    }) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

fn env_file_needs_separator(path: &Path) -> io::Result<bool> {
    let Ok(raw) = fs::read(path) else {
        return Ok(false);
    };
    Ok(!raw.is_empty() && !raw.ends_with(b"\n"))
}

fn load_instance_records_for_scope(
    scope_key: &str,
    session_id: &str,
) -> io::Result<Vec<LoadedInstanceRecord>> {
    let mut out = Vec::new();
    for record in load_instance_records()? {
        if record.record.claude_session_id == session_id
            && record.record.scope_key.as_deref() == Some(scope_key)
        {
            out.push(record);
        }
    }
    Ok(out)
}

fn load_instance_records_for_env_file(
    env_file_path: &Path,
    session_id: &str,
) -> io::Result<Vec<LoadedInstanceRecord>> {
    let env_file_path = env_file_path.to_string_lossy().to_string();
    let mut out = Vec::new();
    for record in load_instance_records()? {
        if record.record.claude_session_id == session_id
            && record.record.env_file_path.as_deref() == Some(env_file_path.as_str())
        {
            out.push(record);
        }
    }
    Ok(out)
}

fn load_instance_records() -> io::Result<Vec<LoadedInstanceRecord>> {
    let mut out = Vec::new();
    let instances_dir = claude_clear_state_dir()?.join("instances");
    if !instances_dir.is_dir() {
        return Ok(out);
    }
    for entry in fs::read_dir(instances_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(record) = serde_json::from_str::<InstanceRecord>(&raw) else {
            continue;
        };
        out.push(LoadedInstanceRecord { path, record });
    }
    Ok(out)
}

fn prune_stale_claude_state() -> io::Result<()> {
    let live_pids = live_process_ids();
    prune_stale_claude_session_states(&live_pids)?;
    prune_stale_claude_instance_records(&live_pids)?;
    Ok(())
}

fn prune_stale_claude_session_states(live_pids: &HashSet<u32>) -> io::Result<()> {
    let sessions_dir = claude_clear_state_dir()?.join("sessions");
    if !sessions_dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(sessions_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(state) = serde_json::from_str::<ClaudeSessionState>(&raw) else {
            continue;
        };
        if state
            .claude_pid
            .is_some_and(|pid| !live_pids.contains(&pid))
        {
            let _ = fs::remove_file(path);
        }
    }
    Ok(())
}

fn prune_stale_claude_instance_records(live_pids: &HashSet<u32>) -> io::Result<()> {
    let instances_dir = claude_clear_state_dir()?.join("instances");
    if !instances_dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(instances_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(record) = serde_json::from_str::<InstanceRecord>(&raw) else {
            continue;
        };
        if live_pids.contains(&record.pid) {
            continue;
        }
        let _ = fs::remove_file(&path);
    }
    Ok(())
}

fn instance_record(
    binding_session: &ClaudeSessionBinding,
    template: &InstanceRecordTemplate,
    control_seq: u64,
) -> InstanceRecord {
    let (scope_key, env_file_path) = match &binding_session.identity {
        ClaudeSessionIdentity::ScopeKey(scope_key) => (Some(scope_key.clone()), None),
        ClaudeSessionIdentity::EnvFile(path) => (None, Some(path.to_string_lossy().to_string())),
    };
    InstanceRecord {
        claude_session_id: binding_session.session_id.clone(),
        scope_key,
        env_file_path,
        backend: template.backend.clone(),
        pid: template.pid,
        cwd: template.cwd.clone(),
        started_unix_ms: template.started_unix_ms,
        control_seq,
    }
}

fn read_full_instance_record(path: &Path) -> Option<InstanceRecord> {
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn update_instance_record(
    path: &Path,
    mut update: impl FnMut(&mut InstanceRecord) -> io::Result<()>,
) -> io::Result<()> {
    let _guard = acquire_record_lock(path)?;
    let Some(mut record) = read_full_instance_record(path) else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("instance record missing: {}", path.display()),
        ));
    };
    update(&mut record)?;
    write_json_atomic(path, &record)
}

fn acquire_record_lock(path: &Path) -> io::Result<RecordLockGuard> {
    let lock_path = record_lock_path(path);
    let deadline = Instant::now() + CONTROL_REQUEST_TIMEOUT;
    loop {
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&lock_path)
        {
            Ok(_) => return Ok(RecordLockGuard { lock_path }),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "timed out acquiring instance record lock: {}",
                            lock_path.display()
                        ),
                    ));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(err) => return Err(err),
        }
    }
}

fn record_lock_path(path: &Path) -> PathBuf {
    let stem = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("instance");
    path.with_file_name(format!(".{stem}.lock"))
}

struct RecordLockGuard {
    lock_path: PathBuf,
}

impl Drop for RecordLockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.lock_path);
    }
}

fn live_process_ids() -> HashSet<u32> {
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::All, true);
    system.processes().keys().map(|pid| pid.as_u32()).collect()
}

fn unique_instance_id(
    instances_dir: &Path,
    backend: &str,
    pid: u32,
    started_unix_ms: u128,
) -> io::Result<String> {
    for suffix in 0u32..1_000u32 {
        let base = if suffix == 0 {
            format!("{backend}-{pid}-{started_unix_ms}")
        } else {
            format!("{backend}-{pid}-{started_unix_ms}-{suffix}")
        };
        let candidate = instances_dir.join(format!("{base}.json"));
        if !candidate.exists() {
            return Ok(base);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "failed to allocate unique Claude clear instance id",
    ))
}

fn claude_clear_state_dir() -> io::Result<PathBuf> {
    if let Some(path) = env::var_os("XDG_STATE_HOME").filter(|raw| !raw.is_empty()) {
        return Ok(PathBuf::from(path).join(STATE_SUBDIR));
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(path) = env::var_os("LOCALAPPDATA").filter(|raw| !raw.is_empty()) {
            return Ok(PathBuf::from(path).join(STATE_SUBDIR));
        }
        if let Some(path) = env::var_os("APPDATA").filter(|raw| !raw.is_empty()) {
            return Ok(PathBuf::from(path).join(STATE_SUBDIR));
        }
    }
    let home = env::var_os("HOME")
        .filter(|raw| !raw.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
    Ok(home.join(".local/state").join(STATE_SUBDIR))
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let Some(parent) = path.parent() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path has no parent: {}", path.display()),
        ));
    };
    fs::create_dir_all(parent)?;
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|err| io::Error::other(format!("failed to serialize json: {err}")))?;
    let tmp_path = unique_atomic_write_tmp_path(parent, path);
    let mut tmp_file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&tmp_path)?;
    tmp_file.write_all(&bytes)?;
    drop(tmp_file);
    replace_file_atomically(&tmp_path, path)?;
    Ok(())
}

fn unique_atomic_write_tmp_path(parent: &Path, path: &Path) -> PathBuf {
    let stem = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("state");
    let id = NEXT_TMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(".{stem}.{}.{}.tmp", std::process::id(), id))
}

#[cfg(not(target_os = "windows"))]
fn replace_file_atomically(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)
}

#[cfg(target_os = "windows")]
fn replace_file_atomically(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let mut from_wide: Vec<u16> = from.as_os_str().encode_wide().collect();
    from_wide.push(0);
    let mut to_wide: Vec<u16> = to.as_os_str().encode_wide().collect();
    to_wide.push(0);
    let ok = unsafe {
        MoveFileExW(
            from_wide.as_ptr(),
            to_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn unix_ms_now() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

fn canonicalize_or_identity(path: PathBuf) -> PathBuf {
    path.canonicalize().unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox_cli::{SandboxCliOperation, SandboxCliPlan, SandboxModeArg};
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn test_tempdir(prefix: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(env::current_dir().expect("current dir"))
            .expect("tempdir")
    }

    fn session_export_line(session_id: &str) -> String {
        format!(
            "export {CLAUDE_SESSION_ID_ENV}={CLAUDE_SESSION_ID_TOKEN_PREFIX}{}\n",
            URL_SAFE_NO_PAD.encode(session_id)
        )
    }

    #[test]
    fn detect_client_context_is_none_without_claude_markers() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        unsafe {
            env::remove_var(CLAUDE_ENV_FILE_ENV);
            env::remove_var(CLAUDE_TEST_SCOPE_KEY_ENV);
            env::remove_var(CLAUDE_PROJECT_DIR_ENV);
        }

        let context = detect_client_context();
        assert!(
            context.is_none(),
            "expected no Claude client context without Claude markers, got: {context:?}"
        );
    }

    #[test]
    fn detect_client_context_uses_claude_env_file_without_parent_claude() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let temp = test_tempdir("claude-context-env-file-");
        let env_file = temp.path().join("claude.env");
        unsafe {
            env::set_var(CLAUDE_ENV_FILE_ENV, &env_file);
            env::remove_var(CLAUDE_TEST_SCOPE_KEY_ENV);
            env::remove_var(CLAUDE_PROJECT_DIR_ENV);
        }

        let context = detect_client_context();
        assert_eq!(
            context,
            Some(ClaudeClientContext { scope_pid: None }),
            "expected CLAUDE_ENV_FILE to activate Claude client context"
        );

        unsafe {
            env::remove_var(CLAUDE_ENV_FILE_ENV);
        }
    }

    #[test]
    fn session_start_hook_appends_session_id_to_claude_env_file() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let temp = test_tempdir("claude-session-start-");
        let env_file = temp.path().join("claude.env");
        unsafe {
            env::set_var(CLAUDE_ENV_FILE_ENV, &env_file);
        }

        let input = HookInput {
            hook_event_name: Some("SessionStart".to_string()),
            session_id: "sess-start".to_string(),
            reason: None,
        };
        handle_session_start(&input).expect("handle session start");

        let raw = fs::read_to_string(&env_file).expect("read env file");
        assert!(raw.contains("export MCP_REPL_CLAUDE_SESSION_ID=mcp_repl_session_id_b64_"));

        unsafe {
            env::remove_var(CLAUDE_ENV_FILE_ENV);
        }
    }

    #[test]
    fn current_claude_session_id_reads_multiline_session_with_apostrophe_from_token() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let temp = test_tempdir("claude-session-start-marker-");
        let env_file = temp.path().join("claude.env");
        unsafe {
            env::set_var(CLAUDE_ENV_FILE_ENV, &env_file);
        }

        let input = HookInput {
            hook_event_name: Some("SessionStart".to_string()),
            session_id: "foo'bar\nbaz".to_string(),
            reason: None,
        };
        handle_session_start(&input).expect("handle session start");

        let binding_session =
            current_claude_session_from_env_file_with_path(&env_file).expect("binding session");
        assert_eq!(binding_session.session_id, "foo'bar\nbaz");

        unsafe {
            env::remove_var(CLAUDE_ENV_FILE_ENV);
        }
    }

    #[test]
    fn current_claude_session_id_rejects_non_token_export() {
        let temp = test_tempdir("claude-invalid-session-export-");
        let env_file = temp.path().join("claude.env");
        fs::write(
            &env_file,
            "export MCP_REPL_CLAUDE_SESSION_ID='foo'\"'\"'bar\nbaz'\n",
        )
        .expect("write env file");

        assert!(
            current_claude_session_from_env_file_with_path(&env_file).is_none(),
            "expected non-token export to fail closed"
        );
    }

    #[test]
    fn session_end_hook_queues_restart_only_for_matching_env_file_and_session() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let temp = test_tempdir("claude-session-end-");
        let env_file_a = temp.path().join("claude-a.env");
        let env_file_b = temp.path().join("claude-b.env");
        fs::write(&env_file_a, session_export_line("sess-shared")).expect("write env file a");
        fs::write(&env_file_b, session_export_line("sess-shared")).expect("write env file b");
        unsafe {
            env::set_var("XDG_STATE_HOME", temp.path());
            env::set_var(CLAUDE_ENV_FILE_ENV, &env_file_b);
        }
        let state_root = claude_clear_state_dir().expect("state root");
        let instances_dir = state_root.join("instances");
        fs::create_dir_all(&instances_dir).expect("create instances dir");
        let live_pid = std::process::id();

        let record_a_path = instances_dir.join("r-a.json");
        let record_b_path = instances_dir.join("r-b.json");
        write_json_atomic(
            &record_a_path,
            &InstanceRecord {
                claude_session_id: "sess-shared".to_string(),
                scope_key: None,
                env_file_path: Some(env_file_a.to_string_lossy().to_string()),
                backend: "r".to_string(),
                pid: live_pid,
                cwd: None,
                started_unix_ms: 1,
                control_seq: 0,
            },
        )
        .expect("write record a");
        write_json_atomic(
            &record_b_path,
            &InstanceRecord {
                claude_session_id: "sess-shared".to_string(),
                scope_key: None,
                env_file_path: Some(env_file_b.to_string_lossy().to_string()),
                backend: "r".to_string(),
                pid: live_pid,
                cwd: None,
                started_unix_ms: 1,
                control_seq: 0,
            },
        )
        .expect("write record b");

        handle_session_end(&HookInput {
            hook_event_name: Some("SessionEnd".to_string()),
            session_id: "sess-shared".to_string(),
            reason: Some("clear".to_string()),
        })
        .expect("handle session end");

        let request_a = read_full_instance_record(&record_a_path).expect("read record a");
        let request_b = read_full_instance_record(&record_b_path).expect("read record b");
        assert_eq!(
            request_a.control_seq, 0,
            "env file mismatch should not queue restart"
        );
        assert_eq!(
            request_b.control_seq, 1,
            "exact env file + session should queue restart"
        );

        unsafe {
            env::remove_var("XDG_STATE_HOME");
            env::remove_var(CLAUDE_ENV_FILE_ENV);
        }
    }

    #[test]
    fn session_end_hook_queues_restart_for_matching_env_file_even_if_file_is_missing() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let temp = test_tempdir("claude-session-end-missing-env-file-");
        let env_file = temp.path().join("claude-missing.env");
        unsafe {
            env::set_var("XDG_STATE_HOME", temp.path());
            env::set_var(CLAUDE_ENV_FILE_ENV, &env_file);
        }
        let state_root = claude_clear_state_dir().expect("state root");
        let instances_dir = state_root.join("instances");
        fs::create_dir_all(&instances_dir).expect("create instances dir");

        let record_path = instances_dir.join("r-missing.json");
        write_json_atomic(
            &record_path,
            &InstanceRecord {
                claude_session_id: "sess-missing".to_string(),
                scope_key: None,
                env_file_path: Some(env_file.to_string_lossy().to_string()),
                backend: "r".to_string(),
                pid: std::process::id(),
                cwd: None,
                started_unix_ms: 1,
                control_seq: 0,
            },
        )
        .expect("write record");

        handle_session_end(&HookInput {
            hook_event_name: Some("SessionEnd".to_string()),
            session_id: "sess-missing".to_string(),
            reason: Some("clear".to_string()),
        })
        .expect("handle session end");

        let request = read_full_instance_record(&record_path).expect("read record");
        assert_eq!(
            request.control_seq, 1,
            "missing env file should not stop matching env-file-bound sessions"
        );

        unsafe {
            env::remove_var("XDG_STATE_HOME");
            env::remove_var(CLAUDE_ENV_FILE_ENV);
        }
    }

    #[test]
    fn current_claude_session_id_reads_last_valid_export_from_env_file() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let temp = test_tempdir("claude-current-session-");
        let env_file = temp.path().join("claude.env");
        fs::write(
            &env_file,
            format!(
                "{}{}",
                session_export_line("sess-old"),
                session_export_line("sess-latest")
            ),
        )
        .expect("write env file");

        let binding_session =
            current_claude_session_from_env_file_with_path(&env_file).expect("binding session");
        assert_eq!(binding_session.session_id, "sess-latest");
        assert!(matches!(
            binding_session.identity,
            ClaudeSessionIdentity::EnvFile(ref path) if path == &env_file
        ));
    }

    #[test]
    fn maybe_register_ignores_malformed_env_file_lines_after_valid_session_export() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let temp = test_tempdir("claude-register-");
        let env_file = temp.path().join("claude.env");
        fs::write(
            &env_file,
            format!("{}source ~/.profile\n", session_export_line("sess-valid")),
        )
        .expect("write env file");

        unsafe {
            env::set_var("XDG_STATE_HOME", temp.path());
            env::set_var(CLAUDE_ENV_FILE_ENV, &env_file);
        }

        let binding = ClaudeClearBinding::maybe_register(Backend::R).expect("maybe register");
        assert!(
            binding.is_some(),
            "expected claude binding to load session id from env file"
        );

        drop(binding);
        unsafe {
            env::remove_var("XDG_STATE_HOME");
            env::remove_var(CLAUDE_ENV_FILE_ENV);
        }
    }

    #[test]
    fn load_instance_records_ignores_legacy_records_with_unknown_fields() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let temp = test_tempdir("claude-load-records-");
        unsafe {
            env::set_var("XDG_STATE_HOME", temp.path());
        }
        let state_root = claude_clear_state_dir().expect("state root");
        let instances_dir = state_root.join("instances");
        fs::create_dir_all(&instances_dir).expect("create instances dir");
        fs::write(
            instances_dir.join("legacy.json"),
            r#"{
  "claude_session_id": "sess-legacy",
  "env_file_path": "/tmp/legacy.env",
  "backend": "r",
  "pid": 1,
  "cwd": null,
  "started_unix_ms": 1,
  "control_seq": 0,
  "previous_claude_session_id": "sess-old"
}"#,
        )
        .expect("write legacy record");

        let records = load_instance_records().expect("load instance records");
        assert!(
            records.is_empty(),
            "expected legacy records with unknown fields to be ignored, got: {records:?}"
        );

        unsafe {
            env::remove_var("XDG_STATE_HOME");
        }
    }

    #[test]
    fn sync_does_not_consume_control_seq_when_restart_fails() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let temp_root = env::current_dir().expect("current dir");
        let temp = tempfile::Builder::new()
            .prefix("claude-sync-")
            .tempdir_in(temp_root)
            .expect("tempdir");
        let env_file = temp.path().join("claude.env");
        fs::write(&env_file, session_export_line("sess-current")).expect("write env file");

        unsafe {
            env::set_var("XDG_STATE_HOME", temp.path());
            env::set_var(CLAUDE_ENV_FILE_ENV, &env_file);
        }

        let binding = ClaudeClearBinding::maybe_register(Backend::Python)
            .expect("maybe register")
            .expect("expected claude binding");
        request_restart(&binding.inner.record_path).expect("queue restart request");

        let inherit_plan = SandboxCliPlan {
            operations: vec![SandboxCliOperation::SetMode(SandboxModeArg::Inherit)],
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut failing_worker = loop {
            let worker =
                WorkerManager::new(Backend::Python, inherit_plan.clone()).expect("worker manager");
            if worker.awaiting_initial_sandbox_state_update() {
                break worker;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "expected worker to await inherited sandbox state before restart test"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        let restart_err = binding
            .sync(&mut failing_worker)
            .expect_err("restart should fail without inherited sandbox state");
        assert!(
            matches!(restart_err, WorkerError::Sandbox(_)),
            "expected sandbox restart error, got {restart_err:?}"
        );
        let last_seq = *binding
            .inner
            .last_control_seq
            .lock()
            .expect("claude control seq mutex poisoned");
        assert_eq!(last_seq, 0, "failed restart should not consume control seq");

        unsafe {
            env::remove_var("XDG_STATE_HOME");
            env::remove_var(CLAUDE_ENV_FILE_ENV);
        }
    }

    #[test]
    fn current_claude_scope_key_normalizes_equivalent_project_paths() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let temp = test_tempdir("claude-scope-key-");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");
        let canonical = workspace.canonicalize().expect("canonical workspace");
        let parent = canonical.parent().expect("workspace parent");
        let noncanonical = parent.join(".").join(
            canonical
                .file_name()
                .expect("workspace file name should exist"),
        );

        unsafe {
            env::remove_var(CLAUDE_TEST_SCOPE_KEY_ENV);
            env::set_var(CLAUDE_PROJECT_DIR_ENV, &canonical);
        }
        let canonical_key =
            current_claude_scope_key(Some(42)).expect("scope key for canonical path");

        unsafe {
            env::set_var(CLAUDE_PROJECT_DIR_ENV, &noncanonical);
        }
        let noncanonical_key =
            current_claude_scope_key(Some(42)).expect("scope key for noncanonical path");

        assert_eq!(
            canonical_key, noncanonical_key,
            "expected equivalent project paths to produce the same scope key"
        );

        unsafe {
            env::remove_var(CLAUDE_PROJECT_DIR_ENV);
        }
    }

    fn current_claude_session_from_env_file_with_path(path: &Path) -> Option<ClaudeSessionBinding> {
        let session_id = read_session_id_from_env_file(Some(path))?
            .trim()
            .to_string();
        if session_id.is_empty() {
            return None;
        }
        Some(ClaudeSessionBinding {
            identity: ClaudeSessionIdentity::EnvFile(path.to_path_buf()),
            session_id,
        })
    }

    #[cfg(windows)]
    #[test]
    fn write_json_atomic_replaces_existing_files() {
        let temp = test_tempdir("claude-write-json-");
        let path = temp.path().join("state.json");
        write_json_atomic(
            &path,
            &InstanceRecord {
                claude_session_id: "sess-a".to_string(),
                scope_key: None,
                env_file_path: Some("/tmp/env".to_string()),
                backend: "r".to_string(),
                pid: 1,
                cwd: None,
                started_unix_ms: 1,
                control_seq: 0,
            },
        )
        .expect("write initial state");
        write_json_atomic(
            &path,
            &InstanceRecord {
                claude_session_id: "sess-b".to_string(),
                scope_key: Some("scope".to_string()),
                env_file_path: None,
                backend: "python".to_string(),
                pid: 2,
                cwd: Some("/tmp".to_string()),
                started_unix_ms: 2,
                control_seq: 1,
            },
        )
        .expect("replace existing state");

        let record = read_full_instance_record(&path).expect("read instance record");
        assert_eq!(record.control_seq, 1);
        assert_eq!(record.started_unix_ms, 2);
    }
}
