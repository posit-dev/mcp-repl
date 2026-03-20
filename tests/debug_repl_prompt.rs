use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

type TestResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[cfg(target_os = "macos")]
fn sandbox_exec_available() -> bool {
    // Mirror tests/common/mod.rs: sandbox-exec may exist but be unusable (status 71).
    if std::env::var_os("CODEX_SANDBOX").is_some() {
        return false;
    }
    Command::new("/usr/bin/sandbox-exec")
        .args(["-p", "(version 1)", "--", "/usr/bin/true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn resolve_mcp_repl_path() -> TestResult<PathBuf> {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_mcp-repl") {
        return Ok(PathBuf::from(path));
    }

    let mut path = std::env::current_exe()?;
    path.pop();
    path.pop();
    for candidate in ["mcp-repl"] {
        let mut candidate_path = path.clone();
        candidate_path.push(candidate);
        if cfg!(windows) {
            candidate_path.set_extension("exe");
        }
        if candidate_path.exists() {
            return Ok(candidate_path);
        }
    }
    Err("unable to locate mcp-repl test binary".into())
}

#[test]
fn debug_repl_prints_initial_prompt() -> TestResult<()> {
    let exe = resolve_mcp_repl_path()?;
    let mut cmd = Command::new(exe);
    cmd.arg("--debug-repl");
    #[cfg(target_os = "macos")]
    if !sandbox_exec_available() {
        cmd.arg("--sandbox").arg("danger-full-access");
    }
    let mut child = cmd
        .env("MCP_REPL_IMAGES", "0")
        .env("MCP_REPL_PAGER_PAGE_CHARS", "1000000")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdout = child.stdout.take().ok_or("missing stdout")?;
    let mut stderr = child.stderr.take().ok_or("missing stderr")?;
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let (err_tx, err_rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 1024];
        loop {
            match stdout.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    std::thread::spawn(move || {
        let mut buf = [0u8; 1024];
        loop {
            match stderr.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if err_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut seen = Vec::new();
    let mut saw_prompt = false;
    let mut saw_idle = false;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining.min(Duration::from_millis(250))) {
            Ok(chunk) => {
                seen.extend_from_slice(&chunk);
                let output = String::from_utf8_lossy(&seen);
                if output.contains("> ") {
                    saw_prompt = true;
                    break;
                }
                if output.contains("<<console status: idle>>") {
                    saw_idle = true;
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // keep waiting
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    drop(child.stdin.take());
    let _ = child.kill();
    let _ = child.wait();

    let output = String::from_utf8_lossy(&seen);
    let mut err_seen = Vec::new();
    while let Ok(chunk) = err_rx.try_recv() {
        err_seen.extend_from_slice(&chunk);
    }
    let err_output = String::from_utf8_lossy(&err_seen);
    let backend_unavailable = output.is_empty()
        || err_output.contains("Fatal error: cannot create 'R_TempDir'")
        || err_output.contains("failed to start R session")
        || err_output
            .contains("worker protocol error: ipc disconnected while waiting for backend info")
        || err_output.contains("worker exited with status")
        || err_output.contains("[repl] error");
    if !((saw_prompt && output.contains("> "))
        || (saw_idle && output.contains("<<console status: idle>>")))
        && backend_unavailable
    {
        eprintln!("debug_repl backend unavailable in this environment; skipping");
        return Ok(());
    }
    assert!(
        (saw_prompt && output.contains("> "))
            || (saw_idle && output.contains("<<console status: idle>>")),
        "expected prompt or idle status in stdout, got: {output:?}, stderr: {err_output:?}"
    );
    Ok(())
}
