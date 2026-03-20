mod common;

#[cfg(target_family = "unix")]
mod unix {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::path::PathBuf;
    use std::process::Stdio;
    use std::time::Duration;

    use tokio::process::Command;
    use tokio::time;

    use crate::common::TestResult;

    fn resolve_exe() -> TestResult<PathBuf> {
        if let Ok(path) = std::env::var("CARGO_BIN_EXE_mcp-repl") {
            return Ok(PathBuf::from(path));
        }

        let mut path = std::env::current_exe()?;
        path.pop();
        path.pop();
        for candidate in ["mcp-repl"] {
            let mut candidate_path = path.clone();
            candidate_path.push(candidate);
            if candidate_path.exists() {
                return Ok(candidate_path);
            }
        }
        Err("unable to locate mcp-repl test binary".into())
    }

    #[tokio::test]
    async fn debug_events_do_not_panic_on_non_utf8_env() -> TestResult<()> {
        let exe = resolve_exe()?;
        let temp = tempfile::tempdir()?;
        let debug_dir = temp.path().join("events");
        let invalid_utf8 = OsString::from_vec(vec![b'b', b'a', b'd', 0x80]);

        let output = time::timeout(
            Duration::from_secs(15),
            Command::new(exe)
                .arg("--debug-events-dir")
                .arg(&debug_dir)
                .arg("--interpreter")
                .arg("python")
                .env("MCP_REPL_UNRELATED_NON_UTF8", invalid_utf8)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .output(),
        )
        .await
        .map_err(|_| "server startup timed out")??;

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_ne!(
            output.status.code(),
            Some(101),
            "startup panicked with non-UTF-8 env var; stderr: {stderr}"
        );
        assert!(
            !stderr.contains("panicked at"),
            "startup panicked with non-UTF-8 env var; stderr: {stderr}"
        );

        Ok(())
    }

    #[tokio::test]
    async fn debug_events_do_not_panic_on_non_utf8_argv() -> TestResult<()> {
        let exe = resolve_exe()?;
        let temp = tempfile::tempdir()?;
        let debug_dir = temp.path().join("events");
        let invalid_arg = OsString::from_vec(vec![b'-', b'-', b'b', b'a', b'd', b'-', 0x80]);

        let output = time::timeout(
            Duration::from_secs(15),
            Command::new(exe)
                .arg("--debug-events-dir")
                .arg(&debug_dir)
                .arg("--interpreter")
                .arg("python")
                .arg(invalid_arg)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .output(),
        )
        .await
        .map_err(|_| "server startup timed out")??;

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_ne!(
            output.status.code(),
            Some(101),
            "startup panicked with non-UTF-8 argv; stderr: {stderr}"
        );
        assert!(
            !stderr.contains("panicked at"),
            "startup panicked with non-UTF-8 argv; stderr: {stderr}"
        );

        Ok(())
    }
}
