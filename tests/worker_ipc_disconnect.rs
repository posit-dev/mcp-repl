mod common;

#[cfg(target_family = "unix")]
mod unix {
    use base64::Engine as _;
    use serde_json::json;
    use std::os::fd::FromRawFd;
    use std::os::unix::io::RawFd;
    use std::path::PathBuf;
    use std::process::Stdio;
    use std::time::Duration;

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
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
        {
            let candidate = "mcp-repl";
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

    #[tokio::test]
    async fn worker_reads_raw_stdin_with_ipc_request_boundary() -> TestResult<()> {
        let exe = resolve_exe()?;
        let (server_read_fd, child_write_fd) = pipe_pair()?;
        let (child_read_fd, server_write_fd) = pipe_pair()?;
        set_cloexec(server_read_fd, true)?;
        set_cloexec(server_write_fd, true)?;
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

        unsafe {
            libc::close(child_read_fd);
            libc::close(child_write_fd);
        }

        let server_read = unsafe { std::fs::File::from_raw_fd(server_read_fd) };
        let server_write = unsafe { std::fs::File::from_raw_fd(server_write_fd) };
        let mut ipc_reader = BufReader::new(tokio::fs::File::from_std(server_read));
        let mut ipc_writer = tokio::fs::File::from_std(server_write);
        let mut stdin = child.stdin.take().ok_or("missing child stdin")?;

        let input = "if (TRUE) {\ncat(\"RAW_STDIN_OK\\n\")\n}\n";
        let request = json!({
            "type": "python_request_start",
            "request_generation": 1,
            "stdin_b64": base64::engine::general_purpose::STANDARD.encode(input.as_bytes())
        });
        ipc_writer.write_all(request.to_string().as_bytes()).await?;
        ipc_writer.write_all(b"\n").await?;
        ipc_writer.flush().await?;
        stdin.write_all(input.as_bytes()).await?;
        stdin.flush().await?;

        let mut seen = String::new();
        let mut line = String::new();
        let read_result = time::timeout(Duration::from_secs(10), async {
            loop {
                line.clear();
                if ipc_reader.read_line(&mut line).await? == 0 {
                    break Ok::<(), Box<dyn std::error::Error + Send + Sync>>(());
                }
                let value: serde_json::Value = serde_json::from_str(line.trim_end())?;
                if value["type"] != "output_text" {
                    continue;
                }
                let Some(data) = value["data_b64"].as_str() else {
                    continue;
                };
                let bytes = base64::engine::general_purpose::STANDARD.decode(data)?;
                seen.push_str(&String::from_utf8_lossy(&bytes));
                if seen.contains("RAW_STDIN_OK") {
                    break Ok(());
                }
            }
        })
        .await;

        let session_end = json!({ "type": "session_end" });
        let _ = ipc_writer
            .write_all(session_end.to_string().as_bytes())
            .await;
        let _ = ipc_writer.write_all(b"\n").await;
        let _ = ipc_writer.flush().await;
        let _ = time::timeout(Duration::from_secs(10), child.wait()).await;

        match read_result {
            Ok(Ok(())) => {}
            Ok(Err(err)) => return Err(err),
            Err(_) => {
                return Err(format!(
                    "worker did not execute raw stdin request before timeout; saw {seen:?}"
                )
                .into());
            }
        }
        assert!(
            seen.contains("RAW_STDIN_OK"),
            "expected raw stdin output, saw {seen:?}"
        );

        Ok(())
    }
}
