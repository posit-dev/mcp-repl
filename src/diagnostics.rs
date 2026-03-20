use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

static STARTUP_EPOCH: OnceLock<Instant> = OnceLock::new();
static STARTUP_LOG_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
static STARTUP_LOG_FILE: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
pub(crate) const STARTUP_LOG_ENV: &str = "MCP_REPL_DEBUG_STARTUP";
pub(crate) const STARTUP_LOG_DEFAULT: &str = "mcp-repl-startup.log";
pub(crate) const WORKER_STARTUP_LOG_DEFAULT: &str = "mcp-repl-worker-startup.log";

pub(crate) fn startup_log_path_from_env(default_path: &str) -> Option<PathBuf> {
    let raw = std::env::var(STARTUP_LOG_ENV).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if is_truthy(trimmed) {
        return Some(PathBuf::from(default_path));
    }
    Some(PathBuf::from(trimmed))
}

fn startup_epoch() -> Instant {
    *STARTUP_EPOCH.get_or_init(Instant::now)
}

fn startup_log_path() -> Option<&'static PathBuf> {
    STARTUP_LOG_PATH
        .get_or_init(|| startup_log_path_from_env(STARTUP_LOG_DEFAULT))
        .as_ref()
}

pub fn startup_log(message: impl AsRef<str>) {
    let Some(path) = startup_log_path() else {
        return;
    };
    let elapsed = startup_epoch().elapsed();
    let file = STARTUP_LOG_FILE.get_or_init(|| {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()
            .map(Mutex::new)
    });
    let Some(file) = file else {
        return;
    };
    if let Ok(mut guard) = file.lock() {
        let _ = writeln!(
            *guard,
            "[repl][startup +{:>6}ms] {}",
            elapsed_ms(elapsed),
            message.as_ref()
        );
        let _ = guard.flush();
    }
}

pub fn elapsed_ms(duration: Duration) -> u128 {
    duration.as_millis()
}

fn is_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}
