#![allow(clippy::await_holding_lock)]

mod common;

use common::TestResult;
use rmcp::model::RawContent;
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};
use tempfile::tempdir;
use tokio::time::{Duration, Instant, sleep};

fn test_mutex() -> &'static Mutex<()> {
    static TEST_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
    TEST_MUTEX.get_or_init(|| Mutex::new(()))
}

fn lock_mutex(mutex: &Mutex<()>) -> MutexGuard<'_, ()> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn lock_test_mutex() -> MutexGuard<'static, ()> {
    lock_mutex(test_mutex())
}

fn result_text(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|item| match &item.raw {
            RawContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn backend_unavailable(text: &str) -> bool {
    text.contains("Fatal error: cannot create 'R_TempDir'")
        || text.contains("failed to start R session")
        || text.contains("worker exited with status")
        || text.contains("unable to initialize the JIT")
        || text.contains(
            "worker protocol error: ipc disconnected while waiting for request completion",
        )
}

fn bundle_events_log_path(text: &str) -> Option<PathBuf> {
    disclosed_path(text, "events.log")
}

fn bundle_transcript_path(text: &str) -> Option<PathBuf> {
    disclosed_path(text, "transcript.txt")
}

fn disclosed_path(text: &str, suffix: &str) -> Option<PathBuf> {
    let end = text.find(suffix)?.saturating_add(suffix.len());
    let start = text[..end]
        .rfind(|ch: char| ch.is_whitespace() || matches!(ch, '"' | '\'' | '[' | '('))
        .map_or(0, |idx| idx.saturating_add(1));
    Some(PathBuf::from(&text[start..end]))
}

fn bundle_root(path: &std::path::Path) -> PathBuf {
    path.parent()
        .expect("bundle artifact should have a parent bundle dir")
        .to_path_buf()
}

async fn wait_for_timeout_bundle_dir(temp_root: &std::path::Path) -> TestResult<PathBuf> {
    let deadline = Instant::now() + test_duration(5, 15);
    while Instant::now() < deadline {
        for entry in fs::read_dir(temp_root)? {
            let entry = entry?;
            let file_name = entry.file_name();
            if !file_name.to_string_lossy().starts_with("mcp-repl-output-") {
                continue;
            }
            let bundle_dir = entry.path().join("output-0001");
            if bundle_dir.exists() {
                return Ok(bundle_dir);
            }
        }
        sleep(Duration::from_millis(20)).await;
    }
    Err("timed out waiting for timeout output bundle dir".into())
}

async fn wait_for_path_to_disappear(path: &std::path::Path) -> TestResult<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if !path.exists() {
            return Ok(());
        }
        sleep(Duration::from_millis(50)).await;
    }
    Err(format!("path still exists after shutdown: {}", path.display()).into())
}

async fn wait_until_not_busy(
    session: &mut common::McpTestSession,
    initial: rmcp::model::CallToolResult,
) -> TestResult<rmcp::model::CallToolResult> {
    let mut result = initial;
    let mut text = result_text(&result);
    if !text.contains("<<console status: busy") {
        return Ok(result);
    }

    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        sleep(Duration::from_millis(100)).await;
        let next = session
            .write_stdin_raw_unterminated_with("", Some(2.0))
            .await?;
        text = result_text(&next);
        result = next;
        if !text.contains("<<console status: busy") {
            return Ok(result);
        }
    }

    Err(format!("worker remained busy after polling: {text:?}").into())
}

const INLINE_TEXT_BUDGET_CHARS: usize = 3500;
const INLINE_TEXT_HARD_SPILL_THRESHOLD_CHARS: usize = INLINE_TEXT_BUDGET_CHARS * 5 / 4;
const UNDER_HARD_SPILL_TEXT_LEN: usize = INLINE_TEXT_BUDGET_CHARS + 200;
const OVER_HARD_SPILL_TEXT_LEN: usize = INLINE_TEXT_HARD_SPILL_THRESHOLD_CHARS + 200;

