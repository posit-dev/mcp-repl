use std::path::PathBuf;
use std::process::Command;

type TestResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn release_binary_path() -> Option<PathBuf> {
    std::env::var_os("MCP_REPL_RELEASE_BINARY").map(PathBuf::from)
}

#[test]
fn shipped_release_binary_does_not_expose_debug_repl() -> TestResult<()> {
    let Some(exe) = release_binary_path() else {
        eprintln!("MCP_REPL_RELEASE_BINARY not set; skipping release CLI contract");
        return Ok(());
    };

    let help = Command::new(&exe).arg("--help").output()?;
    assert!(
        help.status.success(),
        "expected release binary --help to succeed"
    );
    let stdout = String::from_utf8_lossy(&help.stdout);
    assert!(
        !stdout.contains("--debug-repl"),
        "release binary help must not expose --debug-repl, got: {stdout:?}"
    );

    let debug_repl = Command::new(&exe).arg("--debug-repl").output()?;
    assert!(
        !debug_repl.status.success(),
        "expected release binary to reject --debug-repl"
    );
    let stderr = String::from_utf8_lossy(&debug_repl.stderr);
    assert!(
        stderr.contains("debug-repl") || stderr.contains("unknown argument"),
        "expected --debug-repl rejection to mention the flag, got: {stderr:?}"
    );

    Ok(())
}
