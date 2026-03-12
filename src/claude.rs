use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::backend::Backend;
use crate::worker_process::{WorkerError, WorkerManager};

pub const CLAUDE_SESSION_ID_ENV: &str = "MCP_REPL_CLAUDE_SESSION_ID";
pub const CLAUDE_ENV_FILE_ENV: &str = "CLAUDE_ENV_FILE";

const CONTROL_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const STATE_SUBDIR: &str = "mcp-repl/claude-clear";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookCommand {
    SessionStart,
    SessionEnd,
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
    env_file_path: Option<PathBuf>,
    current_session_id: Mutex<String>,
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

impl ClaudeClearBinding {
    pub fn maybe_register(backend: Backend) -> Result<Option<Self>, WorkerError> {
        let Some(session_id) = current_claude_session_id() else {
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
        let env_file_path = env::var_os(CLAUDE_ENV_FILE_ENV).map(PathBuf::from);

        let binding = Self {
            inner: Arc::new(ClaudeClearBindingInner {
                record_path,
                control_path,
                env_file_path,
                current_session_id: Mutex::new(session_id.clone()),
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
        binding.write_record(&session_id).map_err(WorkerError::Io)?;
        Ok(Some(binding))
    }

    pub fn sync(&self, worker: &mut WorkerManager) -> Result<(), WorkerError> {
        if let Some(session_id) = self.resolve_current_session_id() {
            let mut current = self
                .inner
                .current_session_id
                .lock()
                .expect("claude session id mutex poisoned");
            if *current != session_id {
                self.write_record(&session_id).map_err(WorkerError::Io)?;
                *current = session_id;
            }
        }

        let next_seq = read_control_request(&self.inner.control_path)
            .map(|request| request.seq)
            .unwrap_or(0);
        let mut last_seq = self
            .inner
            .last_control_seq
            .lock()
            .expect("claude control seq mutex poisoned");
        if next_seq > *last_seq {
            *last_seq = next_seq;
            let _ = worker.restart(CONTROL_REQUEST_TIMEOUT)?;
        }
        Ok(())
    }

    fn resolve_current_session_id(&self) -> Option<String> {
        current_claude_session_id_from_sources(self.inner.env_file_path.as_deref())
    }

    fn write_record(&self, session_id: &str) -> io::Result<()> {
        let record = InstanceRecord {
            claude_session_id: session_id.to_string(),
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
    if input.session_id.trim().is_empty() {
        return Ok(());
    }
    if input.hook_event_name.as_deref() != Some("SessionStart") {
        return Ok(());
    }
    let Some(path) = env::var_os(CLAUDE_ENV_FILE_ENV).map(PathBuf::from) else {
        return Ok(());
    };
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{CLAUDE_SESSION_ID_ENV}={}", input.session_id.trim())?;
    Ok(())
}

fn handle_session_end(input: &HookInput) -> Result<(), Box<dyn std::error::Error>> {
    if input.session_id.trim().is_empty() {
        return Ok(());
    }
    if input.hook_event_name.as_deref() != Some("SessionEnd") {
        return Ok(());
    }
    if input.reason.as_deref() != Some("clear") {
        return Ok(());
    }

    for record in load_instance_records_for_session(input.session_id.trim())? {
        let path = PathBuf::from(record.control_path);
        request_restart(&path)?;
    }
    Ok(())
}

fn current_claude_session_id() -> Option<String> {
    current_claude_session_id_from_sources(
        env::var_os(CLAUDE_ENV_FILE_ENV).as_deref().map(Path::new),
    )
}

fn current_claude_session_id_from_sources(env_file_path: Option<&Path>) -> Option<String> {
    read_session_id_from_env_file(env_file_path)
        .or_else(|| env::var(CLAUDE_SESSION_ID_ENV).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
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
        let (key, value) = line.split_once('=')?;
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

fn read_control_request(path: &Path) -> Option<ControlRequest> {
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_control_request(path: &Path, request: &ControlRequest) -> io::Result<()> {
    write_json_atomic(path, request)
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
        if record.claude_session_id == session_id {
            out.push(record);
        }
    }
    Ok(out)
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
    let tmp_name = format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("state")
    );
    let tmp_path = parent.join(tmp_name);
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|err| io::Error::other(format!("failed to serialize json: {err}")))?;
    fs::write(&tmp_path, bytes)?;
    fs::rename(&tmp_path, path)?;
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
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn session_start_hook_appends_session_id_to_claude_env_file() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let temp = tempfile::tempdir().expect("tempdir");
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
        assert!(raw.contains("MCP_REPL_CLAUDE_SESSION_ID=sess-start"));

        unsafe {
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

        let session_id = current_claude_session_id_from_sources(Some(&env_file))
            .expect("current claude session id");
        assert_eq!(session_id, "sess-latest");

        unsafe {
            env::remove_var(CLAUDE_SESSION_ID_ENV);
        }
    }
}
