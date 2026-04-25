mod common;

#[cfg(not(windows))]
use common::{McpSnapshot, TestResult};
#[cfg(not(windows))]
use regex_lite::Regex;

#[cfg(not(windows))]
fn python_backend_unavailable(text: &str) -> bool {
    common::backend_unavailable(text)
        || text.contains("python backend requires a unix-style pty")
        || text.contains("worker io error: Permission denied")
}

#[cfg(not(windows))]
fn normalize_python_help_banner(text: String) -> String {
    let version_re =
        Regex::new(r"Welcome to Python \d+\.\d+'s help utility!").expect("python version regex");
    let docs_url_re =
        Regex::new(r"https://docs\.python\.org/\d+\.\d+/tutorial/").expect("python docs url regex");
    let text = version_re.replace_all(&text, "Welcome to Python <VERSION>'s help utility!");
    docs_url_re
        .replace_all(&text, "https://docs.python.org/<VERSION>/tutorial/")
        .to_string()
}

#[cfg(not(windows))]
fn assert_snapshot_or_skip(name: &str, snapshot: &McpSnapshot) -> TestResult<()> {
    let rendered = normalize_python_help_banner(snapshot.render());
    let transcript = normalize_python_help_banner(snapshot.render_transcript());
    if python_backend_unavailable(&rendered) || python_backend_unavailable(&transcript) {
        eprintln!("python help backend unavailable in this environment; skipping");
        return Ok(());
    }

    insta::assert_snapshot!(name, rendered);
    insta::with_settings!({ snapshot_suffix => "transcript" }, {
        insta::assert_snapshot!(name, transcript);
    });
    Ok(())
}

#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread")]
async fn python_help_contract_snapshot() -> TestResult<()> {
    if !common::python_available() {
        eprintln!("python not available; skipping");
        return Ok(());
    }

    let mut snapshot = McpSnapshot::new();
    snapshot
        .python_files_session(
            "files",
            mcp_script! {
                write_stdin("help(len)", timeout = 5.0);
                write_stdin("import pydoc; pydoc.help(len)", timeout = 5.0);
                write_stdin("help()", timeout = 1.0);
                write_stdin("len", timeout = 1.0);
                write_stdin("q", timeout = 1.0);
                write_stdin("1+1", timeout = 5.0);
            },
        )
        .await?;

    assert_snapshot_or_skip("python_help_contract", &snapshot)
}
