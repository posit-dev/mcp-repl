use std::path::PathBuf;
use std::process::Command;

type TestResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn resolve_mcp_repl_path() -> TestResult<PathBuf> {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_mcp-repl") {
        return Ok(PathBuf::from(path));
    }

    let mut path = std::env::current_exe()?;
    path.pop();
    path.pop();
    let mut candidate_path = path;
    candidate_path.push("mcp-repl");
    if cfg!(windows) {
        candidate_path.set_extension("exe");
    }
    if candidate_path.exists() {
        return Ok(candidate_path);
    }

    Err("unable to locate mcp-repl test binary".into())
}

#[test]
fn help_mentions_oversized_output_flag() -> TestResult<()> {
    let exe = resolve_mcp_repl_path()?;
    let output = Command::new(exe).arg("--help").output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "expected --help to succeed");
    assert!(
        stdout.contains("--oversized-output"),
        "expected --help to mention --oversized-output, got: {stdout:?}"
    );
    Ok(())
}

#[test]
fn invalid_oversized_output_value_fails_fast() -> TestResult<()> {
    let exe = resolve_mcp_repl_path()?;
    let output = Command::new(exe)
        .args(["--oversized-output", "bogus"])
        .output()?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "expected invalid oversized-output value to fail"
    );
    assert!(
        stderr.contains("oversized-output") || stderr.contains("unknown argument"),
        "expected oversized-output parse error, got: {stderr:?}"
    );
    Ok(())
}

#[test]
fn repeated_oversized_output_flag_fails_fast() -> TestResult<()> {
    let exe = resolve_mcp_repl_path()?;
    let output = Command::new(exe)
        .args(["--oversized-output", "files", "--oversized-output", "pager"])
        .output()?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "expected repeated oversized-output flag to fail"
    );
    assert!(
        stderr.contains("oversized-output"),
        "expected oversized-output duplication error, got: {stderr:?}"
    );
    Ok(())
}
