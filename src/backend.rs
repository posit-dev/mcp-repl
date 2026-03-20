pub const INTERPRETER_ENV: &str = "MCP_REPL_INTERPRETER";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    R,
    Python,
}

impl Backend {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_lowercase().as_str() {
            "r" => Ok(Backend::R),
            "python" => Ok(Backend::Python),
            other => Err(format!(
                "invalid interpreter: {other} (expected 'r' or 'python')"
            )),
        }
    }
}

pub fn backend_from_env() -> Result<Option<Backend>, String> {
    let Ok(value) = std::env::var(INTERPRETER_ENV) else {
        return Ok(None);
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    Backend::parse(trimmed).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn backend_from_env_reads_interpreter_env_var() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        unsafe {
            std::env::remove_var(INTERPRETER_ENV);
            std::env::set_var(INTERPRETER_ENV, "python");
        }
        let parsed = backend_from_env().expect("parse env var");
        assert_eq!(parsed, Some(Backend::Python));
        unsafe {
            std::env::remove_var(INTERPRETER_ENV);
        }
    }

    #[test]
    fn backend_from_env_ignores_empty_interpreter_env_var() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        unsafe {
            std::env::remove_var(INTERPRETER_ENV);
            std::env::set_var(INTERPRETER_ENV, "   ");
        }
        let parsed = backend_from_env().expect("parse env var");
        assert_eq!(parsed, None);
        unsafe {
            std::env::remove_var(INTERPRETER_ENV);
        }
    }
}