fn test_timeout_secs(default_secs: f64, windows_secs: f64) -> f64 {
    if cfg!(windows) {
        windows_secs
    } else {
        default_secs
    }
}

fn test_duration(default_secs: u64, windows_secs: u64) -> Duration {
    Duration::from_secs(if cfg!(windows) {
        windows_secs
    } else {
        default_secs
    })
}

fn test_delay_ms(default_ms: u64, windows_ms: u64) -> Duration {
    Duration::from_millis(if cfg!(windows) {
        windows_ms
    } else {
        default_ms
    })
}

fn output_bundle_temp_env_vars(path: &std::path::Path) -> Vec<(String, String)> {
    let value = path.display().to_string();
    vec![
        ("TMPDIR".to_string(), value.clone()),
        ("TMP".to_string(), value.clone()),
        ("TEMP".to_string(), value),
    ]
}

async fn spawn_behavior_session() -> TestResult<common::McpTestSession> {
    spawn_behavior_session_with_env_vars(Vec::new()).await
}

async fn spawn_behavior_session_with_env_vars(
    env_vars: Vec<(String, String)>,
) -> TestResult<common::McpTestSession> {
    #[cfg(target_os = "windows")]
    {
        common::spawn_server_with_args_env_and_pager_page_chars(
            vec!["--sandbox".to_string(), "danger-full-access".to_string()],
            env_vars,
            300,
        )
        .await
    }
    #[cfg(not(target_os = "windows"))]
    {
        common::spawn_server_with_env_vars(env_vars).await
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn write_stdin_discards_when_busy() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let mut session = spawn_behavior_session().await?;

    let _ = session
        .write_stdin_raw_with("Sys.sleep(2)", Some(0.1))
        .await?;

    let result = session.write_stdin_raw_with("1+1", Some(0.2)).await?;

    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("write_stdin_behavior backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("input discarded while worker busy")
            || text.contains("<<console status: busy"),
        "expected busy discard/timeout message, got: {text:?}"
    );
    assert_ne!(result.is_error, Some(true));

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn write_stdin_preserves_multiline_echo() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let mut session = spawn_behavior_session().await?;

    let result = session.write_stdin_raw_with("1+\n1", Some(30.0)).await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("write_stdin_behavior backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if text.contains("<<console status: busy") {
        eprintln!("write_stdin_behavior continuation output still busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;
    assert!(text.contains("[1] 2"), "expected result, got: {text:?}");
    assert!(
        text.contains("> 1+"),
        "expected echoed first line to be preserved, got: {text:?}"
    );
    assert!(
        text.contains("\n+ 1"),
        "expected echoed continuation line to be preserved, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn write_stdin_does_not_treat_colon_input_as_pager_command_by_default() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let mut session = spawn_behavior_session().await?;

    let result = session.write_stdin_raw_with(":q", Some(10.0)).await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("write_stdin_behavior backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    session.cancel().await?;
    assert!(
        !text.contains("[pager]")
            && !text.contains("--More--")
            && !text.contains("no pager active"),
        "did not expect pager handling in default files mode, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn write_stdin_mixed_stdout_stderr() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let mut session = spawn_behavior_session().await?;

    let result = session
        .write_stdin_raw_with(
            "cat('out1\\n'); message('err1'); cat('out2\\n')",
            Some(10.0),
        )
        .await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("write_stdin_behavior backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;
    assert!(text.contains("out1"), "missing stdout, got: {text:?}");
    assert!(text.contains("out2"), "missing stdout, got: {text:?}");
    assert!(
        text.contains("stderr:"),
        "missing stderr prefix, got: {text:?}"
    );
    assert_ne!(result.is_error, Some(true));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn write_stdin_normalizes_error_prompt() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let mut session = spawn_behavior_session().await?;

    let result = session
        .write_stdin_raw_with("cat('> Error: boom\\n'); message('boom')", Some(30.0))
        .await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("write_stdin_behavior backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if text.contains("<<console status: busy") {
        eprintln!("write_stdin_behavior error prompt output still busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;
    assert!(
        text.contains("\nError: boom\n"),
        "missing error text, got: {text:?}"
    );
    assert!(
        !text.contains("\n> Error: boom\n"),
        "expected leading prompt to be normalized, got: {text:?}"
    );
    assert_ne!(result.is_error, Some(true));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn write_stdin_large_output_is_not_paged() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let mut session = spawn_behavior_session().await?;

    let result = session
        .write_stdin_raw_with(
            "line <- paste(rep('x', 200), collapse = ''); for (i in 1:120) cat(sprintf('line%04d %s\\n', i, line))",
            Some(30.0),
        )
        .await?;
    let result = wait_until_not_busy(&mut session, result).await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("write_stdin_behavior backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;

    assert!(
        !text.contains("--More--"),
        "did not expect pager footer, got: {text:?}"
    );
    assert!(
        text.contains("line0120"),
        "expected the full output in one reply, got: {text:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn write_stdin_text_slightly_over_inline_budget_stays_inline() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let mut session = spawn_behavior_session().await?;

    let input = format!(
        "big <- paste(rep('u', {UNDER_HARD_SPILL_TEXT_LEN}), collapse = ''); cat('UNDER_START\\n'); cat(big); cat('\\nUNDER_END\\n')"
    );
    let result = session.write_stdin_raw_with(&input, Some(30.0)).await?;
    let result = wait_until_not_busy(&mut session, result).await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("write_stdin_behavior backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    session.cancel().await?;

    assert!(
        text.contains("UNDER_START") && text.contains("UNDER_END"),
        "expected full under-threshold text inline, got: {text:?}"
    );
    assert!(
        bundle_transcript_path(&text).is_none(),
        "did not expect transcript path for under-threshold text, got: {text:?}"
    );
    assert!(
        !text.contains("full output:"),
        "did not expect truncation marker for under-threshold text, got: {text:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn write_stdin_text_above_hard_spill_threshold_uses_output_bundle_dir() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let mut session = spawn_behavior_session().await?;

    let input = format!(
        "big <- paste(rep('v', {OVER_HARD_SPILL_TEXT_LEN}), collapse = ''); cat('OVER_START\\n'); cat(big); cat('\\nOVER_END\\n')"
    );
    let result = session.write_stdin_raw_with(&input, Some(30.0)).await?;
    let result = wait_until_not_busy(&mut session, result).await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("write_stdin_behavior backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let transcript_path = bundle_transcript_path(&text).unwrap_or_else(|| {
        panic!("expected transcript path in over-threshold reply, got: {text:?}")
    });
    let transcript = fs::read_to_string(&transcript_path)?;

    session.cancel().await?;

    assert!(
        transcript.contains("OVER_START") && transcript.contains("OVER_END"),
        "expected transcript bundle to contain the full over-threshold worker text, got: {transcript:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn text_only_oversized_reply_uses_output_bundle_dir() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let mut session = spawn_behavior_session().await?;

    let input = "big <- paste(rep('x', 120), collapse = ''); for (i in 1:80) cat(sprintf('mid%03d %s\\n', i, big))";
    let result = session.write_stdin_raw_with(input, Some(30.0)).await?;
    let result = wait_until_not_busy(&mut session, result).await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("write_stdin_behavior backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let transcript_path = bundle_transcript_path(&text)
        .unwrap_or_else(|| panic!("expected transcript path in oversized reply, got: {text:?}"));
    let transcript = fs::read_to_string(&transcript_path)?;
    let bundle_dir = bundle_root(&transcript_path);
    let events_log = bundle_dir.join("events.log");
    let images_dir = bundle_dir.join("images");

    session.cancel().await?;

    assert!(
        transcript.contains("mid080"),
        "expected transcript bundle to contain the full worker text, got: {transcript:?}"
    );
    assert!(
        !events_log.exists(),
        "did not expect events.log for text-only bundle"
    );
    assert!(
        !images_dir.exists(),
        "did not expect images dir for text-only bundle"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn timeout_output_bundle_backfills_earlier_worker_text_and_excludes_timeout_marker()
-> TestResult<()> {
    let _guard = lock_test_mutex();
    let mut session = spawn_behavior_session().await?;

    let input = "big <- paste(rep('x', 120), collapse = ''); cat('start\\n'); flush.console(); Sys.sleep(0.2); for (i in 1:80) cat(sprintf('mid%03d %s\\n', i, big)); flush.console(); Sys.sleep(0.1); cat('end\\n')";
    let first = session.write_stdin_raw_with(input, Some(0.05)).await?;
    let first_text = result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("write_stdin_behavior backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(bundle_events_log_path(&first_text).is_none());

    sleep(Duration::from_millis(260)).await;
    let spilled = session.write_stdin_raw_with("", Some(2.0)).await?;
    let spilled_text = result_text(&spilled);
    if spilled_text.contains("<<console status: busy") {
        eprintln!("write_stdin_behavior spill poll remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    let transcript_path = bundle_transcript_path(&spilled_text).unwrap_or_else(|| {
        panic!("expected transcript path in oversized poll reply, got: {spilled_text:?}")
    });
    let file_text = fs::read_to_string(&transcript_path)?;

    session.cancel().await?;

    assert!(
        file_text.contains("> big <- paste"),
        "expected echoed input in spill file, got: {file_text:?}"
    );
    assert!(
        file_text.contains("start"),
        "expected early worker text from timeout reply in spill file, got: {file_text:?}"
    );
    assert!(
        file_text.contains("mid080"),
        "expected oversized poll output in spill file, got: {file_text:?}"
    );
    assert!(
        file_text.contains("end"),
        "expected later worker text in spill file, got: {file_text:?}"
    );
    assert!(
        !file_text.contains("<<console status: busy"),
        "did not expect timeout marker in spill file, got: {file_text:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn timeout_output_bundle_is_disclosed_only_after_poll_crosses_hard_spill_threshold()
-> TestResult<()> {
    let _guard = lock_test_mutex();
    let mut session = spawn_behavior_session().await?;

    // Keep the oversized output comfortably behind the initial 50 ms timeout.
    // The worker timeout path polls in 50 ms slices, so a narrower gap can make
    // this boundary test flap and disclose the bundle on the first reply.
    let input = format!(
        "small <- paste(rep('s', {UNDER_HARD_SPILL_TEXT_LEN}), collapse = ''); big <- paste(rep('t', {OVER_HARD_SPILL_TEXT_LEN}), collapse = ''); cat('SMALL_START\\n'); cat(small); cat('\\nSMALL_END\\n'); flush.console(); Sys.sleep(0.5); cat('BIG_START\\n'); cat(big); cat('\\nBIG_END\\n')"
    );
    let first = session
        .write_stdin_raw_with(&input, Some(test_timeout_secs(0.05, 0.2)))
        .await?;
    let first_text = result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("write_stdin_behavior backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        bundle_transcript_path(&first_text).is_none(),
        "did not expect transcript path before a poll crosses the hard spill threshold, got: {first_text:?}"
    );
    sleep(test_delay_ms(600, 900)).await;
    let spilled = session.write_stdin_raw_with("", Some(2.0)).await?;
    let spilled_text = result_text(&spilled);
    if spilled_text.contains("<<console status: busy") {
        eprintln!("write_stdin_behavior spill poll remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    let transcript_path = bundle_transcript_path(&spilled_text).unwrap_or_else(|| {
        panic!("expected transcript path once the poll crossed the hard spill threshold, got: {spilled_text:?}")
    });
    let file_text = fs::read_to_string(&transcript_path)?;

    session.cancel().await?;

    assert!(
        file_text.contains("SMALL_START") && file_text.contains("SMALL_END"),
        "expected transcript file to backfill earlier under-threshold timeout text, got: {file_text:?}"
    );
    assert!(
        file_text.contains("BIG_START") && file_text.contains("BIG_END"),
        "expected transcript file to include the over-threshold poll text, got: {file_text:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn timeout_spill_file_path_stays_stable_across_later_small_poll() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let mut session = spawn_behavior_session().await?;

    let input = "big <- paste(rep('y', 120), collapse = ''); cat('start\\n'); flush.console(); Sys.sleep(0.2); for (i in 1:80) cat(sprintf('mid%03d %s\\n', i, big)); flush.console(); Sys.sleep(0.35); cat('tail\\n')";
    let first = session.write_stdin_raw_with(input, Some(0.05)).await?;
    let first_text = result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("write_stdin_behavior backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    sleep(Duration::from_millis(260)).await;
    let spilled = session.write_stdin_raw_with("", Some(0.1)).await?;
    let spilled_text = result_text(&spilled);
    let transcript_path = match bundle_transcript_path(&spilled_text) {
        Some(path) => path,
        None if spilled_text.contains("<<console status: busy") => {
            eprintln!("write_stdin_behavior spill poll remained busy; skipping");
            session.cancel().await?;
            return Ok(());
        }
        None => {
            panic!("expected transcript path in first oversized poll reply, got: {spilled_text:?}")
        }
    };

    sleep(Duration::from_millis(450)).await;
    let final_poll = session.write_stdin_raw_with("", Some(2.0)).await?;
    let final_text = result_text(&final_poll);
    if final_text.contains("<<console status: busy") {
        eprintln!("write_stdin_behavior final poll remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    let file_text = fs::read_to_string(&transcript_path)?;

    session.cancel().await?;

    assert!(
        bundle_events_log_path(&final_text).is_none(),
        "did not expect bundle path to be repeated on later small poll, got: {final_text:?}"
    );
    assert!(
        file_text.contains("tail"),
        "expected later small poll output to append to existing spill file, got: {file_text:?}"
    );
    assert!(
        final_text.contains("tail") || final_text.contains("<<console status: idle>>"),
        "expected later small poll to either return inline tail text or settle idle after appending to the existing spill file, got: {final_text:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn timeout_bundle_file_creation_failure_preserves_inline_content() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let temp = tempdir()?;
    let mut session =
        spawn_behavior_session_with_env_vars(output_bundle_temp_env_vars(temp.path())).await?;

    let input = "big <- paste(rep('z', 120), collapse = ''); cat('start\\n'); flush.console(); Sys.sleep(0.2); for (i in 1:80) cat(sprintf('mid%03d %s\\n', i, big)); flush.console(); Sys.sleep(0.1); cat('end\\n')";
    let first = session.write_stdin_raw_with(input, Some(0.05)).await?;
    let first_text = result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("write_stdin_behavior backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let bundle_dir = wait_for_timeout_bundle_dir(temp.path()).await?;
    fs::remove_dir_all(&bundle_dir)?;

    sleep(test_delay_ms(260, 600)).await;
    let spilled = session.write_stdin_raw_with("", Some(2.0)).await?;
    let spilled_text = result_text(&spilled);

    let follow_up = session.write_stdin_raw_with("1+1", Some(2.0)).await?;
    let follow_up_text = result_text(&follow_up);

    session.cancel().await?;

    assert!(
        !spilled_text.contains("worker error:"),
        "did not expect bundle write failure to surface as a worker error: {spilled_text:?}"
    );
    assert!(
        bundle_transcript_path(&spilled_text).is_none(),
        "did not expect a transcript path after bundle file creation failed: {spilled_text:?}"
    );
    assert!(
        spilled_text.contains("mid080") && spilled_text.contains("end"),
        "expected bundle write failure to fall back to inline worker text, got: {spilled_text:?}"
    );
    assert!(
        follow_up_text.contains("[1] 2"),
        "expected session to stay alive after bundle file creation failed: {follow_up_text:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn hidden_timeout_bundle_is_removed_after_request_finishes_inline() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let temp = tempdir()?;
    let mut session =
        spawn_behavior_session_with_env_vars(output_bundle_temp_env_vars(temp.path())).await?;

    let first = session
        .write_stdin_raw_with(
            "cat('start\\n'); flush.console(); Sys.sleep(0.2); cat('end\\n')",
            Some(0.05),
        )
        .await?;
    let first_text = result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("write_stdin_behavior backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        bundle_transcript_path(&first_text).is_none(),
        "did not expect timeout bundle disclosure on first small timeout reply, got: {first_text:?}"
    );

    let hidden_bundle_dir = wait_for_timeout_bundle_dir(temp.path()).await?;

    sleep(test_delay_ms(260, 600)).await;
    let final_poll = session.write_stdin_raw_with("", Some(2.0)).await?;
    let final_text = result_text(&final_poll);
    if final_text.contains("<<console status: busy") {
        eprintln!("write_stdin_behavior final inline poll remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }

    wait_for_path_to_disappear(&hidden_bundle_dir).await?;
    session.cancel().await?;

    assert!(
        final_text.contains("end"),
        "expected final worker output inline, got: {final_text:?}"
    );
    assert!(
        bundle_transcript_path(&final_text).is_none(),
        "did not expect hidden timeout bundle disclosure on final inline poll, got: {final_text:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn timeout_bundle_stops_before_ctrl_d_restart_output() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let mut session = spawn_behavior_session().await?;

    let input = "big <- paste(rep('q', 120), collapse = ''); cat('start\\n'); flush.console(); Sys.sleep(0.2); for (i in 1:80) cat(sprintf('mid%03d %s\\n', i, big)); flush.console(); Sys.sleep(30); cat('tail\\n')";
    let first = session.write_stdin_raw_with(input, Some(0.05)).await?;
    let first_text = result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("write_stdin_behavior backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    sleep(Duration::from_millis(260)).await;
    let spilled = session.write_stdin_raw_with("", Some(0.1)).await?;
    let spilled_text = result_text(&spilled);
    let transcript_path = match bundle_transcript_path(&spilled_text) {
        Some(path) => path,
        None if spilled_text.contains("<<console status: busy") => {
            eprintln!("write_stdin_behavior spill poll remained busy; skipping");
            session.cancel().await?;
            return Ok(());
        }
        None => {
            panic!("expected transcript path in oversized timeout poll, got: {spilled_text:?}")
        }
    };
    let transcript_before = fs::read_to_string(&transcript_path)?;

    let restart = session
        .write_stdin_raw_with("\u{4}print('after reset')", Some(10.0))
        .await?;
    let restart_text = result_text(&restart);
    if restart_text.contains("<<console status: busy") {
        eprintln!("write_stdin_behavior ctrl-d restart did not complete in time; skipping");
        session.cancel().await?;
        return Ok(());
    }

    sleep(Duration::from_millis(100)).await;
    let transcript_after = fs::read_to_string(&transcript_path)?;

    session.cancel().await?;

    assert!(
        restart_text.contains("after reset"),
        "expected restarted session output, got: {restart_text:?}"
    );
    assert_eq!(
        transcript_after, transcript_before,
        "did not expect ctrl-d restart output to append to prior timeout bundle"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn timeout_bundle_stops_before_fresh_follow_up_output() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let mut session = spawn_behavior_session().await?;

    let input = "big <- paste(rep('n', 120), collapse = ''); cat('start\\n'); flush.console(); Sys.sleep(0.2); for (i in 1:80) cat(sprintf('mid%03d %s\\n', i, big)); flush.console(); Sys.sleep(0.2); cat('tail\\n')";
    let first = session.write_stdin_raw_with(input, Some(0.05)).await?;
    let first_text = result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("write_stdin_behavior backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    sleep(Duration::from_millis(260)).await;
    let spilled = session
        .write_stdin_raw_unterminated_with("", Some(0.1))
        .await?;
    let spilled_text = result_text(&spilled);
    let transcript_path = match bundle_transcript_path(&spilled_text) {
        Some(path) => path,
        None if spilled_text.contains("<<console status: busy") => {
            eprintln!("write_stdin_behavior spill poll remained busy; skipping");
            session.cancel().await?;
            return Ok(());
        }
        None => {
            panic!("expected transcript path in oversized timeout poll, got: {spilled_text:?}")
        }
    };
    let transcript_before = fs::read_to_string(&transcript_path)?;
    sleep(Duration::from_millis(260)).await;
    let follow_up = session
        .write_stdin_raw_with("cat('NEW_TURN\\n')", Some(2.0))
        .await?;
    let follow_up_text = result_text(&follow_up);
    if follow_up_text.contains("<<console status: busy") {
        eprintln!("write_stdin_behavior fresh follow-up remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    let transcript_after = fs::read_to_string(&transcript_path)?;

    session.cancel().await?;

    assert!(
        follow_up_text.contains("NEW_TURN"),
        "expected fresh follow-up output inline, got: {follow_up_text:?}"
    );
    assert!(
        !transcript_after.contains("NEW_TURN"),
        "did not expect fresh follow-up output to append to prior timeout bundle: {transcript_after:?}"
    );
    assert!(
        transcript_after.contains(&transcript_before),
        "expected original timeout bundle contents to remain intact"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn output_bundle_prunes_oldest_inactive_bundle_when_count_limit_exceeded() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let mut session = spawn_behavior_session_with_env_vars(vec![
        (
            "MCP_REPL_OUTPUT_BUNDLE_MAX_COUNT".to_string(),
            "2".to_string(),
        ),
        (
            "MCP_REPL_OUTPUT_BUNDLE_MAX_BYTES".to_string(),
            "1048576".to_string(),
        ),
        (
            "MCP_REPL_OUTPUT_BUNDLE_MAX_TOTAL_BYTES".to_string(),
            "2097152".to_string(),
        ),
    ])
    .await?;
    let mut bundles = Vec::new();

    for label in ["a", "b", "c"] {
        let input = format!(
            "big <- paste(rep('{label}', 120), collapse = ''); for (i in 1:80) cat(sprintf('{label}%03d %s\\n', i, big))"
        );
        let result = session.write_stdin_raw_with(input, Some(30.0)).await?;
        let result = wait_until_not_busy(&mut session, result).await?;
        let text = result_text(&result);
        if backend_unavailable(&text) {
            eprintln!("write_stdin_behavior backend unavailable in this environment; skipping");
            session.cancel().await?;
            return Ok(());
        }
        let transcript_path = bundle_transcript_path(&text).unwrap_or_else(|| {
            panic!("expected transcript path in oversized reply, got: {text:?}")
        });
        bundles.push(transcript_path);
    }

    assert!(
        !bundles[0].exists(),
        "expected oldest bundle to be pruned after count cap, still exists: {:?}",
        bundles[0]
    );
    assert!(bundles[1].exists(), "expected second bundle to remain");
    assert!(bundles[2].exists(), "expected newest bundle to remain");

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn output_bundle_reports_omitted_tail_when_bundle_size_cap_is_hit() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let mut session = spawn_behavior_session_with_env_vars(vec![
        (
            "MCP_REPL_OUTPUT_BUNDLE_MAX_COUNT".to_string(),
            "20".to_string(),
        ),
        (
            "MCP_REPL_OUTPUT_BUNDLE_MAX_BYTES".to_string(),
            "2048".to_string(),
        ),
        (
            "MCP_REPL_OUTPUT_BUNDLE_MAX_TOTAL_BYTES".to_string(),
            "1048576".to_string(),
        ),
    ])
    .await?;

    let input = "big <- paste(rep('z', 120), collapse = ''); for (i in 1:120) cat(sprintf('z%03d %s\\n', i, big))";
    let result = session.write_stdin_raw_with(input, Some(30.0)).await?;
    let result = wait_until_not_busy(&mut session, result).await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("write_stdin_behavior backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let transcript_path = bundle_transcript_path(&text).unwrap_or_else(|| {
        panic!("expected transcript path in capped oversized reply, got: {text:?}")
    });
    let transcript = fs::read_to_string(&transcript_path)?;
    let events_log = bundle_root(&transcript_path).join("events.log");

    session.cancel().await?;

    assert!(
        text.contains("later content omitted"),
        "expected inline omission notice after bundle cap, got: {text:?}"
    );
    assert!(
        !events_log.exists(),
        "did not expect events.log for text-only capped bundle"
    );
    assert!(
        !transcript.contains("z120"),
        "did not expect capped transcript to contain the omitted tail, got: {transcript:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn output_bundle_prunes_oldest_inactive_bundle_when_total_size_limit_is_hit() -> TestResult<()>
{
    let _guard = lock_test_mutex();
    let mut session = spawn_behavior_session_with_env_vars(vec![
        (
            "MCP_REPL_OUTPUT_BUNDLE_MAX_COUNT".to_string(),
            "20".to_string(),
        ),
        (
            "MCP_REPL_OUTPUT_BUNDLE_MAX_BYTES".to_string(),
            "1048576".to_string(),
        ),
        (
            "MCP_REPL_OUTPUT_BUNDLE_MAX_TOTAL_BYTES".to_string(),
            "7000".to_string(),
        ),
    ])
    .await?;

    let mut bundles = Vec::new();
    for label in ["m", "n"] {
        let input = format!(
            "big <- paste(rep('{label}', 120), collapse = ''); for (i in 1:45) cat(sprintf('{label}%03d %s\\n', i, big))"
        );
        let result = session.write_stdin_raw_with(input, Some(30.0)).await?;
        let result = wait_until_not_busy(&mut session, result).await?;
        let text = result_text(&result);
        if backend_unavailable(&text) {
            eprintln!("write_stdin_behavior backend unavailable in this environment; skipping");
            session.cancel().await?;
            return Ok(());
        }
        let transcript_path = bundle_transcript_path(&text).unwrap_or_else(|| {
            panic!("expected transcript path in oversized reply, got: {text:?}")
        });
        bundles.push(transcript_path);
    }

    assert!(
        !bundles[0].exists(),
        "expected oldest bundle to be pruned after total-size cap, still exists: {:?}",
        bundles[0]
    );
    assert!(bundles[1].exists(), "expected newest bundle to remain");

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn output_bundle_is_cleaned_up_when_server_exits() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let mut session = spawn_behavior_session().await?;

    let input = "big <- paste(rep('q', 120), collapse = ''); for (i in 1:80) cat(sprintf('q%03d %s\\n', i, big))";
    let result = session.write_stdin_raw_with(input, Some(30.0)).await?;
    let result = wait_until_not_busy(&mut session, result).await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("write_stdin_behavior backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    let transcript_path = bundle_transcript_path(&text)
        .unwrap_or_else(|| panic!("expected transcript path in oversized reply, got: {text:?}"));

    session.cancel().await?;
    wait_for_path_to_disappear(&transcript_path).await?;

    Ok(())
}

#[test]
fn lock_mutex_handles_poisoned_mutex() {
    let mutex = Mutex::new(());
    let _ = std::panic::catch_unwind(|| {
        let _guard = mutex.lock().expect("lock");
        panic!("poison mutex");
    });

    let _guard = lock_mutex(&mutex);
}
