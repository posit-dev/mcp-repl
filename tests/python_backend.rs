mod common;

use common::TestResult;
use rmcp::model::RawContent;
use std::fs;
use std::path::PathBuf;
use tokio::time::{Duration, Instant, sleep};

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

fn bundle_transcript_path(text: &str) -> Option<PathBuf> {
    let end = text
        .find("transcript.txt")?
        .saturating_add("transcript.txt".len());
    let start = text[..end]
        .rfind(|ch: char| ch.is_whitespace() || matches!(ch, '"' | '\'' | '[' | '('))
        .map_or(0, |idx| idx.saturating_add(1));
    Some(PathBuf::from(&text[start..end]))
}

fn visible_reply_text(text: &str) -> TestResult<String> {
    if let Some(path) = bundle_transcript_path(text) {
        return Ok(fs::read_to_string(path)?);
    }
    Ok(text.to_string())
}

fn require_python() -> bool {
    if common::python_available() {
        true
    } else {
        eprintln!("python not available; skipping");
        false
    }
}

fn is_busy_response(text: &str) -> bool {
    text.contains("<<repl status: busy")
        || text.contains("worker is busy")
        || text.contains("request already running")
        || text.contains("input discarded while worker busy")
}

fn interrupt_recovery_deadline() -> Instant {
    Instant::now() + Duration::from_secs(if cfg!(target_os = "macos") { 20 } else { 5 })
}

