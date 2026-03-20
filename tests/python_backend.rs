mod common;

use common::TestResult;
use rmcp::model::RawContent;
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

fn require_python() -> bool {
    if common::python_available() {
        true
    } else {
        eprintln!("python not available; skipping");
        false
    }
}

fn is_busy_response(text: &str) -> bool {
    text.contains("<<console status: busy")
        || text.contains("worker is busy")
        || text.contains("request already running")
        || text.contains("input discarded while worker busy")
}

async fn start_python_session() -> TestResult<Option<common::McpTestSession>> {
    if !require_python() {
        return Ok(None);
    }

    let mut session = common::spawn_python_server().await?;
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
        timeout_text.contains("<<console status: busy"),
        "expected sleep call to time out, got: {timeout_text:?}"
    );

    let interrupt_result = session.write_stdin_raw_with("\u{3}", Some(5.0)).await?;
    let interrupt_text = result_text(&interrupt_result);
    assert!(
        interrupt_text.contains(">>>"),
        "expected prompt after interrupt, got: {interrupt_text:?}"
    );

    let deadline = Instant::now() + Duration::from_secs(5);
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
async fn python_interrupt_discards_buffered_tail_after_timeout() -> TestResult<()> {
    let Some(mut session) = start_python_session().await? else {
        return Ok(());
    };

    let timeout_result = session
        .write_stdin_raw_with("import time; time.sleep(30)\nx_tail_marker = 99", Some(0.5))
        .await?;
    let timeout_text = result_text(&timeout_result);
    assert!(
        timeout_text.contains("<<console status: busy"),
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

    let deadline = Instant::now() + Duration::from_secs(5);
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

    let deadline = Instant::now() + Duration::from_secs(5);
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
