use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value as JsonValue, json};

const EVENTS_LOG_FILE: &str = "events.jsonl";

static LOGGER: OnceLock<Option<Arc<EventLogger>>> = OnceLock::new();

#[derive(Debug, Clone)]
pub struct StartupContext {
    pub mode: String,
    pub backend: String,
    pub debug_repl: bool,
    pub sandbox_state: Option<String>,
}

#[derive(Debug)]
struct EventLogger {
    file: Mutex<File>,
    file_path: PathBuf,
    startup_epoch: Instant,
    session_instance_id: String,
    pid: u32,
    seq: AtomicU64,
}

impl EventLogger {
    fn new(file_path: PathBuf) -> Result<Self, Box<dyn std::error::Error>> {
        let unix_ms = unix_ms_now();
        let pid = std::process::id();
        let session_instance_id = format!("{unix_ms}-{pid}");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file_path)?;
        Ok(Self {
            file: Mutex::new(file),
            file_path,
            startup_epoch: Instant::now(),
            session_instance_id,
            pid,
            seq: AtomicU64::new(0),
        })
    }

    fn write_event(&self, event: &str, payload: JsonValue) -> Result<(), std::io::Error> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let line = json!({
            "ts_unix_ms": unix_ms_now(),
            "uptime_ms": self.startup_epoch.elapsed().as_millis(),
            "seq": seq,
            "session_instance_id": self.session_instance_id,
            "pid": self.pid,
            "event": event,
            "payload": payload,
        });
        let mut file = self.file.lock().expect("event logger mutex poisoned");
        writeln!(file, "{line}")?;
        file.flush()?;
        Ok(())
    }
}

pub fn initialize(
    debug_dir: Option<PathBuf>,
    context: StartupContext,
) -> Result<(), Box<dyn std::error::Error>> {
    if LOGGER.get().is_some() {
        return Ok(());
    }

    crate::debug_logs::initialize(debug_dir)?;
    let maybe_logger = if let Some(file_path) = crate::debug_logs::log_path(EVENTS_LOG_FILE) {
        let logger = Arc::new(EventLogger::new(file_path)?);
        logger.write_event("startup", startup_payload(&context, &logger.file_path))?;
        Some(logger)
    } else {
        None
    };
    let _ = LOGGER.set(maybe_logger);
    Ok(())
}

pub fn log(event: &str, payload: JsonValue) {
    let Some(logger) = current_logger() else {
        return;
    };
    let _ = logger.write_event(event, payload);
}

pub fn log_lazy<F>(event: &str, payload: F)
where
    F: FnOnce() -> JsonValue,
{
    let Some(logger) = current_logger() else {
        return;
    };
    let _ = logger.write_event(event, payload());
}

fn current_logger() -> Option<Arc<EventLogger>> {
    LOGGER.get().and_then(|entry| entry.clone())
}

fn startup_payload(context: &StartupContext, file_path: &Path) -> JsonValue {
    let cwd = std::env::current_dir()
        .ok()
        .map(|path| path.to_string_lossy().to_string());
    let argv: Vec<String> = std::env::args_os()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let codex_session_id = codex_session_id();
    let codex_env = visible_codex_env();
    json!({
        "mode": context.mode,
        "backend": context.backend,
        "debug_repl": context.debug_repl,
        "sandbox_state": context.sandbox_state,
        "cwd": cwd,
        "argv": argv,
        "log_file": file_path.to_string_lossy().to_string(),
        "codex_session_id": codex_session_id,
        "codex_env": codex_env,
    })
}

fn codex_session_id() -> Option<String> {
    for key in [
        "CODEX_SESSION_ID",
        "CODEX_THREAD_ID",
        "CODEX_CONVERSATION_ID",
    ] {
        if let Ok(value) = std::env::var(key)
            && !value.trim().is_empty()
        {
            return Some(value);
        }
    }
    None
}

fn visible_codex_env() -> BTreeMap<String, String> {
    visible_codex_env_from_iter(std::env::vars_os().filter_map(|(raw_key, raw_value)| {
        let key = raw_key.into_string().ok()?;
        Some((key, raw_value.to_string_lossy().into_owned()))
    }))
}

fn visible_codex_env_from_iter<I>(iter: I) -> BTreeMap<String, String>
where
    I: IntoIterator<Item = (String, String)>,
{
    let mut out = BTreeMap::new();
    for (key, value) in iter {
        if !key.starts_with("CODEX_") {
            continue;
        }
        if is_sensitive_env_key(&key) {
            continue;
        }
        out.insert(key, value);
    }
    out
}

fn is_sensitive_env_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    ["KEY", "TOKEN", "SECRET", "PASSWORD"]
        .iter()
        .any(|needle| upper.contains(needle))
}

fn unix_ms_now() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_codex_env_filters_sensitive_keys() {
        let env = vec![
            ("CODEX_SESSION_ID".to_string(), "sess-123".to_string()),
            ("CODEX_SANDBOX".to_string(), "seatbelt".to_string()),
            ("CODEX_API_KEY".to_string(), "redacted".to_string()),
            ("OTHER_VAR".to_string(), "ignored".to_string()),
        ];
        let filtered = visible_codex_env_from_iter(env);
        assert_eq!(
            filtered.get("CODEX_SESSION_ID"),
            Some(&"sess-123".to_string())
        );
        assert_eq!(filtered.get("CODEX_SANDBOX"), Some(&"seatbelt".to_string()));
        assert!(!filtered.contains_key("CODEX_API_KEY"));
        assert!(!filtered.contains_key("OTHER_VAR"));
    }

    #[test]
    fn logger_writes_jsonl_event() {
        let temp = tempfile::tempdir().expect("tempdir");
        let logger = EventLogger::new(temp.path().join(EVENTS_LOG_FILE)).expect("create logger");
        logger
            .write_event("test-event", json!({"ok": true}))
            .expect("write event");
        let text = std::fs::read_to_string(&logger.file_path).expect("read event log");
        assert!(text.contains("\"event\":\"test-event\""));
        assert!(text.contains("\"ok\":true"));
        assert!(text.contains("\"session_instance_id\""));
    }

    #[test]
    fn logger_uses_stable_events_file_name() {
        let temp = tempfile::tempdir().expect("tempdir");
        let logger = EventLogger::new(temp.path().join(EVENTS_LOG_FILE)).expect("create logger");
        assert_eq!(
            logger.file_path.file_name().and_then(|name| name.to_str()),
            Some(EVENTS_LOG_FILE)
        );
    }

    #[test]
    fn log_lazy_does_not_build_payload_without_logger() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let invoked = AtomicBool::new(false);
        super::log_lazy("test-event", || {
            invoked.store(true, Ordering::SeqCst);
            json!({"ok": true})
        });
        assert!(!invoked.load(Ordering::SeqCst));
    }
}
