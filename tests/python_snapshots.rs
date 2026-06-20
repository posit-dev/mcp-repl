mod common;

use common::{McpSnapshot, TestResult};

fn assert_snapshot_or_skip(name: &str, snapshot: &McpSnapshot) -> TestResult<()> {
    let rendered = snapshot.render();
    let transcript = snapshot.render_transcript();
    if common::backend_unavailable(&rendered) || common::backend_unavailable(&transcript) {
        eprintln!("python snapshot backend unavailable in this environment; skipping");
        return Ok(());
    }

    insta::assert_snapshot!(name, rendered);
    insta::with_settings!({ snapshot_suffix => "transcript" }, {
        insta::assert_snapshot!(name, transcript);
    });
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshots_buffered_input_prompt_matching_primary_prompt() -> TestResult<()> {
    if !common::python_available() {
        eprintln!("python not available; skipping");
        return Ok(());
    }

    let mut snapshot = McpSnapshot::new();
    snapshot
        .python_files_session(
            "input_prompt_matching_ps1",
            mcp_script! {
                write_stdin(
                    r#"import sys
sys.ps1 = "same> "
value = input("same> ")
print("MATCHED_PROMPT_VALUE", value)
"#,
                    timeout = 5.0
                );
                write_stdin("buffered", timeout = 5.0);
            },
        )
        .await?;

    assert_snapshot_or_skip(
        "snapshots_buffered_input_prompt_matching_primary_prompt",
        &snapshot,
    )
}
