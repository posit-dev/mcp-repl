mod common;

#[cfg(target_family = "unix")]
mod unix {
    use std::os::unix::io::RawFd;
    use std::path::PathBuf;
    use std::process::Stdio;
    use std::time::Duration;

    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;
    use tokio::time;

    use crate::common::TestResult;

    fn set_cloexec(fd: RawFd, enabled: bool) -> TestResult<()> {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let new_flags = if enabled {
            flags | libc::FD_CLOEXEC
        } else {
            flags & !libc::FD_CLOEXEC
        };
        let rc = unsafe { libc::fcntl(fd, libc::F_SETFD, new_flags) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }

    fn pipe_pair() -> TestResult<(RawFd, RawFd)> {
        let mut fds = [0_i32; 2];
        let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok((fds[0], fds[1]))
    }

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
    async fn worker_exits_when_ipc_disconnects() -> TestResult<()> {
        let exe = resolve_exe()?;
        // Create the same IPC topology as `IpcServer::bind()`:
        // - pipe a: worker writes -> server reads
        // - pipe b: server writes -> worker reads
        let (server_read_fd, child_write_fd) = pipe_pair()?;
        let (child_read_fd, server_write_fd) = pipe_pair()?;
        // Ensure the worker does not inherit the server ends (otherwise EOF never arrives).
        set_cloexec(server_read_fd, true)?;
        set_cloexec(server_write_fd, true)?;
        // Ensure child fds are inherited across exec.
        set_cloexec(child_read_fd, false)?;
        set_cloexec(child_write_fd, false)?;

        let mut child = Command::new(exe)
            .arg("--worker")
            .env_remove("R_PROFILE_USER")
            .env_remove("R_PROFILE_SITE")
            .env("MCP_REPL_IPC_READ_FD", child_read_fd.to_string())
            .env("MCP_REPL_IPC_WRITE_FD", child_write_fd.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        // Close child ends in the parent: the worker owns these now.
        unsafe {
            libc::close(child_read_fd);
            libc::close(child_write_fd);
        }

        let mut stdin = child.stdin.take().ok_or("missing child stdin")?;
        stdin.write_all(b"cat(\"OK\\n\")\n").await?;
        stdin.flush().await?;

        // Simulate server IPC disconnect: close both server ends.
        unsafe {
            libc::close(server_write_fd);
            libc::close(server_read_fd);
        }

        let status = match time::timeout(Duration::from_secs(10), child.wait()).await {
            Ok(status) => status?,
            Err(_) => return Err("worker did not exit after IPC closed".into()),
        };
        assert!(status.success(), "worker exit status: {status:?}");

        Ok(())
    }
}
