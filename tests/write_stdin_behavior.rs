#![allow(clippy::await_holding_lock)]

mod common;

use common::TestResult;
use rmcp::model::RawContent;
use std::fs;
use std::path::{Path, PathBuf};
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

fn has_timeout_bundle_dir(temp_root: &std::path::Path) -> TestResult<bool> {
    for entry in fs::read_dir(temp_root)? {
        let entry = entry?;
        let file_name = entry.file_name();
        if !file_name.to_string_lossy().starts_with("mcp-repl-output-") {
            continue;
        }
        if entry.path().join("output-0001").exists() {
            return Ok(true);
        }
    }
    Ok(false)
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

fn r_path_literal(path: &Path) -> TestResult<String> {
    Ok(serde_json::to_string(&path.to_string_lossy().to_string())?)
}

async fn wait_until_path_exists(path: &Path, label: &str) -> TestResult<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if path.exists() {
            return Ok(());
        }
        sleep(Duration::from_millis(20)).await;
    }

    Err(format!("timed out waiting for {label}: {}", path.display()).into())
}

async fn wait_until_not_busy(
    session: &mut common::McpTestSession,
    initial: rmcp::model::CallToolResult,
) -> TestResult<rmcp::model::CallToolResult> {
    let mut result = initial;
    let mut text = result_text(&result);
    if !text.contains("<<repl status: busy") {
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
        if !text.contains("<<repl status: busy") {
            return Ok(result);
        }
    }

    Err(format!("worker remained busy after polling: {text:?}").into())
}

async fn wait_until_file_contains_via_polls(
    session: &mut common::McpTestSession,
    path: &std::path::Path,
    needle: &str,
) -> TestResult<String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last_text = String::new();
    while Instant::now() < deadline {
        match fs::read_to_string(path) {
            Ok(text) => {
                if text.contains(needle) {
                    return Ok(text);
                }
                last_text = text;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }

        let next = session
            .write_stdin_raw_unterminated_with("", Some(0.5))
            .await?;
        let next_text = result_text(&next);
        if let Some(disclosed_path) = bundle_transcript_path(&next_text) {
            assert_eq!(
                disclosed_path, path,
                "did not expect later empty polls to switch transcript paths, got: {next_text:?}"
            );
        }
        sleep(Duration::from_millis(50)).await;
    }

    Err(format!(
        "file did not contain {needle:?} before timeout: {} last contents: {last_text:?}",
        path.display()
    )
    .into())
}

async fn wait_until_busy_follow_up_discloses_transcript_path(
    session: &mut common::McpTestSession,
    input: &str,
    timeout_secs: f64,
) -> TestResult<Option<PathBuf>> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last_text = String::new();
    while Instant::now() < deadline {
        let reply = session
            .write_stdin_raw_with(input, Some(timeout_secs))
            .await?;
        let text = result_text(&reply);
        let is_busy = text.contains("input discarded while worker busy")
            || text.contains("<<repl status: busy");
        if let Some(path) = bundle_transcript_path(&text) {
            if is_busy {
                return Ok(Some(path));
            }
            return Ok(None);
        }
        if !is_busy {
            return Ok(None);
        }
        last_text = text;
        sleep(Duration::from_millis(50)).await;
    }

    Err(
        format!("busy follow-up never disclosed a transcript path before timeout: {last_text:?}")
            .into(),
    )
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
    // These assertions exercise the files-mode public API on every platform.
    common::spawn_server_with_files_env_vars(env_vars).await
}

async fn spawn_pager_behavior_session(page_chars: u64) -> TestResult<common::McpTestSession> {
    common::spawn_server_with_pager_page_chars(page_chars).await
}

