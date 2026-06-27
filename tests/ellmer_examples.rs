use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

type TestResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn resolve_mcp_repl_path() -> TestResult<PathBuf> {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_mcp-repl") {
        return Ok(PathBuf::from(path));
    }

    let mut path = std::env::current_exe()?;
    path.pop();
    path.pop();
    path.push("mcp-repl");
    if cfg!(windows) {
        path.set_extension("exe");
    }
    if path.exists() {
        return Ok(path);
    }

    Err("unable to locate mcp-repl test binary".into())
}

fn live_prerequisites_available() -> bool {
    let probe = r#"
packages <- c("ellmer", "mcptools", "jsonlite", "glue", "palmerpenguins")
missing <- packages[!vapply(packages, requireNamespace, logical(1), quietly = TRUE)]
if (length(missing) > 0) {
  cat("missing packages:", paste(missing, collapse = ", "), "\n")
  quit(status = 2)
}
if (!nzchar(Sys.getenv("OPENAI_API_KEY"))) {
  cat("OPENAI_API_KEY is unset\n")
  quit(status = 3)
}
"#;
    match Command::new("Rscript").arg("-e").arg(probe).output() {
        Ok(output) if output.status.success() => true,
        Ok(output) => {
            eprintln!(
                "ellmer examples smoke prerequisites unavailable; skipping:\n{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            false
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("ellmer examples smoke Rscript not found; skipping");
            false
        }
        Err(err) => {
            eprintln!("ellmer examples smoke prerequisites probe failed ({err}); skipping");
            false
        }
    }
}

fn output_with_timeout(mut command: Command, timeout: Duration) -> TestResult<Output> {
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);

    let mut child = command.spawn()?;
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            return Ok(child.wait_with_output()?);
        }
        if Instant::now() >= deadline {
            #[cfg(unix)]
            unsafe {
                libc::killpg(child.id() as i32, libc::SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("command timed out after {}s", timeout.as_secs()).into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn assert_example_runs(script: &str, mcp_repl: &Path) -> TestResult<()> {
    let mut command = Command::new("Rscript");
    command
        .current_dir(repo_root())
        .env("MCP_REPL_BINARY", mcp_repl)
        .arg(script);

    let output = output_with_timeout(command, Duration::from_secs(90))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "{script} failed with status {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
    assert!(
        stdout.to_lowercase().contains("penguin"),
        "{script} should return a penguins answer\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    Ok(())
}

#[test]
fn ellmer_examples_smoke_when_live_prerequisites_exist() -> TestResult<()> {
    if !live_prerequisites_available() {
        return Ok(());
    }
    let mcp_repl = match resolve_mcp_repl_path() {
        Ok(path) => path,
        Err(err) => {
            eprintln!("ellmer examples smoke mcp-repl binary unavailable ({err}); skipping");
            return Ok(());
        }
    };

    assert_example_runs("examples/ellmer-mcp-repl.R", &mcp_repl)?;
    assert_example_runs("examples/ellmer-mcp-repl-files.R", &mcp_repl)
}
