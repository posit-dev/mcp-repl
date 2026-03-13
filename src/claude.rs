use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::backend::Backend;
use crate::worker_process::{WorkerError, WorkerManager};

pub const CLAUDE_SESSION_ID_ENV: &str = "MCP_REPL_CLAUDE_SESSION_ID";
pub const CLAUDE_ENV_FILE_ENV: &str = "CLAUDE_ENV_FILE";
pub const CLAUDE_PROJECT_DIR_ENV: &str = "CLAUDE_PROJECT_DIR";

const CONTROL_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const STATE_SUBDIR: &str = "mcp-repl/claude-clear";
static NEXT_TMP_FILE_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookCommand {
    SessionStart,
    SessionEnd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HandoffSource {
    EnvVar,
    EnvFile,
    ProjectState,
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
    control_path: PathBuf,
    env_file_path: Mutex<Option<PathBuf>>,
    current_session_id: Mutex<String>,
    previous_session_id: Mutex<Option<String>>,
    last_control_seq: Mutex<u64>,
    record_template: InstanceRecordTemplate,
}

#[derive(Debug, Clone)]
struct InstanceRecordTemplate {
    backend: String,
    pid: u32,
    cwd: Option<String>,
    started_unix_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InstanceRecord {
    claude_session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_claude_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    env_file_path: Option<String>,
    backend: String,
    pid: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    control_path: String,
    started_unix_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ControlRequest {
    seq: u64,
    op: String,
    requested_unix_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProjectSessionRecord {
    claude_session_id: String,
    project_dir: String,
    active: bool,
    updated_unix_ms: u128,
}

impl ClaudeClearBinding {
    pub fn maybe_register(backend: Backend) -> Result<Option<Self>, WorkerError> {
        let project_session_dir = current_project_session_dir();
        let env_file_path = env::var_os(CLAUDE_ENV_FILE_ENV).map(PathBuf::from);
        let Some(session_id) = current_claude_session_id_from_sources(
            project_session_dir.as_deref(),
            env_file_path.as_deref(),
        ) else {
            return Ok(None);
        };

        let state_root = claude_clear_state_dir().map_err(WorkerError::Io)?;
        let instances_dir = state_root.join("instances");
        let controls_dir = state_root.join("controls");
        fs::create_dir_all(&instances_dir)?;
        fs::create_dir_all(&controls_dir)?;

        let pid = std::process::id();
        let started_unix_ms = unix_ms_now();
        let backend_label = match backend {
            Backend::R => "r",
            Backend::Python => "python",
        };
        let instance_id = unique_instance_id(&instances_dir, backend_label, pid, started_unix_ms)
            .map_err(WorkerError::Io)?;
        let record_path = instances_dir.join(format!("{instance_id}.json"));
        let control_path = controls_dir.join(format!("{instance_id}.json"));

        let binding = Self {
            inner: Arc::new(ClaudeClearBindingInner {
                record_path,
                control_path,
                env_file_path: Mutex::new(env_file_path),
                current_session_id: Mutex::new(session_id.clone()),
                previous_session_id: Mutex::new(None),
                last_control_seq: Mutex::new(0),
                record_template: InstanceRecordTemplate {
                    backend: backend_label.to_string(),
                    pid,
                    cwd: env::current_dir()
                        .ok()
                        .map(|path| path.to_string_lossy().to_string()),
                    started_unix_ms,
                },
            }),
        };
        write_control_request(
            &binding.inner.control_path,
            &ControlRequest {
                seq: 0,
                op: "restart".to_string(),
                requested_unix_ms: started_unix_ms,
            },
        )
        .map_err(WorkerError::Io)?;
        binding
            .write_record(&session_id, None)
            .map_err(WorkerError::Io)?;
        Ok(Some(binding))
    }

    pub fn sync(&self, worker: &mut WorkerManager) -> Result<(), WorkerError> {
        let mut restart_observed = false;
        let next_seq = read_control_request(&self.inner.control_path)
            .map(|request| request.seq)
            .unwrap_or(0);
        let should_restart = {
            let last_seq = self
                .inner
                .last_control_seq
                .lock()
                .expect("claude control seq mutex poisoned");
            next_seq > *last_seq
        };
        if should_restart {
            let _ = worker.restart(CONTROL_REQUEST_TIMEOUT)?;
            let mut last_seq = self
                .inner
                .last_control_seq
                .lock()
                .expect("claude control seq mutex poisoned");
            if next_seq > *last_seq {
                *last_seq = next_seq;
            }
            restart_observed = true;
        }

        if let Some((session_id, recorded_previous_session_id)) =
            self.resolve_current_session_state()
        {
            let mut current = self
                .inner
                .current_session_id
                .lock()
                .expect("claude session id mutex poisoned");
            let mut previous = self
                .inner
                .previous_session_id
                .lock()
                .expect("claude previous session id mutex poisoned");
            if *current != session_id {
                let previous_session_id = if restart_observed {
                    None
                } else {
                    recorded_previous_session_id.or_else(|| {
                        // `/clear` can deliver SessionStart for the new Claude session before the
                        // old SessionEnd hook has scanned instance records. Keep the old session id
                        // in the record during that handoff so the clear-triggered restart still
                        // finds us.
                        Some(current.clone())
                    })
                };
                self.write_record(&session_id, previous_session_id.as_deref())
                    .map_err(WorkerError::Io)?;
                *previous = previous_session_id;
                *current = session_id;
            } else if restart_observed && previous.take().is_some() {
                self.write_record(&session_id, None)
                    .map_err(WorkerError::Io)?;
            }
        }
        Ok(())
    }

    fn resolve_current_session_state(&self) -> Option<(String, Option<String>)> {
        if let Some(record) = read_instance_record(&self.inner.record_path) {
            let mut env_file_path = self
                .inner
                .env_file_path
                .lock()
                .expect("claude env file path mutex poisoned");
            *env_file_path = record.env_file_path.as_deref().map(PathBuf::from);
            return Some((record.claude_session_id, record.previous_claude_session_id));
        }
        let session_id = self
            .inner
            .current_session_id
            .lock()
            .expect("claude session id mutex poisoned")
            .clone();
        let previous_session_id = self
            .inner
            .previous_session_id
            .lock()
            .expect("claude previous session id mutex poisoned")
            .clone();
        Some((session_id, previous_session_id))
    }

    fn write_record(&self, session_id: &str, previous_session_id: Option<&str>) -> io::Result<()> {
        let env_file_path = self
            .inner
            .env_file_path
            .lock()
            .expect("claude env file path mutex poisoned")
            .clone();
        let record = InstanceRecord {
            claude_session_id: session_id.to_string(),
            previous_claude_session_id: previous_session_id.map(str::to_string),
            env_file_path: env_file_path
                .as_ref()
                .map(|path| path.to_string_lossy().to_string()),
            backend: self.inner.record_template.backend.clone(),
            pid: self.inner.record_template.pid,
            cwd: self.inner.record_template.cwd.clone(),
            control_path: self.inner.control_path.to_string_lossy().to_string(),
            started_unix_ms: self.inner.record_template.started_unix_ms,
        };
        write_json_atomic(&self.inner.record_path, &record)
    }
}

impl Drop for ClaudeClearBindingInner {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.record_path);
        let _ = fs::remove_file(&self.control_path);
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
    let project_session_state = current_project_session_state();
    let env_file_path = env::var_os(CLAUDE_ENV_FILE_ENV).map(PathBuf::from);
    if let Some((previous_session_id, source)) = previous_claude_session_id_for_handoff(
        project_session_state
            .as_ref()
            .map(|(_, session_dir)| session_dir.as_path()),
        env_file_path.as_deref(),
        session_id,
    ) {
        rebind_instance_records_for_session(
            &previous_session_id,
            session_id,
            env_file_path.as_deref(),
            project_session_state
                .as_ref()
                .map(|(_, session_dir)| session_dir.as_path()),
            source,
        )?;
    }
    if let Some((project_dir, _)) = project_session_state {
        write_project_session_record(&project_dir, session_id, true)?;
    }
    if let Some(path) = env_file_path {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let needs_separator = env_file_needs_separator(&path)?;
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        if needs_separator {
            writeln!(file)?;
        }
        writeln!(file, "export {CLAUDE_SESSION_ID_ENV}={}", session_id)?;
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
    if let Some(project_dir) = current_project_dir() {
        write_project_session_record(&project_dir, session_id, false)?;
    }
    if input.reason.as_deref() != Some("clear") {
        // Keep inactive per-session project state so the next SessionStart can rebind any idle
        // server records before a `/clear` arrives in the new Claude session.
        return Ok(());
    }

    for record in load_instance_records_for_session(session_id)? {
        let path = PathBuf::from(record.control_path);
        request_restart(&path)?;
    }
    Ok(())
}

fn current_claude_session_id_from_sources(
    project_session_dir: Option<&Path>,
    env_file_path: Option<&Path>,
) -> Option<String> {
    read_current_session_id_from_project_state(project_session_dir)
        .or_else(|| read_session_id_from_env_file(env_file_path))
        .or_else(|| env::var(CLAUDE_SESSION_ID_ENV).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn previous_claude_session_id_for_handoff(
    project_session_dir: Option<&Path>,
    env_file_path: Option<&Path>,
    current_session_id: &str,
) -> Option<(String, HandoffSource)> {
    if env_file_path.is_some()
        && let Ok(value) = env::var(CLAUDE_SESSION_ID_ENV)
    {
        let value = value.trim().to_string();
        if !value.is_empty() && value != current_session_id {
            return Some((value, HandoffSource::EnvVar));
        }
    }
    if let Some(value) = read_session_id_from_env_file(env_file_path) {
        let value = value.trim().to_string();
        if !value.is_empty() && value != current_session_id {
            return Some((value, HandoffSource::EnvFile));
        }
    }
    read_latest_inactive_session_id_from_project_state(project_session_dir)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty() && value != current_session_id)
        .map(|value| (value, HandoffSource::ProjectState))
}

fn current_project_session_state() -> Option<(PathBuf, PathBuf)> {
    let project_dir = current_project_dir()?;
    let session_dir = project_session_dir_for_dir(&project_dir).ok()?;
    Some((project_dir, session_dir))
}

fn current_project_session_dir() -> Option<PathBuf> {
    current_project_session_state().map(|(_, path)| path)
}

fn current_project_dir() -> Option<PathBuf> {
    env::var_os(CLAUDE_PROJECT_DIR_ENV)
        .filter(|raw| !raw.is_empty())
        .map(PathBuf::from)
}

fn project_session_dir_for_dir(project_dir: &Path) -> io::Result<PathBuf> {
    let sessions_dir = claude_clear_state_dir()?.join("sessions");
    Ok(sessions_dir.join(stable_project_key(project_dir)))
}

fn stable_project_key(project_dir: &Path) -> String {
    let project_dir = project_dir.to_string_lossy();
    let mut hash = 0xcbf29ce484222325u64;
    for byte in project_dir.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let stem = Path::new(project_dir.as_ref())
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("project");
    let stem: String = stem
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect();
    format!("{stem}-{hash:016x}")
}

fn stable_session_key(session_id: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in session_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("session-{hash:016x}")
}

fn project_session_path_for_session(project_dir: &Path, session_id: &str) -> io::Result<PathBuf> {
    let session_dir = project_session_dir_for_dir(project_dir)?;
    Ok(session_dir.join(format!("{}.json", stable_session_key(session_id))))
}

fn write_project_session_record(
    project_dir: &Path,
    session_id: &str,
    active: bool,
) -> io::Result<()> {
    let path = project_session_path_for_session(project_dir, session_id)?;
    write_json_atomic(
        &path,
        &ProjectSessionRecord {
            claude_session_id: session_id.to_string(),
            project_dir: project_dir.to_string_lossy().to_string(),
            active,
            updated_unix_ms: unix_ms_now(),
        },
    )
}

fn load_project_session_records(path: Option<&Path>) -> Vec<ProjectSessionRecord> {
    let Some(path) = path else {
        return Vec::new();
    };
    if !path.is_dir() {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(path) else {
        return Vec::new();
    };

    let mut records = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(record) = serde_json::from_str::<ProjectSessionRecord>(&raw) else {
            continue;
        };
        if record.claude_session_id.trim().is_empty() {
            continue;
        }
        records.push(record);
    }
    records
}

fn read_current_session_id_from_project_state(path: Option<&Path>) -> Option<String> {
    load_project_session_records(path)
        .into_iter()
        .filter(|record| record.active)
        .max_by_key(|record| record.updated_unix_ms)
        .map(|record| record.claude_session_id)
}

fn read_latest_inactive_session_id_from_project_state(path: Option<&Path>) -> Option<String> {
    load_project_session_records(path)
        .into_iter()
        .filter(|record| !record.active)
        .max_by_key(|record| record.updated_unix_ms)
        .map(|record| record.claude_session_id)
}

fn project_session_is_active(path: Option<&Path>, session_id: &str) -> bool {
    load_project_session_records(path)
        .into_iter()
        .any(|record| record.active && record.claude_session_id == session_id)
}

fn read_session_id_from_env_file(path: Option<&Path>) -> Option<String> {
    let path = path?;
    let raw = fs::read_to_string(path).ok()?;
    for line in raw.lines().rev() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != CLAUDE_SESSION_ID_ENV {
            continue;
        }
        let value = value.trim().trim_matches('"').trim_matches('\'');
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

fn request_restart(path: &Path) -> io::Result<()> {
    let next_seq = read_control_request(path)
        .map(|request| request.seq.saturating_add(1))
        .unwrap_or(1);
    write_control_request(
        path,
        &ControlRequest {
            seq: next_seq,
            op: "restart".to_string(),
            requested_unix_ms: unix_ms_now(),
        },
    )
}

fn read_instance_record(path: &Path) -> Option<InstanceRecord> {
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn read_control_request(path: &Path) -> Option<ControlRequest> {
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_control_request(path: &Path, request: &ControlRequest) -> io::Result<()> {
    write_json_atomic(path, request)
}

fn env_file_needs_separator(path: &Path) -> io::Result<bool> {
    let Ok(raw) = fs::read(path) else {
        return Ok(false);
    };
    Ok(!raw.is_empty() && !raw.ends_with(b"\n"))
}

fn load_instance_records_for_session(session_id: &str) -> io::Result<Vec<InstanceRecord>> {
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
        if record.claude_session_id == session_id
            || record.previous_claude_session_id.as_deref() == Some(session_id)
        {
            out.push(record);
        }
    }
    Ok(out)
}

fn rebind_instance_records_for_session(
    previous_session_id: &str,
    session_id: &str,
    env_file_path: Option<&Path>,
    project_session_dir: Option<&Path>,
    source: HandoffSource,
) -> io::Result<()> {
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
        let Ok(mut record) = serde_json::from_str::<InstanceRecord>(&raw) else {
            continue;
        };
        if record.claude_session_id != previous_session_id
            && record.previous_claude_session_id.as_deref() != Some(previous_session_id)
        {
            continue;
        }
        if project_session_is_active(project_session_dir, previous_session_id) {
            continue;
        }
        match source {
            HandoffSource::ProjectState => {}
            HandoffSource::EnvVar | HandoffSource::EnvFile => {
                let Some(current_env_file_path) = env_file_path else {
                    continue;
                };
                if record.env_file_path.as_deref().map(Path::new) == Some(current_env_file_path) {
                    continue;
                }
            }
        }
        record.claude_session_id = session_id.to_string();
        record.previous_claude_session_id = Some(previous_session_id.to_string());
        record.env_file_path = env_file_path.map(|path| path.to_string_lossy().to_string());
        write_json_atomic(&path, &record)?;
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox_cli::{SandboxCliOperation, SandboxCliPlan, SandboxModeArg};
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn session_start_hook_appends_session_id_to_claude_env_file() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let temp = tempfile::tempdir().expect("tempdir");
        let env_file = temp.path().join("claude.env");
        let project_dir = temp.path().join("project");
        fs::create_dir_all(&project_dir).expect("create project dir");
        unsafe {
            env::set_var("XDG_STATE_HOME", temp.path());
            env::set_var(CLAUDE_PROJECT_DIR_ENV, &project_dir);
            env::set_var(CLAUDE_ENV_FILE_ENV, &env_file);
        }

        let input = HookInput {
            hook_event_name: Some("SessionStart".to_string()),
            session_id: "sess-start".to_string(),
            reason: None,
        };
        handle_session_start(&input).expect("handle session start");

        let raw = fs::read_to_string(&env_file).expect("read env file");
        assert!(raw.contains("export MCP_REPL_CLAUDE_SESSION_ID=sess-start"));

        unsafe {
            env::remove_var("XDG_STATE_HOME");
            env::remove_var(CLAUDE_PROJECT_DIR_ENV);
            env::remove_var(CLAUDE_ENV_FILE_ENV);
        }
    }

    #[test]
    fn session_end_hook_queues_restart_for_matching_records() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let temp = tempfile::tempdir().expect("tempdir");
        unsafe {
            env::set_var("XDG_STATE_HOME", temp.path());
        }
        let state_root = claude_clear_state_dir().expect("state root");
        let instances_dir = state_root.join("instances");
        let controls_dir = state_root.join("controls");
        fs::create_dir_all(&instances_dir).expect("create instances dir");
        fs::create_dir_all(&controls_dir).expect("create controls dir");

        let control_path = controls_dir.join("r-1.json");
        write_control_request(
            &control_path,
            &ControlRequest {
                seq: 0,
                op: "restart".to_string(),
                requested_unix_ms: 1,
            },
        )
        .expect("seed control request");
        write_json_atomic(
            &instances_dir.join("r-1.json"),
            &InstanceRecord {
                claude_session_id: "sess-old".to_string(),
                previous_claude_session_id: None,
                env_file_path: None,
                backend: "r".to_string(),
                pid: 1,
                cwd: None,
                control_path: control_path.to_string_lossy().to_string(),
                started_unix_ms: 1,
            },
        )
        .expect("write instance record");

        handle_session_end(&HookInput {
            hook_event_name: Some("SessionEnd".to_string()),
            session_id: "sess-old".to_string(),
            reason: Some("clear".to_string()),
        })
        .expect("handle session end");

        let request = read_control_request(&control_path).expect("control request");
        assert_eq!(request.seq, 1);

        unsafe {
            env::remove_var("XDG_STATE_HOME");
        }
    }

    #[test]
    fn session_end_hook_matches_previous_session_id_during_rebind() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let temp = tempfile::tempdir().expect("tempdir");
        unsafe {
            env::set_var("XDG_STATE_HOME", temp.path());
        }
        let state_root = claude_clear_state_dir().expect("state root");
        let instances_dir = state_root.join("instances");
        let controls_dir = state_root.join("controls");
        fs::create_dir_all(&instances_dir).expect("create instances dir");
        fs::create_dir_all(&controls_dir).expect("create controls dir");

        let control_path = controls_dir.join("r-1.json");
        write_control_request(
            &control_path,
            &ControlRequest {
                seq: 0,
                op: "restart".to_string(),
                requested_unix_ms: 1,
            },
        )
        .expect("seed control request");
        write_json_atomic(
            &instances_dir.join("r-1.json"),
            &InstanceRecord {
                claude_session_id: "sess-new".to_string(),
                previous_claude_session_id: Some("sess-old".to_string()),
                env_file_path: None,
                backend: "r".to_string(),
                pid: 1,
                cwd: None,
                control_path: control_path.to_string_lossy().to_string(),
                started_unix_ms: 1,
            },
        )
        .expect("write instance record");

        handle_session_end(&HookInput {
            hook_event_name: Some("SessionEnd".to_string()),
            session_id: "sess-old".to_string(),
            reason: Some("clear".to_string()),
        })
        .expect("handle session end");

        let request = read_control_request(&control_path).expect("control request");
        assert_eq!(request.seq, 1);

        unsafe {
            env::remove_var("XDG_STATE_HOME");
        }
    }

    #[test]
    fn current_claude_session_id_prefers_env_file_value() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let temp = tempfile::tempdir().expect("tempdir");
        let env_file = temp.path().join("claude.env");
        fs::write(
            &env_file,
            "MCP_REPL_CLAUDE_SESSION_ID=sess-from-file\nMCP_REPL_CLAUDE_SESSION_ID=sess-latest\n",
        )
        .expect("write env file");

        unsafe {
            env::set_var(CLAUDE_SESSION_ID_ENV, "sess-from-env");
        }

        let session_id = current_claude_session_id_from_sources(None, Some(&env_file))
            .expect("current claude session id");
        assert_eq!(session_id, "sess-latest");

        unsafe {
            env::remove_var(CLAUDE_SESSION_ID_ENV);
        }
    }

    #[test]
    fn current_claude_session_id_prefers_project_state_over_stale_env() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let temp = tempfile::tempdir().expect("tempdir");
        let project_dir = temp.path().join("project");
        fs::create_dir_all(&project_dir).expect("create project dir");

        unsafe {
            env::set_var("XDG_STATE_HOME", temp.path());
            env::set_var(CLAUDE_PROJECT_DIR_ENV, &project_dir);
            env::set_var(CLAUDE_SESSION_ID_ENV, "sess-stale");
            env::remove_var(CLAUDE_ENV_FILE_ENV);
        }

        handle_session_start(&HookInput {
            hook_event_name: Some("SessionStart".to_string()),
            session_id: "sess-current".to_string(),
            reason: None,
        })
        .expect("handle session start");

        let session_id =
            current_claude_session_id_from_sources(current_project_session_dir().as_deref(), None)
                .expect("current claude session id");
        assert_eq!(session_id, "sess-current");

        unsafe {
            env::remove_var("XDG_STATE_HOME");
            env::remove_var(CLAUDE_PROJECT_DIR_ENV);
            env::remove_var(CLAUDE_SESSION_ID_ENV);
        }
    }

    #[test]
    fn maybe_register_ignores_malformed_env_file_lines_after_valid_session_export() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let temp = tempfile::tempdir().expect("tempdir");
        let env_file = temp.path().join("claude.env");
        fs::write(
            &env_file,
            "export MCP_REPL_CLAUDE_SESSION_ID=sess-valid\nsource ~/.profile\n",
        )
        .expect("write env file");

        unsafe {
            env::set_var("XDG_STATE_HOME", temp.path());
            env::set_var(CLAUDE_ENV_FILE_ENV, &env_file);
            env::remove_var(CLAUDE_PROJECT_DIR_ENV);
            env::remove_var(CLAUDE_SESSION_ID_ENV);
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
    fn maybe_register_requires_claude_project_dir_for_project_state_lookup() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let temp = tempfile::tempdir().expect("tempdir");
        let project_dir = temp.path().join("project");
        fs::create_dir_all(&project_dir).expect("create project dir");

        unsafe {
            env::set_var("XDG_STATE_HOME", temp.path());
            env::remove_var(CLAUDE_PROJECT_DIR_ENV);
            env::remove_var(CLAUDE_ENV_FILE_ENV);
            env::remove_var(CLAUDE_SESSION_ID_ENV);
        }

        let previous_cwd = env::current_dir().expect("current dir");
        env::set_current_dir(&project_dir).expect("set current dir");
        let cwd = env::current_dir().expect("cwd after set");

        let session_path = project_session_path_for_session(&cwd, "sess-from-project-state")
            .expect("project session state path");
        write_json_atomic(
            &session_path,
            &ProjectSessionRecord {
                claude_session_id: "sess-from-project-state".to_string(),
                project_dir: cwd.to_string_lossy().to_string(),
                active: true,
                updated_unix_ms: 1,
            },
        )
        .expect("write project session state");

        let binding = ClaudeClearBinding::maybe_register(Backend::R).expect("maybe register");
        assert!(
            binding.is_none(),
            "expected no claude binding without CLAUDE_PROJECT_DIR"
        );

        env::set_current_dir(previous_cwd).expect("restore current dir");
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

        unsafe {
            env::set_var("XDG_STATE_HOME", temp.path());
            env::set_var(CLAUDE_SESSION_ID_ENV, "sess-current");
            env::remove_var(CLAUDE_PROJECT_DIR_ENV);
            env::remove_var(CLAUDE_ENV_FILE_ENV);
        }

        let binding = ClaudeClearBinding::maybe_register(Backend::Python)
            .expect("maybe register")
            .expect("expected claude binding");
        request_restart(&binding.inner.control_path).expect("queue restart request");

        let inherit_plan = SandboxCliPlan {
            operations: vec![SandboxCliOperation::SetMode(SandboxModeArg::Inherit)],
        };
        let mut failing_worker =
            WorkerManager::new(Backend::Python, inherit_plan).expect("worker manager");
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
            env::remove_var(CLAUDE_SESSION_ID_ENV);
        }
    }

    #[cfg(windows)]
    #[test]
    fn write_json_atomic_replaces_existing_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("state.json");
        write_json_atomic(
            &path,
            &ControlRequest {
                seq: 0,
                op: "restart".to_string(),
                requested_unix_ms: 1,
            },
        )
        .expect("write initial state");
        write_json_atomic(
            &path,
            &ControlRequest {
                seq: 1,
                op: "restart".to_string(),
                requested_unix_ms: 2,
            },
        )
        .expect("replace existing state");

        let request = read_control_request(&path).expect("read control request");
        assert_eq!(request.seq, 1);
        assert_eq!(request.requested_unix_ms, 2);
    }
}
