use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

pub const DEBUG_DIR_ENV: &str = "MCP_REPL_DEBUG_DIR";
pub(crate) const DEBUG_SESSION_DIR_ENV: &str = "MCP_REPL_DEBUG_SESSION_DIR";

static SESSION_DIR: OnceLock<Option<PathBuf>> = OnceLock::new();

pub fn initialize(cli_debug_dir: Option<PathBuf>) -> Result<(), Box<dyn std::error::Error>> {
    if SESSION_DIR.get().is_some() {
        return Ok(());
    }
    let session_dir = resolve_session_dir(cli_debug_dir)?;
    let _ = SESSION_DIR.set(session_dir);
    Ok(())
}

pub fn session_dir() -> Option<&'static PathBuf> {
    SESSION_DIR
        .get_or_init(|| resolve_session_dir(None).ok().flatten())
        .as_ref()
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
    session_dir_from_env_var(DEBUG_SESSION_DIR_ENV).or_else(|| session_dir().cloned())
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
    let mut args = args.into_iter().map(Into::into);
    while let Some(arg) = args.next() {
        if arg == "--debug-dir" {
            let value = args.next()?;
            if value.is_empty() {
                return None;
            }
            return Some(PathBuf::from(value));
        }
        let arg = arg.to_string_lossy();
        if let Some(value) = arg.strip_prefix("--debug-dir=") {
            if value.is_empty() {
                return None;
            }
            return Some(PathBuf::from(value));
        }
    }
    None
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