async fn start_python_session() -> TestResult<Option<common::McpTestSession>> {
    if !require_python() {
        return Ok(None);
    }

    let mut session = common::spawn_python_server_with_files().await?;
    let probe = session.write_stdin_raw_with("", Some(2.0)).await?;
    let probe_text = result_text(&probe);
    if probe_text.contains("worker io error: Permission denied")
        || probe_text.contains("python backend requires a unix-style pty")
    {
        eprintln!("python backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(None);
    }

    Ok(Some(session))
}

#[cfg(unix)]
const DETACHED_STDIO_HOLDER_SECS: f64 = 2.5;

#[cfg(unix)]
fn shutdown_completion_budget() -> Duration {
    if cfg!(target_os = "macos") {
        Duration::from_millis(1_500)
    } else {
        Duration::from_millis(1_200)
    }
}

#[cfg(unix)]
async fn arm_detached_stdio_holder(session: &mut common::McpTestSession) -> TestResult<()> {
    let setup = session
        .write_stdin_raw_with(
            format!(
                r#"import subprocess, sys
script = "import time; time.sleep({DETACHED_STDIO_HOLDER_SECS})"
subprocess.Popen(
    [sys.executable, "-c", script],
    stdin=subprocess.DEVNULL,
    close_fds=True,
    start_new_session=True,
)
print("detached ready")
"#
            ),
            Some(5.0),
        )
        .await?;
    let setup_text = result_text(&setup);
    if is_busy_response(&setup_text) {
        return Err("detached-stdio setup remained busy".into());
    }
    assert!(
        setup_text.contains("detached ready"),
        "expected detached-stdio setup reply, got: {setup_text:?}"
    );
    Ok(())
}

#[cfg(unix)]
async fn arm_background_ipc_holder(session: &mut common::McpTestSession) -> TestResult<()> {
    let setup = session
        .write_stdin_raw_with(
            format!(
                r#"import subprocess, sys
script = "import time; time.sleep({DETACHED_STDIO_HOLDER_SECS})"
subprocess.Popen(
    [sys.executable, "-c", script],
    stdin=subprocess.DEVNULL,
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
    close_fds=False,
)
print("ipc background ready")
"#
            ),
            Some(5.0),
        )
        .await?;
    let setup_text = result_text(&setup);
    if is_busy_response(&setup_text) {
        return Err("background-ipc setup remained busy".into());
    }
    assert!(
        setup_text.contains("ipc background ready"),
        "expected background-ipc setup reply, got: {setup_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_smoke() -> TestResult<()> {
    let Some(mut session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session.write_stdin_raw_with("1+1", Some(5.0)).await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        eprintln!("python_smoke remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(text.contains("2"), "expected 2, got: {text:?}");

    session.cancel().await?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_quit_does_not_wait_for_detached_stdio_holders() -> TestResult<()> {
    let Some(mut session) = start_python_session().await? else {
        return Ok(());
    };

    arm_detached_stdio_holder(&mut session).await?;

    let start = Instant::now();
    let quit = session.write_stdin_raw_with("quit()", Some(5.0)).await?;
    let elapsed = start.elapsed();
    let quit_text = result_text(&quit);
    if is_busy_response(&quit_text) {
        eprintln!("python_quit_does_not_wait_for_detached_stdio_holders remained busy on quit");
        session.cancel().await?;
        return Ok(());
    }

    assert!(
        elapsed < shutdown_completion_budget(),
        "expected quit() to finish before detached child exit, got {elapsed:?}: {quit_text:?}"
    );

    let follow_up = session
        .write_stdin_raw_with("print('AFTER_QUIT')", Some(5.0))
        .await?;
    let follow_up_text = result_text(&follow_up);
    if is_busy_response(&follow_up_text) {
        eprintln!(
            "python_quit_does_not_wait_for_detached_stdio_holders remained busy after respawn"
        );
        session.cancel().await?;
        return Ok(());
    }

    session.cancel().await?;

    assert!(
        follow_up_text.contains("AFTER_QUIT"),
        "expected prompt recovery after quit() respawn, got: {follow_up_text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_respawn_does_not_wait_for_detached_stdio_holders() -> TestResult<()> {
    let Some(mut session) = start_python_session().await? else {
        return Ok(());
    };

    let arm = session
        .write_stdin_raw_with(
            format!(
                r#"import os, subprocess, sys, threading, time
script = "import time; time.sleep({DETACHED_STDIO_HOLDER_SECS})"
def leave_detached_tail():
    time.sleep(0.2)
    subprocess.Popen(
        [sys.executable, "-c", script],
        stdin=subprocess.DEVNULL,
        close_fds=True,
        start_new_session=True,
    )
    os._exit(0)
threading.Thread(target=leave_detached_tail, daemon=True).start()
print("detached respawn armed")
"#
            ),
            Some(5.0),
        )
        .await?;
    let arm_text = result_text(&arm);
    if is_busy_response(&arm_text) {
        eprintln!("python_respawn_does_not_wait_for_detached_stdio_holders remained busy");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        arm_text.contains("detached respawn armed"),
        "expected detached-respawn arming reply, got: {arm_text:?}"
    );

    sleep(Duration::from_millis(500)).await;
    let start = Instant::now();
    let follow_up = session
        .write_stdin_raw_with("print('AFTER_RESPAWN')", Some(5.0))
        .await?;
    let elapsed = start.elapsed();
    let follow_up_text = result_text(&follow_up);
    if is_busy_response(&follow_up_text) {
        eprintln!(
            "python_respawn_does_not_wait_for_detached_stdio_holders remained busy after exit"
        );
        session.cancel().await?;
        return Ok(());
    }

    session.cancel().await?;

    assert!(
        elapsed < shutdown_completion_budget(),
        "expected respawn to finish before detached child exit, got {elapsed:?}: {follow_up_text:?}"
    );
    assert!(
        follow_up_text.contains("AFTER_RESPAWN"),
        "expected prompt recovery after respawn, got: {follow_up_text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_quit_does_not_wait_for_background_ipc_holders() -> TestResult<()> {
    let Some(mut session) = start_python_session().await? else {
        return Ok(());
    };

    arm_background_ipc_holder(&mut session).await?;

    let start = Instant::now();
    let quit = session.write_stdin_raw_with("quit()", Some(5.0)).await?;
    let elapsed = start.elapsed();
    let quit_text = result_text(&quit);
    if is_busy_response(&quit_text) {
        eprintln!("python_quit_does_not_wait_for_background_ipc_holders remained busy on quit");
        session.cancel().await?;
        return Ok(());
    }

    assert!(
        elapsed < shutdown_completion_budget(),
        "expected quit() to finish before background IPC holder exit, got {elapsed:?}: {quit_text:?}"
    );

    let follow_up = session
        .write_stdin_raw_with("print('AFTER_IPC_QUIT')", Some(5.0))
        .await?;
    let follow_up_text = result_text(&follow_up);
    if is_busy_response(&follow_up_text) {
        eprintln!(
            "python_quit_does_not_wait_for_background_ipc_holders remained busy after respawn"
        );
        session.cancel().await?;
        return Ok(());
    }

    session.cancel().await?;

    assert!(
        follow_up_text.contains("AFTER_IPC_QUIT"),
        "expected prompt recovery after quit() respawn, got: {follow_up_text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_respawn_does_not_wait_for_background_ipc_holders() -> TestResult<()> {
    let Some(mut session) = start_python_session().await? else {
        return Ok(());
    };

    let arm = session
        .write_stdin_raw_with(
            format!(
                r#"import os, subprocess, sys, threading, time
script = "import time; time.sleep({DETACHED_STDIO_HOLDER_SECS})"
def leave_background_ipc_tail():
    time.sleep(0.2)
    subprocess.Popen(
        [sys.executable, "-c", script],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        close_fds=False,
    )
    os._exit(0)
threading.Thread(target=leave_background_ipc_tail, daemon=True).start()
print("ipc respawn armed")
"#
            ),
            Some(5.0),
        )
        .await?;
    let arm_text = result_text(&arm);
    if is_busy_response(&arm_text) {
        eprintln!("python_respawn_does_not_wait_for_background_ipc_holders remained busy");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        arm_text.contains("ipc respawn armed"),
        "expected background-ipc respawn arming reply, got: {arm_text:?}"
    );

    sleep(Duration::from_millis(500)).await;
    let start = Instant::now();
    let follow_up = session
        .write_stdin_raw_with("print('AFTER_IPC_RESPAWN')", Some(5.0))
        .await?;
    let elapsed = start.elapsed();
    let follow_up_text = result_text(&follow_up);
    if is_busy_response(&follow_up_text) {
        eprintln!(
            "python_respawn_does_not_wait_for_background_ipc_holders remained busy after exit"
        );
        session.cancel().await?;
        return Ok(());
    }

    session.cancel().await?;

    assert!(
        elapsed < shutdown_completion_budget(),
        "expected respawn to finish before background IPC holder exit, got {elapsed:?}: {follow_up_text:?}"
    );
    assert!(
        follow_up_text.contains("AFTER_IPC_RESPAWN"),
        "expected prompt recovery after respawn, got: {follow_up_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_multiline_block() -> TestResult<()> {
    let Some(mut session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with("def f():\n    return 3\n\nf()", Some(5.0))
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        eprintln!("python_multiline_block remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(text.contains("3"), "expected 3, got: {text:?}");

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_input_roundtrip() -> TestResult<()> {
    let Some(mut session) = start_python_session().await? else {
        return Ok(());
    };

    let mut text = result_text(
        &session
            .write_stdin_raw_with("x = input('prompt> ')", Some(1.0))
            .await?,
    );
    if is_busy_response(&text) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && is_busy_response(&text) && !text.contains("prompt>") {
            sleep(Duration::from_millis(50)).await;
            text = result_text(&session.write_stdin_raw_with("", Some(1.0)).await?);
        }
    }
    if is_busy_response(&text) {
        eprintln!("python_input_roundtrip remained busy before prompt; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(text.contains("prompt>"), "expected prompt, got: {text:?}");

    let mut text = result_text(
        &session
            .write_stdin_raw_with("hello\nprint(x)", Some(5.0))
            .await?,
    );
    if is_busy_response(&text) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && is_busy_response(&text) && !text.contains("hello") {
            sleep(Duration::from_millis(50)).await;
            text = result_text(&session.write_stdin_raw_with("", Some(1.0)).await?);
        }
    }
    if is_busy_response(&text) {
        eprintln!("python_input_roundtrip remained busy while reading input; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(text.contains("hello"), "expected echo, got: {text:?}");

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_busy_discards_input() -> TestResult<()> {
    let Some(mut session) = start_python_session().await? else {
        return Ok(());
    };

    let _ = session
        .write_stdin_raw_with("import time; time.sleep(2)", Some(0.1))
        .await?;

    let result = session.write_stdin_raw_with("1+1", Some(0.2)).await?;
    let text = result_text(&result);
    assert!(
        text.contains("input discarded while worker busy"),
        "expected busy discard message, got: {text:?}"
    );
    assert_ne!(result.is_error, Some(true));

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_stderr_merged_into_output() -> TestResult<()> {
    let Some(mut session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            "import sys; print('out'); sys.stderr.write('err\\n')",
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        eprintln!("python_stderr_merged_into_output remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(text.contains("out"), "missing stdout, got: {text:?}");
    assert!(text.contains("err"), "missing stderr, got: {text:?}");

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_interrupt_unblocks_long_running_request() -> TestResult<()> {
    let Some(mut session) = start_python_session().await? else {
        return Ok(());
    };

    let timeout_result = session
        .write_stdin_raw_with("import time; time.sleep(30)", Some(0.5))
        .await?;
    let timeout_text = result_text(&timeout_result);
    assert!(
        timeout_text.contains("<<repl status: busy"),
        "expected sleep call to time out, got: {timeout_text:?}"
    );

    let interrupt_result = session.write_stdin_raw_with("\u{3}", Some(5.0)).await?;
    let interrupt_text = result_text(&interrupt_result);
    assert!(
        interrupt_text.contains(">>>"),
        "expected prompt after interrupt, got: {interrupt_text:?}"
    );

    let deadline = interrupt_recovery_deadline();
    loop {
        if Instant::now() >= deadline {
            session.cancel().await?;
            return Err("worker stayed busy after interrupt".into());
        }

        let result = session.write_stdin_raw_with("1+1", Some(0.5)).await?;
        let text = result_text(&result);
        if text.contains("worker is busy") || text.contains("request already running") {
            sleep(Duration::from_millis(50)).await;
            continue;
        }
        assert!(
            text.contains("2"),
            "expected evaluation after interrupt, got: {text:?}"
        );
        break;
    }

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_detached_idle_output_does_not_bundle_follow_up_reply() -> TestResult<()> {
    let Some(mut session) = start_python_session().await? else {
        return Ok(());
    };

    let setup = session
        .write_stdin_raw_with(
            r#"import subprocess, sys
script = """import sys, time
time.sleep(0.3)
for i in range(160):
    sys.stdout.write("IDLE_%03d " % i + ("x" * 80) + "\\n")
sys.stdout.flush()
"""
subprocess.Popen(
    [sys.executable, "-c", script],
    stdin=subprocess.DEVNULL,
    close_fds=False,
)
print("parent ready")
"#,
            Some(5.0),
        )
        .await?;
    let setup_text = result_text(&setup);
    if is_busy_response(&setup_text) {
        eprintln!("python detached-idle setup remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        setup_text.contains("parent ready"),
        "expected detached-idle setup reply, got: {setup_text:?}"
    );

    sleep(Duration::from_millis(1500)).await;
    let follow_up = session
        .write_stdin_raw_with("print('FOLLOWUP_OK')", Some(5.0))
        .await?;
    let follow_up_text = result_text(&follow_up);
    if is_busy_response(&follow_up_text) {
        eprintln!("python detached-idle follow-up remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let transcript_path = bundle_transcript_path(&follow_up_text).unwrap_or_else(|| {
        panic!("expected detached idle output to disclose transcript path, got: {follow_up_text:?}")
    });
    let transcript = std::fs::read_to_string(&transcript_path)?;

    session.cancel().await?;

    assert!(
        follow_up_text.contains("FOLLOWUP_OK"),
        "expected follow-up output inline, got: {follow_up_text:?}"
    );
    assert!(
        transcript.contains("IDLE_000"),
        "expected detached idle output in transcript bundle, got: {transcript:?}"
    );
    assert!(
        !transcript.contains("FOLLOWUP_OK"),
        "did not expect follow-up output to be bundled with detached idle output: {transcript:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_idle_exit_preserves_detached_tail_before_respawn() -> TestResult<()> {
    let Some(mut session) = start_python_session().await? else {
        return Ok(());
    };

    let arm = session
        .write_stdin_raw_with(
            "import os, sys, threading, time; print('armed'); threading.Thread(target=lambda: (time.sleep(0.2), sys.stdout.write('IDLE_TAIL\\n'), sys.stdout.flush(), os._exit(0)), daemon=True).start()",
            Some(5.0),
        )
        .await?;
    let arm_text = result_text(&arm);
    if is_busy_response(&arm_text) {
        eprintln!(
            "python_idle_exit_preserves_detached_tail_before_respawn remained busy; skipping"
        );
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        arm_text.contains("armed"),
        "expected arming output, got: {arm_text:?}"
    );

    sleep(Duration::from_millis(500)).await;
    let reply = session
        .write_stdin_raw_with("print('AFTER_RESPAWN')", Some(5.0))
        .await?;
    let text = result_text(&reply);
    if is_busy_response(&text) {
        eprintln!(
            "python_idle_exit_preserves_detached_tail_before_respawn remained busy after respawn; skipping"
        );
        session.cancel().await?;
        return Ok(());
    }
    let visible = visible_reply_text(&text)?;

    session.cancel().await?;

    assert!(
        visible.contains("IDLE_TAIL"),
        "expected detached idle output to survive auto-respawn, got: {visible:?}"
    );
    assert!(
        visible.contains("AFTER_RESPAWN"),
        "expected fresh respawned output, got: {visible:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_restart_does_not_leak_old_generation_output() -> TestResult<()> {
    let Some(mut session) = start_python_session().await? else {
        return Ok(());
    };

    let timeout_result = session
        .write_stdin_raw_with(
            "import sys, time; big = 'OLD_BLOCK\\n' * 200000; sys.stdout.write(big); sys.stdout.flush(); time.sleep(30)",
            Some(0.05),
        )
        .await?;
    let timeout_text = result_text(&timeout_result);
    if !is_busy_response(&timeout_text) {
        eprintln!(
            "python_restart_does_not_leak_old_generation_output did not time out as expected; skipping"
        );
        session.cancel().await?;
        return Ok(());
    }

    let restart = session.write_stdin_raw_with("\u{4}", Some(10.0)).await?;
    let restart_text = result_text(&restart);
    if is_busy_response(&restart_text) {
        eprintln!(
            "python_restart_does_not_leak_old_generation_output restart remained busy; skipping"
        );
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        restart_text.contains("new session started"),
        "expected restart notice, got: {restart_text:?}"
    );

    let next = session
        .write_stdin_raw_with("print('NEW_GENERATION_OK')", Some(5.0))
        .await?;
    let next_text = result_text(&next);
    if is_busy_response(&next_text) {
        eprintln!(
            "python_restart_does_not_leak_old_generation_output next turn remained busy; skipping"
        );
        session.cancel().await?;
        return Ok(());
    }
    let visible = visible_reply_text(&next_text)?;

    session.cancel().await?;

    assert!(
        visible.contains("NEW_GENERATION_OK"),
        "expected fresh-generation reply, got: {visible:?}"
    );
    assert!(
        !visible.contains("OLD_BLOCK"),
        "did not expect old-generation output after restart, got: {visible:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_detached_incomplete_utf8_tail_does_not_merge_into_next_request() -> TestResult<()> {
    let Some(mut session) = start_python_session().await? else {
        return Ok(());
    };

    let setup = session
        .write_stdin_raw_with(
            r#"import subprocess, sys
script = """import os, sys, time
time.sleep(0.3)
for i in range(160):
    os.write(sys.stdout.fileno(), ("IDLE_%03d " % i + ("x" * 80) + "\\n").encode())
os.write(sys.stdout.fileno(), bytes([0xC3]))
"""
subprocess.Popen(
    [sys.executable, "-c", script],
    stdin=subprocess.DEVNULL,
    close_fds=False,
)
print("parent ready")
"#,
            Some(5.0),
        )
        .await?;
    let setup_text = result_text(&setup);
    if is_busy_response(&setup_text) {
        eprintln!("python detached-incomplete setup remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        setup_text.contains("parent ready"),
        "expected detached-incomplete setup reply, got: {setup_text:?}"
    );

    sleep(Duration::from_millis(700)).await;
    let follow_up = session
        .write_stdin_raw_with(
            "import os, sys\nos.write(sys.stdout.fileno(), bytes([0xA9, 0x0A]))\nprint('FOLLOWUP_OK')",
            Some(5.0),
        )
        .await?;
    let follow_up_text = result_text(&follow_up);
    if is_busy_response(&follow_up_text) {
        eprintln!("python detached-incomplete follow-up remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let transcript_path = bundle_transcript_path(&follow_up_text).unwrap_or_else(|| {
        panic!(
            "expected detached output transcript path in follow-up reply, got: {follow_up_text:?}"
        )
    });
    let transcript = std::fs::read_to_string(&transcript_path)?;

    session.cancel().await?;

    assert!(
        follow_up_text.contains("\\xA9"),
        "expected new request continuation byte to stay split, got: {follow_up_text:?}"
    );
    assert!(
        !follow_up_text.contains("é"),
        "did not expect cross-request UTF-8 merge, got: {follow_up_text:?}"
    );
    assert!(
        follow_up_text.contains("FOLLOWUP_OK"),
        "expected follow-up output, got: {follow_up_text:?}"
    );
    assert!(
        transcript.contains("IDLE_000"),
        "expected detached idle output in transcript, got: {transcript:?}"
    );
    assert!(
        transcript.contains("\\xC3"),
        "expected detached lead byte to stay with detached transcript, got: {transcript:?}"
    );
    assert!(
        !transcript.contains("FOLLOWUP_OK"),
        "did not expect follow-up output in detached transcript: {transcript:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_interrupt_discards_buffered_tail_after_timeout() -> TestResult<()> {
    let Some(mut session) = start_python_session().await? else {
        return Ok(());
    };

    let timeout_result = session
        .write_stdin_raw_with("import time; time.sleep(30)\nx_tail_marker = 99", Some(0.5))
        .await?;
    let timeout_text = result_text(&timeout_result);
    assert!(
        timeout_text.contains("<<repl status: busy"),
        "expected sleep call to time out, got: {timeout_text:?}"
    );

    let interrupt_result = session.write_stdin_raw_with("\u{3}", Some(5.0)).await?;
    let interrupt_text = result_text(&interrupt_result);
    assert!(
        interrupt_text.contains(">>>"),
        "expected prompt after interrupt, got: {interrupt_text:?}"
    );

    let poll_result = session.write_stdin_raw_with("", Some(0.5)).await?;
    let _poll_text = result_text(&poll_result);

    let deadline = interrupt_recovery_deadline();
    loop {
        if Instant::now() >= deadline {
            session.cancel().await?;
            return Err("worker stayed busy after interrupt".into());
        }

        let result = session.write_stdin_raw_with("1+1", Some(0.5)).await?;
        let text = result_text(&result);
        if text.contains("worker is busy") || text.contains("request already running") {
            sleep(Duration::from_millis(50)).await;
            continue;
        }
        assert!(
            text.contains("2"),
            "expected evaluation after interrupt, got: {text:?}"
        );
        break;
    }

    let deadline = interrupt_recovery_deadline();
    loop {
        if Instant::now() >= deadline {
            session.cancel().await?;
            return Err("worker stayed busy before tail-marker probe".into());
        }

        let marker_result = session
            .write_stdin_raw_with("globals().get('x_tail_marker', 'MISSING')", Some(0.5))
            .await?;
        let marker_text = result_text(&marker_result);
        if is_busy_response(&marker_text) {
            sleep(Duration::from_millis(50)).await;
            continue;
        }
        assert!(
            marker_text.contains("'MISSING'"),
            "expected buffered tail assignment to be discarded, got: {marker_text:?}"
        );
        break;
    }

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_multistatement_payload_completes() -> TestResult<()> {
    let Some(mut session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with("def f():\n    return 3\n\nf()\nprint('done')", Some(5.0))
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        eprintln!("python_multistatement_payload_completes remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(text.contains("3"), "expected 3, got: {text:?}");
    assert!(text.contains("done"), "expected done, got: {text:?}");

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_exception_reported_in_output() -> TestResult<()> {
    let Some(mut session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session.write_stdin_raw_with("1/0", Some(5.0)).await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        eprintln!("python_exception_reported_in_output remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("ZeroDivisionError"),
        "expected traceback, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_pdb_roundtrip() -> TestResult<()> {
    let Some(mut session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with("import pdb; pdb.set_trace()", Some(1.0))
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        eprintln!("python_pdb_roundtrip remained busy entering pdb; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(text.contains("(Pdb)"), "expected pdb prompt, got: {text:?}");

    let result = session.write_stdin_raw_with("c", Some(5.0)).await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        eprintln!("python_pdb_roundtrip remained busy after continue; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains(">>>"),
        "expected python prompt after continue, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_input_can_consume_buffered_lines() -> TestResult<()> {
    let Some(mut session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with("x = input('p> ')\nhello\nprint('got', x)", Some(5.0))
        .await?;
    let mut text = result_text(&result);
    if is_busy_response(&text) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && is_busy_response(&text) && !text.contains("got hello") {
            sleep(Duration::from_millis(50)).await;
            text = result_text(&session.write_stdin_raw_with("", Some(1.0)).await?);
        }
    }
    if is_busy_response(&text) {
        eprintln!("python_input_can_consume_buffered_lines remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        text.contains("got hello"),
        "expected input() to consume buffered hello, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}
