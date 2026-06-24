mod common;

use common::{McpSnapshot, TestResult};
use regex_lite::Regex;

fn python_backend_unavailable(text: &str) -> bool {
    common::backend_unavailable(text)
        || text.contains("worker io error: Permission denied")
        || text.contains("failed to locate a shared libpython")
}

fn normalize_python_help_banner(text: String) -> String {
    let version_re =
        Regex::new(r"Welcome to Python \d+\.\d+'s help utility!").expect("python version regex");
    let docs_url_re =
        Regex::new(r"https://docs\.python\.org/\d+\.\d+/tutorial/").expect("python docs url regex");
    let rendered_prompt_entry_re = Regex::new(
        r#"(?m)^    \{\n      "type": "text",\n      "text": "(>>> |\.\.\. )"\n    \},\n"#,
    )
    .expect("rendered leading prompt entry regex");
    let rendered_trailing_prompt_entry_re = Regex::new(
        r#"(?m),\n    \{\n      "type": "text",\n      "text": "(>>> |\.\.\. )"\n    \}"#,
    )
    .expect("rendered trailing prompt entry regex");
    let text = version_re.replace_all(&text, "Welcome to Python <VERSION>'s help utility!");
    let text = docs_url_re
        .replace_all(&text, "https://docs.python.org/<VERSION>/tutorial/")
        .to_string();
    let text = rendered_prompt_entry_re.replace_all(&text, "").to_string();
    let text = rendered_trailing_prompt_entry_re
        .replace_all(&text, "")
        .to_string();
    let text = text
        .replace(r#""text": ">>> "#, r#""text": ""#)
        .replace(r#""text": "... "#, r#""text": ""#);
    let text = normalize_python_help_intro(text);
    text.replace(r"l\ble\ben\bn", "len")
        .replace("l\u{0008}le\u{0008}en\u{0008}n", "len")
        .lines()
        .map(str::trim_end)
        .filter(|line| !matches!(*line, "<<< >>>" | "<<< ..."))
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_python_help_intro(text: String) -> String {
    let mut out = Vec::new();
    let mut skipping_transcript_intro = false;
    let mut pending_blank_transcript_line = false;

    for line in text.lines() {
        let trimmed_line = line.trim_end();
        if pending_blank_transcript_line {
            if line.starts_with("<<< Welcome to Python <VERSION>'s help utility!") {
                out.push("<<< <PYTHON HELP BANNER>".to_string());
                pending_blank_transcript_line = false;
                skipping_transcript_intro = true;
                continue;
            }
            out.push("<<<".to_string());
            pending_blank_transcript_line = false;
        }

        if skipping_transcript_intro {
            if trimmed_line == "<<< help>" {
                skipping_transcript_intro = false;
                out.push("<<< help>".to_string());
            }
            continue;
        }

        if trimmed_line == "<<<" {
            pending_blank_transcript_line = true;
            continue;
        }

        if line.contains(r#""text": "help()\n"#)
            && line.contains("Welcome to Python <VERSION>'s help utility!")
        {
            out.push(r#"      "text": "help()\n<PYTHON HELP BANNER>""#.to_string());
            continue;
        }

        if line.contains(r#""text": ""#)
            && line.contains("Welcome to Python <VERSION>'s help utility!")
        {
            out.push(r#"      "text": "<PYTHON HELP BANNER>""#.to_string());
            continue;
        }

        if line.starts_with("<<< Welcome to Python <VERSION>'s help utility!") {
            out.push("<<< <PYTHON HELP BANNER>".to_string());
            skipping_transcript_intro = true;
            continue;
        }

        out.push(line.to_string());
    }

    if pending_blank_transcript_line {
        out.push("<<<".to_string());
    }

    out.join("\n")
}

#[test]
fn normalizes_help_banner_after_whitespace_only_transcript_line() {
    let transcript = normalize_python_help_banner(
        ">>> help()\n<<< \n<<< Welcome to Python 3.12's help utility!\n<<< help>".to_string(),
    );

    assert_eq!(
        transcript,
        ">>> help()\n<<< <PYTHON HELP BANNER>\n<<< help>"
    );
}

#[test]
fn normalizes_rendered_help_banner_after_leading_newline() {
    let rendered = normalize_python_help_banner(
        r#"      "text": "\nWelcome to Python 3.12's help utility!\n\nIf this is your first time using Python, you should definitely check out\nhelp> ""#
            .to_string(),
    );

    assert_eq!(rendered, r#"      "text": "<PYTHON HELP BANNER>""#);
}

fn assert_snapshot_or_skip(name: &str, snapshot: &McpSnapshot) -> TestResult<()> {
    let rendered = normalize_python_help_banner(snapshot.render());
    let transcript = normalize_python_help_banner(snapshot.render_transcript());
    if python_backend_unavailable(&rendered) || python_backend_unavailable(&transcript) {
        eprintln!("python help backend unavailable in this environment; skipping");
        return Ok(());
    }

    if cfg!(windows) {
        insta::with_settings!({ snapshot_suffix => "windows" }, {
            insta::assert_snapshot!(name, rendered);
        });
        insta::with_settings!({ snapshot_suffix => "windows-transcript" }, {
            insta::assert_snapshot!(name, transcript);
        });
    } else {
        insta::assert_snapshot!(name, rendered);
        insta::with_settings!({ snapshot_suffix => "transcript" }, {
            insta::assert_snapshot!(name, transcript);
        });
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_help_contract_snapshot() -> TestResult<()> {
    if !common::python_available() {
        eprintln!("python not available; skipping");
        return Ok(());
    }

    let mut snapshot = McpSnapshot::new();
    snapshot
        .python_help_files_session(
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
