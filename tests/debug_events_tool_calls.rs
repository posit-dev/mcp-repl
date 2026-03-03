mod common;

use std::fs;

use common::{TestResult, spawn_server_with_env_vars};
use serde_json::Value;

#[tokio::test]
async fn debug_events_include_tool_call_arguments_and_results() -> TestResult<()> {
    let temp = tempfile::tempdir()?;
    let debug_dir = temp.path().join("events");
    let mut session = spawn_server_with_env_vars(vec![(
        "MCP_REPL_DEBUG_EVENTS_DIR".to_string(),
        debug_dir.to_string_lossy().to_string(),
    )])
    .await?;

    let _ = session.write_stdin_raw_with("1+1", Some(5.0)).await?;
    let _ = session
        .call_tool_raw("repl_reset", serde_json::json!({}))
        .await?;
    session.cancel().await?;

    let mut files = fs::read_dir(&debug_dir)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect::<Vec<_>>();
    files.sort();
    let log_path = files
        .last()
        .cloned()
        .ok_or("missing debug event log file")?;
    let log_text = fs::read_to_string(log_path)?;

    let events = log_text
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()?;

    let saw_repl_begin = events.iter().any(|entry| {
        entry["event"] == "tool_call_begin"
            && entry["payload"]["tool"] == "repl"
            && entry["payload"]["arguments"]["input"] == "1+1\n"
    });
    assert!(
        saw_repl_begin,
        "expected repl tool_call_begin with arguments"
    );

    let saw_repl_end = events.iter().any(|entry| {
        entry["event"] == "tool_call_end"
            && entry["payload"]["tool"] == "repl"
            && entry["payload"]["result"].is_object()
    });
    assert!(
        saw_repl_end,
        "expected repl tool_call_end with serialized result"
    );

    let saw_reset_begin = events.iter().any(|entry| {
        entry["event"] == "tool_call_begin"
            && entry["payload"]["tool"] == "repl_reset"
            && entry["payload"]["arguments"].is_object()
    });
    assert!(
        saw_reset_begin,
        "expected repl_reset tool_call_begin with arguments object"
    );

    let saw_reset_end = events.iter().any(|entry| {
        entry["event"] == "tool_call_end"
            && entry["payload"]["tool"] == "repl_reset"
            && entry["payload"]["result"].is_object()
    });
    assert!(
        saw_reset_end,
        "expected repl_reset tool_call_end with serialized result"
    );

    Ok(())
}
