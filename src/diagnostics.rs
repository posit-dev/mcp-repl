use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

static STARTUP_EPOCH: OnceLock<Instant> = OnceLock::new();
static STARTUP_LOG_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
static STARTUP_LOG_FILE: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
pub(crate) const STARTUP_LOG_FILE_NAME: &str = "startup.log";
pub(crate) const WORKER_STARTUP_LOG_FILE_NAME: &str = "worker-startup.log";
pub(crate) const STARTUP_LOG_PATH_ENV: &str = "MCP_REPL_STARTUP_LOG_PATH";

fn startup_epoch() -> Instant {
    *STARTUP_EPOCH.get_or_init(Instant::now)
}

fn startup_log_path() -> Option<&'static PathBuf> {
    STARTUP_LOG_PATH
        .get_or_init(|| {
            if let Some(path) = std::env::var_os(STARTUP_LOG_PATH_ENV)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
            {
                return Some(path);
            }
            let file_name = if is_worker_mode() {
                WORKER_STARTUP_LOG_FILE_NAME
            } else {
                STARTUP_LOG_FILE_NAME
            };
            crate::debug_logs::log_path(file_name)
        })
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

fn is_worker_mode() -> bool {
    let bare = std::ffi::OsStr::new(crate::worker_protocol::WORKER_MODE_ARG);
    let flag = std::ffi::OsString::from(format!("--{}", crate::worker_protocol::WORKER_MODE_ARG));
    std::env::args_os().any(|arg| arg == bare || arg == flag)
}
