use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tempfile::tempdir;

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
    {
        let candidate = "mcp-repl";
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

fn bundle_transcript_path(text: &str) -> Option<PathBuf> {
    let end = text
        .find("transcript.txt")?
        .saturating_add("transcript.txt".len());
    let start = text[..end]
        .rfind(|ch: char| ch.is_whitespace() || matches!(ch, '"' | '\'' | '[' | '('))
        .map_or(0, |idx| idx.saturating_add(1));
    Some(PathBuf::from(&text[start..end]))
}

fn backend_unavailable(stdout: &str, stderr: &str) -> bool {
    stdout.is_empty()
        || stderr.contains("Fatal error: cannot create 'R_TempDir'")
        || stderr.contains("failed to start R session")
        || stderr.contains("worker protocol error: ipc disconnected while waiting for backend info")
        || stderr.contains("worker exited with status")
        || stderr.contains("[repl] error")
}

fn debug_repl_test_mutex() -> &'static Mutex<()> {
    static TEST_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
    TEST_MUTEX.get_or_init(|| Mutex::new(()))
}

fn wait_for_prompt_or_idle(
    rx: &mpsc::Receiver<Vec<u8>>,
    seen: &mut Vec<u8>,
    deadline: Instant,
) -> (bool, bool) {
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
                let output = String::from_utf8_lossy(seen);
                if output.contains("> ") {
                    saw_prompt = true;
                    break;
                }
                if output.contains("<<repl status: idle>>") {
                    saw_idle = true;
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    (saw_prompt, saw_idle)
}

fn assert_debug_repl_starts(extra_args: &[&str]) -> TestResult<()> {
    let _guard = debug_repl_test_mutex()
        .lock()
        .expect("debug repl prompt test mutex poisoned");
    let exe = resolve_mcp_repl_path()?;
    let mut cmd = Command::new(exe);
    cmd.arg("--debug-repl");
    cmd.args(extra_args);
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
    let (saw_prompt, saw_idle) = wait_for_prompt_or_idle(&rx, &mut seen, deadline);

    drop(child.stdin.take());
    let _ = child.kill();
    let _ = child.wait();

    let output = String::from_utf8_lossy(&seen);
    let mut err_seen = Vec::new();
    while let Ok(chunk) = err_rx.try_recv() {
        err_seen.extend_from_slice(&chunk);
    }
    let err_output = String::from_utf8_lossy(&err_seen);
    let backend_unavailable = backend_unavailable(&output, &err_output);
    if !((saw_prompt && output.contains("> "))
        || (saw_idle && output.contains("<<repl status: idle>>")))
        && backend_unavailable
    {
        eprintln!("debug_repl backend unavailable in this environment; skipping");
        return Ok(());
    }
    assert!(
        (saw_prompt && output.contains("> "))
            || (saw_idle && output.contains("<<repl status: idle>>")),
        "expected prompt or idle status in stdout, got: {output:?}, stderr: {err_output:?}"
    );
    Ok(())
}

#[test]
fn debug_repl_prints_initial_prompt() -> TestResult<()> {
    assert_debug_repl_starts(&[])
}

#[test]
fn debug_repl_inherit_prints_initial_prompt() -> TestResult<()> {
    assert_debug_repl_starts(&["--sandbox", "inherit"])
}

#[test]
fn debug_repl_files_mode_uses_output_bundle_dir_for_large_output() -> TestResult<()> {
    let _guard = debug_repl_test_mutex()
        .lock()
        .expect("debug repl prompt test mutex poisoned");
    let exe = resolve_mcp_repl_path()?;
    let temp = tempdir()?;
    let mut cmd = Command::new(exe);
    cmd.arg("--debug-repl")
        .arg("--oversized-output")
        .arg("files");
    #[cfg(target_os = "macos")]
    if !sandbox_exec_available() {
        cmd.arg("--sandbox").arg("danger-full-access");
    }
    let mut child = cmd
        .env("MCP_REPL_IMAGES", "0")
        .env("MCP_REPL_PAGER_PAGE_CHARS", "1000000")
        .env("TMPDIR", temp.path())
        .env("TMP", temp.path())
        .env("TEMP", temp.path())
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

    let startup_deadline = Instant::now() + Duration::from_secs(20);
    let mut startup_seen = Vec::new();
    let (saw_prompt, saw_idle) = wait_for_prompt_or_idle(&rx, &mut startup_seen, startup_deadline);

    let mut startup_err_seen = Vec::new();
    while let Ok(chunk) = err_rx.try_recv() {
        startup_err_seen.extend_from_slice(&chunk);
    }
    let startup_stdout = String::from_utf8_lossy(&startup_seen);
    let startup_stderr = String::from_utf8_lossy(&startup_err_seen);
    if !((saw_prompt && startup_stdout.contains("> "))
        || (saw_idle && startup_stdout.contains("<<repl status: idle>>")))
        && backend_unavailable(&startup_stdout, &startup_stderr)
    {
        drop(child.stdin.take());
        let _ = child.kill();
        let _ = child.wait();
        eprintln!("debug_repl backend unavailable in this environment; skipping");
        return Ok(());
    }
    assert!(
        (saw_prompt && startup_stdout.contains("> "))
            || (saw_idle && startup_stdout.contains("<<repl status: idle>>")),
        "expected prompt or idle status before sending debug repl input, got stdout: {startup_stdout:?}, stderr: {startup_stderr:?}"
    );

    {
        let stdin = child.stdin.as_mut().ok_or("missing stdin")?;
        write!(
            stdin,
            "big <- paste(rep('x', 5000), collapse = ''); cat('BUNDLE_START\\n'); cat(big); cat('\\nBUNDLE_END\\n')\nEND\n"
        )?;
        stdin.flush()?;
    }

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut seen = Vec::new();
    let transcript_path = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break None;
        }
        match rx.recv_timeout(remaining.min(Duration::from_millis(250))) {
            Ok(chunk) => {
                seen.extend_from_slice(&chunk);
                let stdout = String::from_utf8_lossy(&seen);
                if let Some(path) = bundle_transcript_path(&stdout) {
                    break Some(path);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break None,
        }
    };

    let stdout = String::from_utf8_lossy(&seen);
    let mut err_seen = Vec::new();
    while let Ok(chunk) = err_rx.try_recv() {
        err_seen.extend_from_slice(&chunk);
    }
    let stderr = String::from_utf8_lossy(&err_seen);
    if backend_unavailable(&stdout, &stderr) {
        drop(child.stdin.take());
        let _ = child.kill();
        let _ = child.wait();
        eprintln!("debug_repl backend unavailable in this environment; skipping");
        return Ok(());
    }

    let transcript_path = transcript_path.unwrap_or_else(|| {
        drop(child.stdin.take());
        let _ = child.kill();
        let _ = child.wait();
        panic!("expected transcript path in debug repl files mode output, got stdout: {stdout:?}, stderr: {stderr:?}")
    });
    let transcript = fs::read_to_string(&transcript_path)?;
    drop(child.stdin.take());
    let _ = child.kill();
    let _ = child.wait();

    assert!(
        transcript.contains("BUNDLE_START") && transcript.contains("BUNDLE_END"),
        "expected transcript bundle to capture the large debug repl output, got: {transcript:?}"
    );

    Ok(())
}
