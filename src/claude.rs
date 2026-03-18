use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};

use crate::backend::Backend;
use crate::worker_process::{WorkerError, WorkerManager};

pub const CLAUDE_SESSION_ID_ENV: &str = "MCP_REPL_CLAUDE_SESSION_ID";
pub const CLAUDE_ENV_FILE_ENV: &str = "CLAUDE_ENV_FILE";

const CONTROL_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const STATE_SUBDIR: &str = "mcp-repl/claude-clear";
const CLAUDE_SESSION_ID_TOKEN_PREFIX: &str = "mcp_repl_session_id_b64_";
static NEXT_TMP_FILE_ID: AtomicU64 = AtomicU64::new(0);

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
    last_control_seq: Mutex<u64>,
}

#[derive(Debug, Clone)]
struct InstanceRecordTemplate {
    backend: String,
    pid: u32,
    cwd: Option<String>,
    started_unix_ms: u128,
}

#[derive(Debug, Clone)]
struct ClaudeSessionBinding {
    env_file_path: PathBuf,
    session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InstanceRecord {
    claude_session_id: String,
    env_file_path: String,
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
        Self::maybe_register_with_initial_seq(backend, 0)
    }

    pub fn maybe_register_late(backend: Backend) -> Result<Option<Self>, WorkerError> {
        Self::maybe_register_with_initial_seq(backend, 1)
    }

    fn maybe_register_with_initial_seq(
        backend: Backend,
        initial_control_seq: u64,
    ) -> Result<Option<Self>, WorkerError> {
        let Some(binding_session) = current_claude_session_from_env_file() else {
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
                last_control_seq: Mutex::new(0),
            }),
        };
        write_control_request(
            &binding.inner.control_path,
            &ControlRequest {
                seq: initial_control_seq,
                op: "restart".to_string(),
                requested_unix_ms: started_unix_ms,
            },
        )
        .map_err(WorkerError::Io)?;
        binding
            .write_record(
                &binding_session,
                &InstanceRecordTemplate {
                    backend: backend_label.to_string(),
                    pid,
                    cwd: env::current_dir()
                        .ok()
                        .map(|path| path.to_string_lossy().to_string()),
                    started_unix_ms,
                },
            )
            .map_err(WorkerError::Io)?;
        Ok(Some(binding))
    }

    pub fn sync(&self, worker: &mut WorkerManager) -> Result<(), WorkerError> {
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
    ) -> io::Result<()> {
        let record = InstanceRecord {
            claude_session_id: binding_session.session_id.clone(),
            env_file_path: binding_session.env_file_path.to_string_lossy().to_string(),
            backend: template.backend.clone(),
            pid: template.pid,
            cwd: template.cwd.clone(),
            control_path: self.inner.control_path.to_string_lossy().to_string(),
            started_unix_ms: template.started_unix_ms,
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
    let Some(path) = env::var_os(CLAUDE_ENV_FILE_ENV).map(PathBuf::from) else {
        return Ok(());
    };
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
    let encoded_session_id = URL_SAFE_NO_PAD.encode(session_id);
    writeln!(
        file,
        "export {CLAUDE_SESSION_ID_ENV}={CLAUDE_SESSION_ID_TOKEN_PREFIX}{encoded_session_id}"
    )?;
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
    let Some(env_file_path) = env::var_os(CLAUDE_ENV_FILE_ENV).map(PathBuf::from) else {
        return Ok(());
    };
    if fs::read_to_string(&env_file_path).is_err() {
        return Ok(());
    }

    for record in load_instance_records_for_binding(&env_file_path, session_id)? {
        let path = PathBuf::from(record.control_path);
        request_restart(&path)?;
    }
    Ok(())
}

fn current_claude_session_from_env_file() -> Option<ClaudeSessionBinding> {
    let env_file_path = env::var_os(CLAUDE_ENV_FILE_ENV).map(PathBuf::from)?;
    let session_id = read_session_id_from_env_file(Some(&env_file_path))?
        .trim()
        .to_string();
    if session_id.is_empty() {
        return None;
    }
    Some(ClaudeSessionBinding {
        env_file_path,
        session_id,
    })
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

fn env_file_needs_separator(path: &Path) -> io::Result<bool> {
    let Ok(raw) = fs::read(path) else {
        return Ok(false);
    };
    Ok(!raw.is_empty() && !raw.ends_with(b"\n"))
}

fn load_instance_records_for_binding(
    env_file_path: &Path,
    session_id: &str,
) -> io::Result<Vec<InstanceRecord>> {
    let env_file_path = env_file_path.to_string_lossy().to_string();
    let mut out = Vec::new();
    for record in load_instance_records()? {
        if record.claude_session_id == session_id && record.env_file_path == env_file_path {
            out.push(record);
        }
    }
    Ok(out)
}

fn load_instance_records() -> io::Result<Vec<InstanceRecord>> {
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
        out.push(record);
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
        let controls_dir = state_root.join("controls");
        fs::create_dir_all(&instances_dir).expect("create instances dir");
        fs::create_dir_all(&controls_dir).expect("create controls dir");

        let control_a = controls_dir.join("r-a.json");
        let control_b = controls_dir.join("r-b.json");
        write_control_request(
            &control_a,
            &ControlRequest {
                seq: 0,
                op: "restart".to_string(),
                requested_unix_ms: 1,
            },
        )
        .expect("seed control a");
        write_control_request(
            &control_b,
            &ControlRequest {
                seq: 0,
                op: "restart".to_string(),
                requested_unix_ms: 1,
            },
        )
        .expect("seed control b");
        write_json_atomic(
            &instances_dir.join("r-a.json"),
            &InstanceRecord {
                claude_session_id: "sess-shared".to_string(),
                env_file_path: env_file_a.to_string_lossy().to_string(),
                backend: "r".to_string(),
                pid: 1,
                cwd: None,
                control_path: control_a.to_string_lossy().to_string(),
                started_unix_ms: 1,
            },
        )
        .expect("write record a");
        write_json_atomic(
            &instances_dir.join("r-b.json"),
            &InstanceRecord {
                claude_session_id: "sess-shared".to_string(),
                env_file_path: env_file_b.to_string_lossy().to_string(),
                backend: "r".to_string(),
                pid: 2,
                cwd: None,
                control_path: control_b.to_string_lossy().to_string(),
                started_unix_ms: 1,
            },
        )
        .expect("write record b");

        handle_session_end(&HookInput {
            hook_event_name: Some("SessionEnd".to_string()),
            session_id: "sess-shared".to_string(),
            reason: Some("clear".to_string()),
        })
        .expect("handle session end");

        let request_a = read_control_request(&control_a).expect("read control a");
        let request_b = read_control_request(&control_b).expect("read control b");
        assert_eq!(
            request_a.seq, 0,
            "env file mismatch should not queue restart"
        );
        assert_eq!(
            request_b.seq, 1,
            "exact env file + session should queue restart"
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
        assert_eq!(binding_session.env_file_path, env_file);
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
  "control_path": "/tmp/control.json",
  "started_unix_ms": 1,
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
        request_restart(&binding.inner.control_path).expect("queue restart request");

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

    fn current_claude_session_from_env_file_with_path(path: &Path) -> Option<ClaudeSessionBinding> {
        let session_id = read_session_id_from_env_file(Some(path))?
            .trim()
            .to_string();
        if session_id.is_empty() {
            return None;
        }
        Some(ClaudeSessionBinding {
            env_file_path: path.to_path_buf(),
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
