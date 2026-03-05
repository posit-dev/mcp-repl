use std::fs::OpenOptions;
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

static STARTUP_LOG_ENABLED: OnceLock<bool> = OnceLock::new();
static STARTUP_EPOCH: OnceLock<Instant> = OnceLock::new();
static STARTUP_LOG_FILE: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
const STARTUP_LOG_PATH_ENV: &str = "MCP_CONSOLE_DEBUG_STARTUP_FILE";
const STARTUP_LOG_DEFAULT: &str = "mcp-console-startup.log";

fn startup_enabled() -> bool {
    *STARTUP_LOG_ENABLED.get_or_init(|| {
        let enabled = std::env::var("MCP_CONSOLE_DEBUG_STARTUP")
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false);
        if enabled {
            return true;
        }
        std::env::var(STARTUP_LOG_PATH_ENV)
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false)
    })
}

fn startup_epoch() -> Instant {
    *STARTUP_EPOCH.get_or_init(Instant::now)
}

pub fn startup_log(message: impl AsRef<str>) {
    if !startup_enabled() {
        return;
    }
    let elapsed = startup_epoch().elapsed();
    let file = STARTUP_LOG_FILE.get_or_init(|| {
        let path = std::env::var(STARTUP_LOG_PATH_ENV)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| STARTUP_LOG_DEFAULT.to_string());
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
