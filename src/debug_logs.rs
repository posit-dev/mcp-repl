use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

pub const DEBUG_DIR_ENV: &str = "MCP_REPL_DEBUG_DIR";
pub(crate) const DEBUG_SESSION_DIR_ENV: &str = "MCP_REPL_DEBUG_SESSION_DIR";

static SESSION_STATE: OnceLock<Mutex<SessionState>> = OnceLock::new();

#[derive(Debug, Clone)]
enum SessionState {
    Uninitialized,
    Ready(Option<PathBuf>),
    Failed(String),
}

pub fn initialize(cli_debug_dir: Option<PathBuf>) -> Result<(), Box<dyn std::error::Error>> {
    let mut state = session_state()
        .lock()
        .expect("debug session state mutex poisoned");
    match &*state {
        SessionState::Ready(Some(_)) => return Ok(()),
        SessionState::Ready(None) if cli_debug_dir.is_none() => return Ok(()),
        SessionState::Failed(message) => return Err(message.clone().into()),
        SessionState::Ready(None) | SessionState::Uninitialized => {}
    }

    let session_dir = match resolve_session_dir(cli_debug_dir) {
        Ok(path) => path,
        Err(err) => {
            let message = err.to_string();
            *state = SessionState::Failed(message.clone());
            return Err(message.into());
        }
    };
    *state = SessionState::Ready(session_dir);
    Ok(())
}

pub fn session_dir() -> Option<PathBuf> {
    let mut state = session_state()
        .lock()
        .expect("debug session state mutex poisoned");
    match &*state {
        SessionState::Ready(path) => return path.clone(),
        SessionState::Failed(_) => return None,
        SessionState::Uninitialized => {}
    }

    match resolve_session_dir(None) {
        Ok(path) => {
            *state = SessionState::Ready(path.clone());
            path
        }
        Err(err) => {
            *state = SessionState::Failed(err.to_string());
            None
        }
    }
}

pub fn log_path(file_name: &str) -> Option<PathBuf> {
    session_dir().map(|dir| dir.join(file_name))
}

pub fn apply_child_env(command: &mut Command) {
    let Some(path) = propagated_session_dir() else {
        return;
    };
    command.env(DEBUG_SESSION_DIR_ENV, path);
}

fn propagated_session_dir() -> Option<PathBuf> {
    session_dir_from_env_var(DEBUG_SESSION_DIR_ENV).or_else(session_dir)
}

fn resolve_session_dir(
    cli_debug_dir: Option<PathBuf>,
) -> Result<Option<PathBuf>, Box<dyn std::error::Error>> {
    if let Some(path) = session_dir_from_env_var(DEBUG_SESSION_DIR_ENV) {
        fs::create_dir_all(&path)?;
        return Ok(Some(path));
    }

    let Some(base_dir) = cli_debug_dir
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(find_debug_dir_from_args)
        .or_else(|| session_dir_from_env_var(DEBUG_DIR_ENV))
    else {
        return Ok(None);
    };

    fs::create_dir_all(&base_dir)?;
    let session_dir = create_unique_session_dir(&base_dir)?;
    Ok(Some(session_dir))
}

fn session_dir_from_env_var(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn find_debug_dir_from_args() -> Option<PathBuf> {
    parse_debug_dir_arg(std::env::args_os().skip(1))
}

fn parse_debug_dir_arg<I>(args: I) -> Option<PathBuf>
where
    I: IntoIterator,
    I::Item: Into<std::ffi::OsString>,
{
    let args = args.into_iter().map(Into::into).collect::<Vec<_>>();
    if args.first().is_some_and(|arg| arg == "install") {
        return None;
    }

    let mut args = args.into_iter();
    let mut parsed = None;
    while let Some(arg) = args.next() {
        if arg == "--debug-dir" {
            let value = args.next()?;
            if value.is_empty() {
                return None;
            }
            parsed = Some(PathBuf::from(value));
            continue;
        }
        let arg = arg.to_string_lossy();
        if let Some(value) = arg.strip_prefix("--debug-dir=") {
            if value.is_empty() {
                return None;
            }
            parsed = Some(PathBuf::from(value));
        }
    }
    parsed
}

fn create_unique_session_dir(base_dir: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let pid = std::process::id();

    for suffix in 0u32..1_000u32 {
        let name = if suffix == 0 {
            format!("session-{unix_ms}-{pid}")
        } else {
            format!("session-{unix_ms}-{pid}-{suffix}")
        };
        let path = base_dir.join(name);
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(Box::new(err)),
        }
    }

    Err("failed to allocate unique debug session directory after 1000 attempts".into())
}

fn session_state() -> &'static Mutex<SessionState> {
    SESSION_STATE.get_or_init(|| Mutex::new(SessionState::Uninitialized))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reset_session_state_for_test() {
        *session_state()
            .lock()
            .expect("debug session state mutex poisoned") = SessionState::Uninitialized;
    }

    #[test]
    fn create_unique_session_dir_creates_child_directory() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = create_unique_session_dir(temp.path()).expect("create session dir");
        assert!(path.is_dir());
        assert_eq!(path.parent(), Some(temp.path()));
        assert!(
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("session-"))
        );
    }

    #[test]
    fn find_debug_dir_from_equals_arg_parses_path() {
        let parsed = parse_debug_dir_arg(["--debug-dir=/tmp/mcp-repl-debug"]);
        assert_eq!(parsed, Some(PathBuf::from("/tmp/mcp-repl-debug")));
    }

    #[test]
    fn parse_debug_dir_arg_uses_last_occurrence() {
        let parsed = parse_debug_dir_arg(["--debug-dir=/tmp/first", "--debug-dir", "/tmp/final"]);
        assert_eq!(parsed, Some(PathBuf::from("/tmp/final")));
    }

    #[test]
    fn parse_debug_dir_arg_ignores_install_passthrough_args() {
        let parsed =
            parse_debug_dir_arg(["install", "--arg", "--debug-dir", "--arg", "/tmp/final"]);
        assert_eq!(parsed, None);
    }

    #[test]
    fn initialize_preserves_early_debug_dir_failure() {
        let temp = tempfile::tempdir().expect("tempdir");
        let bad_path = temp.path().join("debug-root");
        std::fs::write(&bad_path, "not a directory").expect("write bad path");

        let original = std::env::var_os(DEBUG_DIR_ENV);
        reset_session_state_for_test();
        unsafe {
            std::env::set_var(DEBUG_DIR_ENV, &bad_path);
        }

        assert_eq!(session_dir(), None);
        let err = initialize(None).expect_err("initialize should preserve earlier failure");

        match original {
            Some(value) => unsafe {
                std::env::set_var(DEBUG_DIR_ENV, value);
            },
            None => unsafe {
                std::env::remove_var(DEBUG_DIR_ENV);
            },
        }
        reset_session_state_for_test();

        let message = err.to_string();
        assert!(
            message.contains("debug-root")
                || message.contains("directory")
                || message.contains("exists"),
            "unexpected error: {message}"
        );
    }
}