#[tokio::test(flavor = "multi_thread")]
async fn write_stdin_discards_when_busy() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let session = spawn_behavior_session().await?;

    let busy_sleep_secs = if cfg!(windows) { 2.0 } else { 0.75 };
    let _ = session
        .write_stdin_raw_with(format!("Sys.sleep({busy_sleep_secs})"), Some(0.1))
        .await?;

    let result = session.write_stdin_raw_with("1+1", Some(0.2)).await?;

    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("write_stdin_behavior backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("input discarded while worker busy") || text.contains("<<repl status: busy"),
        "expected busy discard/timeout message, got: {text:?}"
    );
    assert_ne!(result.is_error, Some(true));

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn write_stdin_echo_prefix_batch() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let mut session = spawn_behavior_session().await?;

    let result = session.write_stdin_raw_with("1+\n1", Some(30.0)).await?;
    let result = wait_until_not_busy(&mut session, result).await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("write_stdin_behavior backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(text.contains("[1] 2"), "expected result, got: {text:?}");
    assert!(
        !text.contains("> 1+"),
        "did not expect echoed first line in trimmed reply, got: {text:?}"
    );
    assert!(
        !text.contains("\n+ 1"),
        "did not expect echoed continuation line in trimmed reply, got: {text:?}"
    );

    let result = session
        .write_stdin_raw_with("echo_trim_x <- 1\necho_trim_x + 1", Some(30.0))
        .await?;
    let result = wait_until_not_busy(&mut session, result).await?;
    let text = result_text(&result);
    assert!(text.contains("[1] 2"), "expected result, got: {text:?}");
    assert!(
        !text.contains("> echo_trim_x <- 1"),
        "did not expect leading assignment echo in trimmed reply, got: {text:?}"
    );
    assert!(
        !text.contains("> echo_trim_x + 1"),
        "did not expect trailing expression echo when the whole prefix is safe to trim, got: {text:?}"
    );

    let result = session
        .write_stdin_raw_with("echo_drop_x <- 1\necho_drop_y <- 2", Some(30.0))
        .await?;
    let result = wait_until_not_busy(&mut session, result).await?;
    let text = result_text(&result);
    assert_eq!(text, "> ", "expected prompt-only reply, got: {text:?}");

    let result = session
        .write_stdin_raw_with("cat('A\\n')\n1+1", Some(30.0))
        .await?;
    let result = wait_until_not_busy(&mut session, result).await?;
    let text = result_text(&result);
    assert!(
        text.contains("A\n"),
        "expected first expression output, got: {text:?}"
    );
    assert!(
        text.contains("[1] 2"),
        "expected second expression result, got: {text:?}"
    );
    assert!(
        !text.contains("> cat('A\\n')"),
        "did not expect the leading echoed prefix to remain, got: {text:?}"
    );
    assert!(
        text.contains("> 1+1"),
        "expected later echoed expression to remain for attribution after output interleaving, got: {text:?}"
    );

    let result = session
        .write_stdin_raw_with(
            "cat('FIRST\\n')\ncat('SECOND\\n')\ncat('THIRD\\n')",
            Some(30.0),
        )
        .await?;
    let result = wait_until_not_busy(&mut session, result).await?;
    let text = result_text(&result);
    assert!(
        text.contains("FIRST\n") && text.contains("SECOND\n") && text.contains("THIRD\n"),
        "expected all expression output, got: {text:?}"
    );
    assert!(
        text.contains("> cat('SECOND\\n')"),
        "expected second submitted expression echo for attribution, got: {text:?}"
    );
    assert!(
        text.contains("> cat('THIRD\\n')"),
        "expected third submitted expression echo for attribution, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[cfg(target_family = "unix")]
#[tokio::test(flavor = "multi_thread")]
async fn write_stdin_preserves_prompt_shaped_child_stdout_before_matching_r_echo() -> TestResult<()>
{
    let _guard = lock_test_mutex();
    let mut session = spawn_behavior_session().await?;

    let result = session
        .write_stdin_raw_with(
            r#"system("printf '> 1+1\\n'")
1+1"#,
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
    assert!(text.contains("[1] 2"), "expected result, got: {text:?}");
    assert!(
        text.matches("> 1+1").count() >= 2,
        "expected raw child stdout and later R echo to both remain visible, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn write_stdin_trims_matched_readline_transcripts() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let mut session = spawn_behavior_session().await?;

    let input = format!(
        "first <- readline('FIRST> '); second <- readline('SECOND> '); big <- paste(rep('z', {OVER_HARD_SPILL_TEXT_LEN}), collapse = ''); cat('DONE_START\\n'); cat(big); cat('\\nDONE_END\\n')"
    );
    let first = session.write_stdin_raw_with(&input, Some(10.0)).await?;
    let first_text = result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("write_stdin_behavior backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    assert!(
        first_text.contains("FIRST> "),
        "expected first readline prompt, got: {first_text:?}"
    );

    let second = session.write_stdin_raw_with("alpha", Some(10.0)).await?;
    let second_text = result_text(&second);
    assert!(
        !second_text.contains("FIRST> alpha"),
        "did not expect matched readline transcript in follow-up reply, got: {second_text:?}"
    );
    assert!(
        second_text.contains("SECOND> "),
        "expected the unmatched second readline prompt after the first answer, got: {second_text:?}"
    );

    let third = session.write_stdin_raw_with("beta", Some(30.0)).await?;
    let third = wait_until_not_busy(&mut session, third).await?;
    let third_text = result_text(&third);
    let transcript_path = bundle_transcript_path(&third_text).unwrap_or_else(|| {
        panic!("expected transcript path in spilled readline reply, got: {third_text:?}")
    });
    let transcript = fs::read_to_string(&transcript_path)?;

    session.cancel().await?;

    assert!(
        !transcript.contains("SECOND> beta"),
        "did not expect matched readline transcript in transcript.txt, got: {transcript:?}"
    );
    assert!(
        transcript.contains("DONE_START") && transcript.contains("DONE_END"),
        "expected spilled worker output in transcript.txt, got: {transcript:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn write_stdin_does_not_treat_colon_input_as_pager_command_by_default() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let session = spawn_behavior_session().await?;

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
    let session = spawn_behavior_session().await?;

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
    let session = spawn_behavior_session().await?;

    let result = session
        .write_stdin_raw_with("cat('> Error: boom\\n'); message('boom')", Some(30.0))
        .await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("write_stdin_behavior backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if text.contains("<<repl status: busy") {
        eprintln!("write_stdin_behavior error prompt output still busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;
    assert!(
        text.contains("Error: boom\n"),
        "missing error text, got: {text:?}"
    );
    assert!(
        !text.contains("> Error: boom\n"),
        "expected leading prompt to be normalized, got: {text:?}"
    );
    assert_ne!(result.is_error, Some(true));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn pager_long_r_stderr_keeps_one_stream_prefix() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let mut session = spawn_pager_behavior_session(40_000).await?;

    let result = session
        .write_stdin_raw_with("message(strrep('x', 20000))", Some(30.0))
        .await?;
    let result = wait_until_not_busy(&mut session, result).await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("write_stdin_behavior backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;

    let prefix_count = text.matches("stderr: ").count();
    assert_eq!(
        prefix_count, 1,
        "expected one stderr prefix for one R stderr write, got {prefix_count}: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn pager_long_r_stderr_after_prior_reply_keeps_one_stream_prefix() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let mut session = spawn_pager_behavior_session(40_000).await?;

    let warmup = session
        .write_stdin_raw_with("cat('warmup\\n')", Some(30.0))
        .await?;
    let warmup = wait_until_not_busy(&mut session, warmup).await?;
    let warmup_text = result_text(&warmup);
    if backend_unavailable(&warmup_text) {
        eprintln!("write_stdin_behavior backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        warmup_text.contains("warmup"),
        "expected warmup reply, got: {warmup_text:?}"
    );

    let result = session
        .write_stdin_raw_with("message(strrep('x', 20000))", Some(30.0))
        .await?;
    let result = wait_until_not_busy(&mut session, result).await?;
    let text = result_text(&result);
    session.cancel().await?;

    let prefix_count = text.matches("stderr: ").count();
    assert_eq!(
        prefix_count, 1,
        "expected one stderr prefix after prior consumed output, got {prefix_count}: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn pager_truncated_r_stderr_keeps_stream_prefix() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let mut session = spawn_pager_behavior_session(40_000).await?;

    let result = session
        .write_stdin_raw_with("message(strrep('x', 6 * 1024 * 1024))", Some(30.0))
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
        text.contains("output truncated"),
        "expected output ring truncation notice, got: {text:?}"
    );
    assert!(
        text.contains("stderr: "),
        "expected truncated stderr tail to keep a visible stderr prefix, got: {text:?}"
    );
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
async fn timeout_output_bundle_is_disclosed_only_after_poll_crosses_hard_spill_threshold()
-> TestResult<()> {
    let _guard = lock_test_mutex();
    let session = spawn_behavior_session().await?;

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
    let spilled = session.write_stdin_raw_with("", Some(2.0)).await?;
    let spilled_text = result_text(&spilled);
    if spilled_text.contains("<<repl status: busy") {
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
async fn follow_up_after_timeout_spills_when_prefix_and_reply_exceed_threshold() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let mut session = spawn_behavior_session().await?;
    let temp = tempdir()?;
    let start_gate_path = temp.path().join("start-ready");
    let done_path = temp.path().join("small-done");
    let start_gate_literal = r_path_literal(&start_gate_path)?;
    let done_literal = r_path_literal(&done_path)?;

    let first_input = format!(
        "small <- paste(rep('s', {UNDER_HARD_SPILL_TEXT_LEN}), collapse = ''); while (!file.exists({start_gate_literal})) Sys.sleep(0.01); cat('SMALL_START\\n'); cat(small); cat('\\nSMALL_END\\n'); flush.console(); writeLines('done', {done_literal})"
    );
    let first = session
        .write_stdin_raw_with(&first_input, Some(test_timeout_secs(0.05, 0.2)))
        .await?;
    let first_text = result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("write_stdin_behavior backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        first_text.contains("<<repl status: busy"),
        "expected the initial under-threshold reply to time out, got: {first_text:?}"
    );
    assert!(
        bundle_transcript_path(&first_text).is_none(),
        "did not expect the initial under-threshold timeout reply to spill, got: {first_text:?}"
    );

    fs::write(&start_gate_path, b"ready")?;
    wait_until_path_exists(&done_path, "small output marker").await?;
    let follow_up_input = format!(
        "fresh <- paste(rep('f', {UNDER_HARD_SPILL_TEXT_LEN}), collapse = ''); cat('FRESH_START\\n'); cat(fresh); cat('\\nFRESH_END\\n')"
    );
    let follow_up = session
        .write_stdin_raw_with(&follow_up_input, Some(2.0))
        .await?;
    let follow_up_text = result_text(&follow_up);
    if follow_up_text.contains("<<repl status: busy") {
        eprintln!("write_stdin_behavior follow-up remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    let transcript_path = bundle_transcript_path(&follow_up_text).unwrap_or_else(|| {
        panic!(
            "expected combined detached timeout output and follow-up reply to spill, got: {follow_up_text:?}"
        )
    });
    let transcript =
        wait_until_file_contains_via_polls(&mut session, &transcript_path, "SMALL_END").await?;

    session.cancel().await?;

    assert!(
        transcript.contains("SMALL_START") && transcript.contains("SMALL_END"),
        "expected the detached timeout prefix in the spilled transcript, got: {transcript:?}"
    );
    assert!(
        follow_up_text.contains("FRESH_START") && follow_up_text.contains("FRESH_END"),
        "expected the fresh follow-up reply to remain visible, got: {follow_up_text:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn busy_follow_up_reuses_hidden_timeout_bundle_when_it_first_spills() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let mut session = spawn_behavior_session().await?;

    let input = format!(
        "small <- paste(rep('s', {UNDER_HARD_SPILL_TEXT_LEN}), collapse = ''); big <- paste(rep('t', {OVER_HARD_SPILL_TEXT_LEN}), collapse = ''); Sys.sleep(0.1); cat('SMALL_START\\n'); cat(small); cat('\\nSMALL_END\\n'); flush.console(); cat('BIG_START\\n'); cat(big); cat('\\nBIG_END\\n'); flush.console(); Sys.sleep(0.6); cat('TAIL\\n')"
    );
    let first = session
        .write_stdin_raw_with(&input, Some(test_timeout_secs(0.005, 0.05)))
        .await?;
    let first_text = result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("write_stdin_behavior backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        bundle_transcript_path(&first_text).is_none(),
        "did not expect timeout bundle disclosure before the busy follow-up, got: {first_text:?}"
    );

    sleep(test_delay_ms(160, 700)).await;
    let Some(transcript_path) = wait_until_busy_follow_up_discloses_transcript_path(
        &mut session,
        "1+1",
        test_timeout_secs(0.1, 0.3),
    )
    .await?
    else {
        eprintln!("write_stdin_behavior busy follow-up completed without a busy marker; skipping");
        session.cancel().await?;
        return Ok(());
    };
    let spilled_text = fs::read_to_string(&transcript_path)?;

    assert!(
        spilled_text.contains("SMALL_START") && spilled_text.contains("SMALL_END"),
        "expected spilled transcript to backfill the earlier timeout text, got: {spilled_text:?}"
    );
    assert!(
        spilled_text.contains("BIG_START") && spilled_text.contains("BIG_END"),
        "expected busy follow-up spill to include the later oversized worker text, got: {spilled_text:?}"
    );
    assert!(
        !spilled_text.contains("input discarded while worker busy")
            && !spilled_text.contains("<<repl status: busy"),
        "did not expect busy marker text inside the worker transcript, got: {spilled_text:?}"
    );

    let final_poll = session.write_stdin_raw_with("", Some(0.1)).await?;
    let final_poll = wait_until_not_busy(&mut session, final_poll).await?;
    let final_text = result_text(&final_poll);
    if final_text.contains("<<repl status: busy") {
        eprintln!("write_stdin_behavior final poll remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    let final_transcript =
        wait_until_file_contains_via_polls(&mut session, &transcript_path, "TAIL").await?;

    session.cancel().await?;

    assert!(
        final_transcript.contains("TAIL"),
        "expected the original timeout bundle to receive the final tail text, got: {final_transcript:?}"
    );
    assert!(
        bundle_transcript_path(&final_text).is_none(),
        "did not expect the settled poll to switch to a different transcript path, got: {final_text:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn pager_busy_follow_up_reuses_hidden_timeout_bundle_when_it_first_spills() -> TestResult<()>
{
    let _guard = lock_test_mutex();
    let mut session = spawn_pager_behavior_session(20_000).await?;

    let input = format!(
        "small <- paste(rep('s', {UNDER_HARD_SPILL_TEXT_LEN}), collapse = ''); big <- paste(rep('t', {OVER_HARD_SPILL_TEXT_LEN}), collapse = ''); Sys.sleep(0.1); cat('SMALL_START\\n'); cat(small); cat('\\nSMALL_END\\n'); flush.console(); cat('BIG_START\\n'); cat(big); cat('\\nBIG_END\\n'); flush.console(); Sys.sleep(0.6); cat('TAIL\\n')"
    );
    let first = session
        .write_stdin_raw_with(&input, Some(test_timeout_secs(0.005, 0.05)))
        .await?;
    let first_text = result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("write_stdin_behavior backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        bundle_transcript_path(&first_text).is_none(),
        "did not expect timeout bundle disclosure before the pager busy follow-up, got: {first_text:?}"
    );

    sleep(test_delay_ms(160, 700)).await;
    let Some(transcript_path) = wait_until_busy_follow_up_discloses_transcript_path(
        &mut session,
        "1+1",
        test_timeout_secs(0.1, 0.3),
    )
    .await?
    else {
        eprintln!(
            "write_stdin_behavior pager busy follow-up completed without a busy marker; skipping"
        );
        session.cancel().await?;
        return Ok(());
    };
    let spilled_text = fs::read_to_string(&transcript_path)?;

    assert!(
        spilled_text.contains("SMALL_START") && spilled_text.contains("SMALL_END"),
        "expected pager spill transcript to backfill the earlier timeout text, got: {spilled_text:?}"
    );
    assert!(
        spilled_text.contains("BIG_START") && spilled_text.contains("BIG_END"),
        "expected pager busy follow-up spill to include the later oversized worker text, got: {spilled_text:?}"
    );
    assert!(
        !spilled_text.contains("input discarded while worker busy")
            && !spilled_text.contains("<<repl status: busy"),
        "did not expect pager busy marker text inside the worker transcript, got: {spilled_text:?}"
    );

    let final_poll = session.write_stdin_raw_with("", Some(0.1)).await?;
    let final_poll = wait_until_not_busy(&mut session, final_poll).await?;
    let final_text = result_text(&final_poll);
    if final_text.contains("<<repl status: busy") {
        eprintln!("write_stdin_behavior pager final poll remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    let final_transcript =
        wait_until_file_contains_via_polls(&mut session, &transcript_path, "TAIL").await?;

    session.cancel().await?;

    assert!(
        final_transcript.contains("TAIL"),
        "expected the original pager timeout bundle to receive the final tail text, got: {final_transcript:?}"
    );
    if let Some(path) = bundle_transcript_path(&final_text) {
        assert_eq!(
            path, transcript_path,
            "did not expect the settled pager poll to switch to a different transcript path, got: {final_text:?}"
        );
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn timeout_spill_file_path_stays_stable_across_later_small_poll() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let session = spawn_behavior_session().await?;
    let temp = tempdir()?;
    let start_gate_path = temp.path().join("start-ready");
    let mid_ready_path = temp.path().join("mid-ready");
    let tail_gate_path = temp.path().join("tail-ready");
    let tail_done_path = temp.path().join("tail-done");
    let start_gate_literal = r_path_literal(&start_gate_path)?;
    let mid_ready_literal = r_path_literal(&mid_ready_path)?;
    let tail_gate_literal = r_path_literal(&tail_gate_path)?;
    let tail_done_literal = r_path_literal(&tail_done_path)?;

    let input = format!(
        "big <- paste(rep('y', 120), collapse = ''); while (!file.exists({start_gate_literal})) Sys.sleep(0.01); cat('start\\n'); flush.console(); for (i in 1:80) cat(sprintf('mid%03d %s\\n', i, big)); flush.console(); writeLines('ready', {mid_ready_literal}); while (!file.exists({tail_gate_literal})) Sys.sleep(0.01); cat('tail\\n'); flush.console(); writeLines('done', {tail_done_literal})"
    );
    let first = session.write_stdin_raw_with(input, Some(0.05)).await?;
    let first_text = result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("write_stdin_behavior backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        first_text.contains("<<repl status: busy"),
        "expected the initial gated request to time out, got: {first_text:?}"
    );

    fs::write(&start_gate_path, b"ready")?;
    wait_until_path_exists(&mid_ready_path, "mid output marker").await?;
    let spilled = session.write_stdin_raw_with("", Some(0.1)).await?;
    let spilled_text = result_text(&spilled);
    let transcript_path = match bundle_transcript_path(&spilled_text) {
        Some(path) => path,
        None if spilled_text.contains("<<repl status: busy") => {
            eprintln!("write_stdin_behavior spill poll remained busy; skipping");
            session.cancel().await?;
            return Ok(());
        }
        None => {
            panic!("expected transcript path in first oversized poll reply, got: {spilled_text:?}")
        }
    };

    fs::write(&tail_gate_path, b"ready")?;
    wait_until_path_exists(&tail_done_path, "tail output marker").await?;
    let final_poll = session.write_stdin_raw_with("", Some(2.0)).await?;
    let final_text = result_text(&final_poll);
    if final_text.contains("<<repl status: busy") {
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
        final_text.contains("tail") || final_text.contains("<<repl status: idle>>"),
        "expected later small poll to either return inline tail text or settle idle after appending to the existing spill file, got: {final_text:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn timeout_spill_recreates_deleted_transcript_without_replaying_old_text() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let mut session = spawn_behavior_session().await?;
    let tail_gate = tempdir()?;
    let tail_gate_path = tail_gate.path().join("tail-ready");
    let tail_gate_literal = serde_json::to_string(&tail_gate_path.to_string_lossy())?;

    let input = format!(
        "big <- paste(rep('y', 120), collapse = ''); cat('start\\n'); flush.console(); Sys.sleep(0.1); for (i in 1:80) cat(sprintf('mid%03d %s\\n', i, big)); flush.console(); while (!file.exists({tail_gate_literal})) Sys.sleep(0.05); cat('tail\\n')"
    );
    let first = session.write_stdin_raw_with(input, Some(0.05)).await?;
    let first_text = result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("write_stdin_behavior backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    sleep(test_delay_ms(160, 260)).await;
    let spilled = session.write_stdin_raw_with("", Some(0.1)).await?;
    let spilled_text = result_text(&spilled);
    let transcript_path = match bundle_transcript_path(&spilled_text) {
        Some(path) => path,
        None if spilled_text.contains("<<repl status: busy") => {
            eprintln!("write_stdin_behavior spill poll remained busy; skipping");
            session.cancel().await?;
            return Ok(());
        }
        None => {
            panic!("expected transcript path in first oversized poll reply, got: {spilled_text:?}")
        }
    };
    let spilled_before_delete =
        wait_until_file_contains_via_polls(&mut session, &transcript_path, "mid080").await?;
    assert!(
        !spilled_before_delete.contains("tail"),
        "did not expect tail before test releases the R-side gate, got: {spilled_before_delete:?}"
    );

    fs::remove_file(&transcript_path)?;
    fs::write(&tail_gate_path, b"ready")?;

    let final_poll = session.write_stdin_raw_with("", Some(2.0)).await?;
    let final_text = result_text(&final_poll);
    if final_text.contains("<<repl status: busy") {
        eprintln!("write_stdin_behavior final poll remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    let recreated_transcript = fs::read_to_string(&transcript_path)?;

    let follow_up = session.write_stdin_raw_with("1+1", Some(2.0)).await?;
    let follow_up_text = result_text(&follow_up);

    session.cancel().await?;

    if let Some(path) = bundle_transcript_path(&final_text) {
        assert_eq!(
            path, transcript_path,
            "did not expect later polls to switch transcript paths after transcript deletion, got: {final_text:?}"
        );
    }
    assert!(
        recreated_transcript.contains("tail"),
        "expected later small poll output to recreate the deleted spill file, got: {recreated_transcript:?}"
    );
    assert!(
        !recreated_transcript.contains("mid080"),
        "did not expect earlier spilled text to be replayed after transcript deletion, got: {recreated_transcript:?}"
    );
    assert!(
        final_text.contains("tail") || final_text.contains("<<repl status: idle>>"),
        "expected later small poll to either return inline tail text or settle idle after recreating the spill file, got: {final_text:?}"
    );
    assert!(
        follow_up_text.contains("[1] 2"),
        "expected session to stay alive after transcript deletion, got: {follow_up_text:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn timeout_bundle_file_creation_failure_preserves_inline_content() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let temp = tempdir()?;
    let session =
        spawn_behavior_session_with_env_vars(output_bundle_temp_env_vars(temp.path())).await?;

    let input = "big <- paste(rep('z', 120), collapse = ''); cat('start\\n'); flush.console(); Sys.sleep(0.1); for (i in 1:80) cat(sprintf('mid%03d %s\\n', i, big)); flush.console(); Sys.sleep(0.1); cat('end\\n')";
    let first = session.write_stdin_raw_with(input, Some(0.05)).await?;
    let first_text = result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("write_stdin_behavior backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    fs::remove_dir_all(temp.path())?;

    sleep(test_delay_ms(160, 600)).await;
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
    let session =
        spawn_behavior_session_with_env_vars(output_bundle_temp_env_vars(temp.path())).await?;

    let first = session
        .write_stdin_raw_with(
            "cat('start\\n'); flush.console(); Sys.sleep(0.1); cat('end\\n')",
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

    assert!(
        !has_timeout_bundle_dir(temp.path())?,
        "did not expect a hidden timeout bundle directory before disclosure"
    );

    sleep(test_delay_ms(160, 600)).await;
    let final_poll = session.write_stdin_raw_with("", Some(2.0)).await?;
    let final_text = result_text(&final_poll);
    if final_text.contains("<<repl status: busy") {
        eprintln!("write_stdin_behavior final inline poll remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }

    session.cancel().await?;

    assert!(
        final_text.contains("end"),
        "expected final worker output inline, got: {final_text:?}"
    );
    assert!(
        bundle_transcript_path(&final_text).is_none(),
        "did not expect hidden timeout bundle disclosure on final inline poll, got: {final_text:?}"
    );
    assert!(
        !has_timeout_bundle_dir(temp.path())?,
        "did not expect a timeout bundle directory when the request finished inline"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn timeout_bundle_stops_before_ctrl_d_restart_output() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let session = spawn_behavior_session().await?;

    let input = "big <- paste(rep('q', 120), collapse = ''); cat('start\\n'); flush.console(); Sys.sleep(0.1); for (i in 1:80) cat(sprintf('mid%03d %s\\n', i, big)); flush.console(); Sys.sleep(30); cat('tail\\n')";
    let first = session.write_stdin_raw_with(input, Some(0.05)).await?;
    let first_text = result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("write_stdin_behavior backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    sleep(test_delay_ms(160, 260)).await;
    let spilled = session.write_stdin_raw_with("", Some(0.1)).await?;
    let spilled_text = result_text(&spilled);
    let transcript_path = match bundle_transcript_path(&spilled_text) {
        Some(path) => path,
        None if spilled_text.contains("<<repl status: busy") => {
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
    if restart_text.contains("<<repl status: busy") {
        eprintln!("write_stdin_behavior ctrl-d restart did not complete in time; skipping");
        session.cancel().await?;
        return Ok(());
    }

    sleep(Duration::from_millis(100)).await;
    let transcript_after = fs::read_to_string(&transcript_path)?;

    session.cancel().await?;

    assert!(
        restart_text.contains("new session started"),
        "expected ctrl-d inline reply to keep the restart notice, got: {restart_text:?}"
    );
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

#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread")]
async fn ctrl_c_follow_up_keeps_detached_tail_out_of_fresh_reply_bundle() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let mut session = spawn_behavior_session().await?;

    let input = format!(
        "small <- paste(rep('s', {UNDER_HARD_SPILL_TEXT_LEN}), collapse = ''); detached <- paste(rep('d', {OVER_HARD_SPILL_TEXT_LEN}), collapse = ''); cat('SMALL_START\\n'); cat(small); cat('\\nSMALL_END\\n'); flush.console(); tryCatch({{ Sys.sleep(30) }}, interrupt = function(e) {{ cat('DETACHED_START\\n'); cat(detached); cat('\\nDETACHED_END\\n'); flush.console() }})"
    );
    let first = session.write_stdin_raw_with(&input, Some(0.05)).await?;
    let first_text = result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("write_stdin_behavior backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        bundle_transcript_path(&first_text).is_none(),
        "did not expect timeout bundle disclosure before the ctrl-c follow-up, got: {first_text:?}"
    );

    sleep(test_delay_ms(160, 700)).await;
    let follow_up = session
        .write_stdin_raw_with("\u{3}cat('NEW_TURN\\n')", Some(10.0))
        .await?;
    let follow_up_text = result_text(&follow_up);
    if follow_up_text.contains("<<repl status: busy") {
        eprintln!("write_stdin_behavior ctrl-c follow-up did not complete in time; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let disclosed_path = bundle_transcript_path(&follow_up_text).unwrap_or_else(|| {
        panic!(
            "expected detached prefix to disclose a timeout bundle path, got: {follow_up_text:?}"
        )
    });
    let transcript =
        wait_until_file_contains_via_polls(&mut session, &disclosed_path, "DETACHED_END").await?;

    session.cancel().await?;

    assert!(
        follow_up_text.contains("NEW_TURN"),
        "expected fresh follow-up output inline, got: {follow_up_text:?}"
    );
    assert!(
        transcript.contains("SMALL_START") && transcript.contains("SMALL_END"),
        "expected the timeout bundle to preserve the earlier timed-out output, got: {transcript:?}"
    );
    assert!(
        transcript.contains("DETACHED_START") && transcript.contains("DETACHED_END"),
        "expected detached interrupt output on the timeout bundle path, got: {transcript:?}"
    );
    assert!(
        !transcript.contains("NEW_TURN"),
        "did not expect fresh follow-up output to append to the timeout bundle: {transcript:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn disclosed_timeout_bundle_keeps_appending_after_busy_follow_up() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let session = spawn_behavior_session().await?;

    let tail_sleep_secs = if cfg!(windows) { 1.0 } else { 0.6 };
    let input = format!(
        "big <- paste(rep('d', {OVER_HARD_SPILL_TEXT_LEN}), collapse = ''); cat('BIG_START\\n'); cat(big); cat('\\nBIG_END\\n'); flush.console(); Sys.sleep({tail_sleep_secs}); cat('TAIL\\n')"
    );
    let first = session.write_stdin_raw_with(&input, Some(0.05)).await?;
    let first_text = result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("write_stdin_behavior backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    sleep(test_delay_ms(160, 700)).await;
    let spilled = session.write_stdin_raw_with("", Some(0.1)).await?;
    let spilled_text = result_text(&spilled);
    let transcript_path = match bundle_transcript_path(&spilled_text) {
        Some(path) => path,
        None if spilled_text.contains("<<repl status: busy") => {
            eprintln!("write_stdin_behavior spill poll remained busy; skipping");
            session.cancel().await?;
            return Ok(());
        }
        None => {
            panic!("expected transcript path in oversized timeout poll, got: {spilled_text:?}")
        }
    };

    let busy_follow_up = session.write_stdin_raw_with("1+1", Some(0.1)).await?;
    let busy_text = result_text(&busy_follow_up);
    if !busy_text.contains("input discarded while worker busy")
        && !busy_text.contains("<<repl status: busy")
    {
        eprintln!("write_stdin_behavior busy follow-up completed without a busy marker; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let final_poll = session.write_stdin_raw_with("", Some(2.0)).await?;
    let final_text = result_text(&final_poll);
    if final_text.contains("<<repl status: busy") {
        eprintln!("write_stdin_behavior final poll remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    let transcript = fs::read_to_string(&transcript_path)?;

    session.cancel().await?;

    assert!(
        transcript.contains("TAIL"),
        "expected the disclosed timeout bundle to keep receiving later worker output after a busy follow-up, got: {transcript:?}"
    );
    assert!(
        bundle_transcript_path(&final_text).is_none(),
        "did not expect the settled poll to switch to a different transcript path, got: {final_text:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn disclosed_timeout_bundle_keeps_appending_after_idle_busy_follow_up() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let session = spawn_behavior_session().await?;

    let tail_sleep_secs = if cfg!(windows) { 1.5 } else { 0.9 };
    let input = format!(
        "big <- paste(rep('i', {OVER_HARD_SPILL_TEXT_LEN}), collapse = ''); cat('BIG_START\\n'); cat(big); cat('\\nBIG_END\\n'); flush.console(); Sys.sleep({tail_sleep_secs}); cat('TAIL\\n')"
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

    sleep(test_delay_ms(160, 700)).await;
    let spilled = session
        .write_stdin_raw_with("", Some(test_timeout_secs(0.1, 0.3)))
        .await?;
    let spilled_text = result_text(&spilled);
    let transcript_path = match bundle_transcript_path(&spilled_text) {
        Some(path) => path,
        None if spilled_text.contains("<<repl status: busy") => {
            eprintln!("write_stdin_behavior spill poll remained busy; skipping");
            session.cancel().await?;
            return Ok(());
        }
        None => {
            panic!("expected transcript path in oversized timeout poll, got: {spilled_text:?}")
        }
    };

    sleep(test_delay_ms(180, 600)).await;
    let busy_follow_up = session
        .write_stdin_raw_with("1+1", Some(test_timeout_secs(0.05, 0.2)))
        .await?;
    let busy_text = result_text(&busy_follow_up);
    if !busy_text.contains("input discarded while worker busy")
        && !busy_text.contains("<<repl status: busy")
    {
        eprintln!("write_stdin_behavior busy follow-up completed without a busy marker; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let final_poll = session.write_stdin_raw_with("", Some(2.0)).await?;
    let final_text = result_text(&final_poll);
    if final_text.contains("<<repl status: busy") {
        eprintln!("write_stdin_behavior final poll remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    let transcript = fs::read_to_string(&transcript_path)?;

    session.cancel().await?;

    assert!(
        transcript.contains("TAIL"),
        "expected the disclosed timeout bundle to keep receiving later worker output after a silent busy follow-up, got: {transcript:?}"
    );
    assert!(
        bundle_transcript_path(&final_text).is_none(),
        "did not expect the settled poll to switch to a different transcript path, got: {final_text:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn files_empty_poll_after_resolved_timeout_restores_prompt() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let session = spawn_behavior_session().await?;

    let first = session
        .write_stdin_raw_with("Sys.sleep(0.1); 1+1", Some(0.05))
        .await?;
    let first_text = result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("write_stdin_behavior backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    sleep(test_delay_ms(160, 700)).await;
    let follow_up = session
        .write_stdin_raw_unterminated_with("", Some(2.0))
        .await?;
    let follow_up_text = result_text(&follow_up);

    session.cancel().await?;

    assert!(
        !follow_up_text.contains("<<repl status: busy"),
        "expected the empty poll to return the settled result, got: {follow_up_text:?}"
    );
    assert!(
        follow_up_text.contains("[1] 2"),
        "expected the settled timeout result, got: {follow_up_text:?}"
    );
    assert!(
        follow_up_text.contains(">"),
        "expected the restored prompt after the settled files-mode poll, got: {follow_up_text:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn pager_follow_up_after_resolved_timeout_trims_detached_echo_prefix() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let session = spawn_pager_behavior_session(20_000).await?;
    let temp = tempdir()?;
    let start_gate_path = temp.path().join("start-ready");
    let done_path = temp.path().join("pager-done");
    let start_gate_literal = r_path_literal(&start_gate_path)?;
    let done_literal = r_path_literal(&done_path)?;

    let first_input = format!(
        "while (!file.exists({start_gate_literal})) Sys.sleep(0.01); print(1+1); flush.console(); writeLines('done', {done_literal})"
    );
    let first = session
        .write_stdin_raw_with(&first_input, Some(0.05))
        .await?;
    let first_text = result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("write_stdin_behavior backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        first_text.contains("<<repl status: busy"),
        "expected the initial gated pager request to time out, got: {first_text:?}"
    );

    fs::write(&start_gate_path, b"ready")?;
    wait_until_path_exists(&done_path, "pager output marker").await?;
    let mut follow_up = session.write_stdin_raw_with("3+3", Some(2.0)).await?;
    let mut follow_up_text = String::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let text = result_text(&follow_up);
        follow_up_text.push_str(&text);
        if text.contains("[1] 6") {
            break;
        }
        if Instant::now() >= deadline {
            return Err(
                format!("worker did not run fresh pager follow-up: {follow_up_text:?}").into(),
            );
        }
        sleep(Duration::from_millis(100)).await;
        follow_up = if text.contains("input discarded while worker busy")
            || text.contains("worker is busy")
            || text.contains("request already running")
        {
            session.write_stdin_raw_with("3+3", Some(2.0)).await?
        } else {
            session
                .write_stdin_raw_unterminated_with("", Some(2.0))
                .await?
        };
    }

    session.cancel().await?;

    assert!(
        follow_up_text.contains("[1] 2"),
        "expected the settled timeout result to be prefixed into the next pager reply, got: {follow_up_text:?}"
    );
    assert!(
        follow_up_text.contains("[1] 6"),
        "expected the fresh pager follow-up result, got: {follow_up_text:?}"
    );
    assert!(
        !follow_up_text.contains("file.exists(") && !follow_up_text.contains("print(1+1)"),
        "did not expect the timed-out request echo to leak into the next pager reply, got: {follow_up_text:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn timeout_bundle_stops_before_fresh_follow_up_output() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let session = spawn_behavior_session().await?;
    let temp = tempdir()?;
    let start_gate_path = temp.path().join("start-ready");
    let mid_ready_path = temp.path().join("mid-ready");
    let tail_gate_path = temp.path().join("tail-ready");
    let tail_done_path = temp.path().join("tail-done");
    let start_gate_literal = r_path_literal(&start_gate_path)?;
    let mid_ready_literal = r_path_literal(&mid_ready_path)?;
    let tail_gate_literal = r_path_literal(&tail_gate_path)?;
    let tail_done_literal = r_path_literal(&tail_done_path)?;

    let input = format!(
        "big <- paste(rep('n', 120), collapse = ''); while (!file.exists({start_gate_literal})) Sys.sleep(0.01); cat('start\\n'); flush.console(); for (i in 1:80) cat(sprintf('mid%03d %s\\n', i, big)); flush.console(); writeLines('ready', {mid_ready_literal}); while (!file.exists({tail_gate_literal})) Sys.sleep(0.01); cat('tail\\n'); flush.console(); writeLines('done', {tail_done_literal})"
    );
    let first = session.write_stdin_raw_with(input, Some(0.05)).await?;
    let first_text = result_text(&first);
    if backend_unavailable(&first_text) {
        eprintln!("write_stdin_behavior backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        first_text.contains("<<repl status: busy"),
        "expected the initial gated request to time out, got: {first_text:?}"
    );

    fs::write(&start_gate_path, b"ready")?;
    wait_until_path_exists(&mid_ready_path, "mid output marker").await?;
    let spilled = session
        .write_stdin_raw_unterminated_with("", Some(0.1))
        .await?;
    let spilled_text = result_text(&spilled);
    let transcript_path = match bundle_transcript_path(&spilled_text) {
        Some(path) => path,
        None if spilled_text.contains("<<repl status: busy") => {
            eprintln!("write_stdin_behavior spill poll remained busy; skipping");
            session.cancel().await?;
            return Ok(());
        }
        None => {
            panic!("expected transcript path in oversized timeout poll, got: {spilled_text:?}")
        }
    };
    let transcript_before = fs::read_to_string(&transcript_path)?;

    fs::write(&tail_gate_path, b"ready")?;
    wait_until_path_exists(&tail_done_path, "tail output marker").await?;
    let follow_up = session
        .write_stdin_raw_with("cat('NEW_TURN\\n')", Some(2.0))
        .await?;
    let follow_up_text = result_text(&follow_up);
    if follow_up_text.contains("<<repl status: busy") {
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
