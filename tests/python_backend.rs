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

fn python_backend_unavailable(text: &str) -> bool {
    common::backend_unavailable(text) || text.contains("worker io error: Permission denied")
}

fn is_busy_response(text: &str) -> bool {
    text.contains("<<repl status: busy")
        || text.contains("worker is busy")
        || text.contains("request already running")
        || text.contains("input discarded while worker busy")
}

fn assert_no_pager_markers(text: &str, context: &str) {
    assert!(
        !text.contains("Press RETURN"),
        "{context} should stay inline without pager prompts, got: {text:?}"
    );
    assert!(
        !text.contains("--More--"),
        "{context} should stay inline without pager prompts, got: {text:?}"
    );
}

fn interrupt_recovery_deadline() -> Instant {
    Instant::now() + Duration::from_secs(if cfg!(target_os = "macos") { 20 } else { 5 })
}

fn python_startup_probe_budget() -> Duration {
    Duration::from_secs(if cfg!(target_os = "macos") { 90 } else { 10 })
}

async fn start_python_session_with_env_vars(
    env_vars: Vec<(String, String)>,
) -> TestResult<Option<common::McpTestSession>> {
    if !require_python() {
        return Ok(None);
    }

    let mut session = common::spawn_server_with_args_env(
        vec![
            "--interpreter".to_string(),
            "python".to_string(),
            "--oversized-output".to_string(),
            "files".to_string(),
            "--sandbox".to_string(),
            "danger-full-access".to_string(),
        ],
        env_vars,
    )
    .await?;
    let probe = session
        .write_stdin_raw_with("print('mcp_repl_python_ready')", Some(2.0))
        .await?;
    let probe = common::wait_until_not_busy(
        &mut session,
        probe,
        Duration::from_millis(100),
        python_startup_probe_budget(),
    )
    .await?;
    let probe_text = result_text(&probe);
    if probe_text.contains("worker io error: Permission denied") {
        eprintln!("python backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(None);
    }

    Ok(Some(session))
}

async fn start_python_session() -> TestResult<Option<common::McpTestSession>> {
    start_python_session_with_env_vars(Vec::new()).await
}

#[cfg(unix)]
const DETACHED_STDIO_HOLDER_SECS: f64 = 2.5;

#[cfg(unix)]
struct DetachedHolderProbe {
    _dir: tempfile::TempDir,
    marker_path: PathBuf,
}

#[cfg(unix)]
impl DetachedHolderProbe {
    fn new() -> TestResult<Self> {
        let dir = tempdir()?;
        Ok(Self {
            marker_path: dir.path().join("holder-exited"),
            _dir: dir,
        })
    }

    fn marker_literal(&self) -> TestResult<String> {
        let marker = self
            .marker_path
            .to_str()
            .ok_or("detached holder marker path must be valid utf-8")?;
        Ok(serde_json::to_string(marker)?)
    }

    async fn wait_for_exit(&self) -> TestResult<()> {
        wait_for_detached_holder_exit(&self.marker_path).await
    }

    fn has_exited(&self) -> bool {
        self.marker_path.exists()
    }
}

async fn wait_for_detached_holder_exit(marker_path: &Path) -> TestResult<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if marker_path.exists() {
            sleep(Duration::from_millis(250)).await;
            return Ok(());
        }
        sleep(Duration::from_millis(50)).await;
    }
    Err(format!("detached holder did not exit: {}", marker_path.display()).into())
}

#[cfg(unix)]
fn shutdown_completion_budget() -> Duration {
    if cfg!(target_os = "macos") {
        Duration::from_millis(1_500)
    } else {
        Duration::from_millis(1_200)
    }
}

#[cfg(unix)]
async fn arm_detached_stdio_holder(
    session: &mut common::McpTestSession,
) -> TestResult<DetachedHolderProbe> {
    let holder = DetachedHolderProbe::new()?;
    let marker_literal = holder.marker_literal()?;
    let setup = session
        .write_stdin_raw_with(
            format!(
                r#"import subprocess, sys
script = """import pathlib, time
time.sleep({DETACHED_STDIO_HOLDER_SECS})
pathlib.Path({marker_literal}).write_text('done')
"""
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
    Ok(holder)
}

#[cfg(unix)]
async fn arm_background_ipc_holder(
    session: &mut common::McpTestSession,
) -> TestResult<DetachedHolderProbe> {
    let holder = DetachedHolderProbe::new()?;
    let marker_literal = holder.marker_literal()?;
    let setup = session
        .write_stdin_raw_with(
            format!(
                r#"import subprocess, sys
script = """import pathlib, time
time.sleep({DETACHED_STDIO_HOLDER_SECS})
pathlib.Path({marker_literal}).write_text('done')
"""
subprocess.Popen(
    [sys.executable, "-c", script],
    stdin=subprocess.DEVNULL,
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
    close_fds=False,
    start_new_session=True,
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
    Ok(holder)
}

#[tokio::test(flavor = "multi_thread")]
async fn python_smoke() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
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
async fn python_backend_runs_inside_mcp_repl_worker() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"import os, pathlib, sys
if sys.platform == "linux":
    print(pathlib.Path(os.readlink("/proc/self/exe")).name)
elif sys.platform == "darwin":
    import ctypes, ctypes.util
    libc = ctypes.CDLL(ctypes.util.find_library("c"), use_errno=True)
    buf = ctypes.create_string_buffer(4096)
    if libc.proc_pidpath(os.getpid(), buf, len(buf)) <= 0:
        raise OSError(ctypes.get_errno(), "proc_pidpath failed")
    print(pathlib.Path(os.fsdecode(buf.value)).name)
else:
    print(pathlib.Path(sys.argv[0]).name)

"#,
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    assert!(
        text.lines().any(|line| line.trim() == "mcp-repl"),
        "expected Python worker process image to be mcp-repl, got: {text:?}"
    );
    assert!(
        !text.contains("mcp-repl-python-worker"),
        "did not expect a separate Python worker binary, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_sys_stdin_supports_file_like_reads() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"import sys
first = sys.stdin.read(3)
abcdef
second = sys.stdin.readline(2)
third = sys.stdin.readline()
print("READS", repr(first), repr(second), repr(third))
"#,
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err("python sys.stdin file-like read test remained busy".into());
    }
    assert!(
        text.contains(r#"READS 'abc' 'de' 'f\n'"#),
        "expected sys.stdin read APIs to preserve buffered text, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_sys_stdio_supports_binary_buffers() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"import sys
out_count = sys.stdout.buffer.write(b"BINOUT\n")
sys.stdout.buffer.flush()
err_count = sys.stderr.buffer.write(b"BINERR\n")
data = sys.stdin.buffer.read(4)
abc
line = sys.stdin.buffer.readline()
def
print("BUFFERS", out_count, err_count, data, line)
"#,
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err("python binary stdio buffer test remained busy".into());
    }
    assert!(
        text.contains("BINOUT"),
        "expected stdout.buffer bytes, got: {text:?}"
    );
    assert!(
        text.contains("BINERR"),
        "expected stderr.buffer bytes, got: {text:?}"
    );
    assert!(
        text.contains(r#"BUFFERS 7 7 b'abc\n' b'def\n'"#),
        "expected binary stdio buffers to read and write bytes, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_sys_path_includes_current_working_directory() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let temp = tempdir()?;
    fs::write(
        temp.path().join("local_import_probe.py"),
        "VALUE = 'local-ok'\n",
    )?;
    let cwd = serde_json::to_string(temp.path().to_str().ok_or("temp path must be utf-8")?)?;
    let result = session
        .write_stdin_raw_with(
            format!(
                r#"import os
os.chdir({cwd})
import local_import_probe
print("LOCAL_IMPORT", local_import_probe.VALUE)
"#
            ),
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err("python local cwd import test remained busy".into());
    }
    assert!(
        text.contains("LOCAL_IMPORT local-ok"),
        "expected cwd import to resolve local module, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_stdin_buffer_read_counts_bytes() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"import sys
first = sys.stdin.buffer.read(1)
é
rest = sys.stdin.buffer.read(2)
print("BYTE_READ", first, rest, len(first), len(rest))
"#,
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err("python stdin buffer byte count test remained busy".into());
    }
    assert!(
        text.contains(r#"BYTE_READ b'\xc3' b'\xa9\n' 1 2"#),
        "expected stdin.buffer reads to count bytes, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_stdin_buffer_preserves_nul_bytes() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            "import sys\ndata = sys.stdin.buffer.read(3)\nA\0B\nprint('NUL_BYTES', repr(data), len(data))",
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err("python stdin.buffer NUL byte read remained busy".into());
    }
    assert!(
        text.contains(r#"NUL_BYTES b'A\x00B' 3"#),
        "expected stdin.buffer read to preserve NUL bytes, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_raw_fd_stdin_read_completes_request() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"import os
data = os.read(0, 5)
abcd
print("RAW_FD", data)
"#,
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err(format!("python raw fd stdin read remained busy: {text:?}").into());
    }
    assert!(
        text.contains(r#"RAW_FD b'abcd\n'"#),
        "expected raw fd stdin read output, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_bracket_continuation_reports_continuation_prompt() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session.write_stdin_raw_with("x = (", Some(5.0)).await?;
    let text = result_text(&result);
    assert!(
        text.contains("... "),
        "expected bracket continuation prompt, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_closed_indented_expression_reports_primary_prompt() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with("print(\n    'CLOSED_INDENT')", Some(5.0))
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains("CLOSED_INDENT"),
        "expected closed indented expression to execute, got: {text:?}"
    );
    assert!(
        text.contains(">>> "),
        "expected closed indented expression to report primary prompt, got: {text:?}"
    );
    assert!(
        !text.contains("... "),
        "expected closed indented expression not to report continuation prompt, got: {text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_large_raw_fd_read_does_not_complete_before_full_payload() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let mut filler = String::new();
    for idx in 0..40_000 {
        filler.push_str("# filler ");
        filler.push_str(&idx.to_string());
        filler.push('\n');
    }
    let input = format!(
        "import os\nchunk = os.read(0, 1048576)\n{filler}print('RAW_LARGE_DONE', len(chunk))"
    );
    let result = session.write_stdin_raw_with(&input, Some(20.0)).await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains("RAW_LARGE_DONE"),
        "expected large raw fd read request to complete after the full payload, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_terminated_block_reports_primary_prompt() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with("if True:\n    print('BLOCK_DONE')\n\n", Some(5.0))
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err("python terminated block remained busy".into());
    }
    assert!(
        text.contains("BLOCK_DONE"),
        "expected terminated block output, got: {text:?}"
    );
    assert!(
        !text.contains(r#"prompt: "... ""#),
        "expected primary prompt after terminated block, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_sys_exit_terminates_session_without_traceback() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with("import sys; sys.exit(7)", Some(5.0))
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err("python sys.exit() remained busy".into());
    }
    assert!(
        !text.contains("Traceback") && !text.contains("SystemExit"),
        "expected sys.exit() to terminate without traceback, got: {text:?}"
    );

    let follow_up = session
        .write_stdin_raw_with("print('AFTER_SYS_EXIT')", Some(5.0))
        .await?;
    let follow_up_text = result_text(&follow_up);
    if is_busy_response(&follow_up_text) {
        session.cancel().await?;
        return Err("python follow-up after sys.exit() remained busy".into());
    }
    assert!(
        follow_up_text.contains("AFTER_SYS_EXIT"),
        "expected Python session to respawn after sys.exit(), got: {follow_up_text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_sys_exit_runs_atexit_handlers() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let temp = tempdir()?;
    let marker = temp.path().join("atexit-marker.txt");
    let marker_literal = serde_json::to_string(
        marker
            .to_str()
            .ok_or("atexit marker path must be valid utf-8")?,
    )?;
    let result = session
        .write_stdin_raw_with(
            format!(
                r#"import atexit, pathlib, sys
atexit.register(lambda: pathlib.Path({marker_literal}).write_text("atexit ran"))
sys.exit()
"#
            ),
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err("python sys.exit() with atexit remained busy".into());
    }

    let follow_up = session
        .write_stdin_raw_with("print('AFTER_ATEXIT')", Some(5.0))
        .await?;
    let follow_up_text = result_text(&follow_up);
    session.cancel().await?;

    assert!(
        follow_up_text.contains("AFTER_ATEXIT"),
        "expected Python session to respawn after sys.exit(), got: {follow_up_text:?}"
    );
    assert_eq!(fs::read_to_string(&marker)?, "atexit ran");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_input_reads_from_sys_stdin() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"import io, sys
sys.stdin = io.StringIO("replacement\n")
print("INPUT", input())
"#,
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err("python input() sys.stdin replacement test remained busy".into());
    }
    assert!(
        text.contains("INPUT replacement"),
        "expected input() to read from sys.stdin, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_text_write_returns_character_count() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"import sys
count = sys.stdout.write("é")
print("COUNT", count)
"#,
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err("python text write count test remained busy".into());
    }
    assert!(
        text.contains("COUNT 1"),
        "expected TextIO.write() to return character count, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_stdout_stderr_expose_text_stream_methods() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"import sys
print("STDOUT_FLAGS", sys.stdout.readable(), sys.stdout.writable(), sys.stdout.seekable())
print("STDERR_FLAGS", sys.stderr.readable(), sys.stderr.writable(), sys.stderr.seekable())
sys.stdout.writelines(["OUT_A", "OUT_B\n"])
sys.stderr.writelines(["ERR_A", "ERR_B\n"])
"#,
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains("STDOUT_FLAGS False True False"),
        "expected stdout text stream flags, got: {text:?}"
    );
    assert!(
        text.contains("STDERR_FLAGS False True False"),
        "expected stderr text stream flags, got: {text:?}"
    );
    assert!(
        text.contains("OUT_AOUT_B"),
        "expected stdout.writelines() output, got: {text:?}"
    );
    assert!(
        text.contains("ERR_AERR_B"),
        "expected stderr.writelines() output, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_sys_stdin_exposes_worker_stdin_fd() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            "import sys\nprint('STDIN_FD', sys.stdin.fileno())",
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err("python sys.stdin.fileno() remained busy".into());
    }
    assert!(
        text.contains("STDIN_FD 0"),
        "expected sys.stdin to expose worker fd 0, got: {text:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_smoke_without_register_at_fork() -> TestResult<()> {
    let _guard = lock_test_mutex();
    if !require_python() {
        return Ok(());
    }

    let temp = tempdir()?;
    fs::write(
        temp.path().join("sitecustomize.py"),
        "import os\ntry:\n    del os.register_at_fork\nexcept AttributeError:\n    pass\n",
    )?;

    let Some(session) = start_python_session_with_env_vars(vec![(
        "PYTHONPATH".to_string(),
        temp.path().display().to_string(),
    )])
    .await?
    else {
        return Ok(());
    };

    let result = session.write_stdin_raw_with("1+1", Some(5.0)).await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        eprintln!("python_smoke_without_register_at_fork remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(text.contains("2"), "expected 2, got: {text:?}");

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_follow_up_after_resolved_timeout_trims_detached_echo_prefix_in_files_mode()
-> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let first = session
        .write_stdin_raw_with(
            "import time; time.sleep(0.2); print('DETACHED_OK')",
            Some(0.05),
        )
        .await?;
    let first_text = result_text(&first);
    assert!(
        first_text.contains("<<repl status: busy"),
        "expected the initial Python request to time out, got: {first_text:?}"
    );

    sleep(Duration::from_millis(if cfg!(target_os = "macos") {
        700
    } else {
        350
    }))
    .await;

    let follow_up = session
        .write_stdin_raw_with("print('FOLLOWUP_OK')", Some(5.0))
        .await?;
    let follow_up_text = result_text(&follow_up);
    if is_busy_response(&follow_up_text) {
        session.cancel().await?;
        return Err(format!(
            "python follow-up remained busy after timed-out request settled: {follow_up_text:?}"
        )
        .into());
    }

    session.cancel().await?;

    assert!(
        follow_up_text.contains("DETACHED_OK"),
        "expected the settled timeout result to be prefixed into the next files-mode reply, got: {follow_up_text:?}"
    );
    assert!(
        follow_up_text.contains("FOLLOWUP_OK"),
        "expected the fresh Python follow-up result, got: {follow_up_text:?}"
    );
    assert!(
        !follow_up_text.contains("import time; time.sleep(0.2); print('DETACHED_OK')"),
        "did not expect the timed-out Python echo to leak into the next visible reply, got: {follow_up_text:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_fork_child_closes_raw_ipc_fds_without_wrapper_close() -> TestResult<()> {
    let _guard = lock_test_mutex();
    if !require_python() {
        return Ok(());
    }

    let temp = tempdir()?;
    let marker_path = temp.path().join("fork-close.log");
    fs::write(
        temp.path().join("sitecustomize.py"),
        r#"import os
_real_fdopen = os.fdopen
_target_fds = {
    int(os.environ["MCP_REPL_IPC_READ_FD"]),
    int(os.environ["MCP_REPL_IPC_WRITE_FD"]),
}
_marker = os.environ["MCP_REPL_FORK_CLOSE_MARKER"]

class _SpyStream:
    def __init__(self, stream, fd):
        self._stream = stream
        self._fd = fd

    def __iter__(self):
        return iter(self._stream)

    def close(self):
        with open(_marker, "a", encoding="utf-8") as handle:
            handle.write(f"{{self._fd}}\n")
        return self._stream.close()

    def __getattr__(self, name):
        return getattr(self._stream, name)

def _wrapped_fdopen(fd, *args, **kwargs):
    stream = _real_fdopen(fd, *args, **kwargs)
    if fd in _target_fds:
        return _SpyStream(stream, fd)
    return stream

os.fdopen = _wrapped_fdopen
"#,
    )?;

    let Some(session) = start_python_session_with_env_vars(vec![
        ("PYTHONPATH".to_string(), temp.path().display().to_string()),
        (
            "MCP_REPL_FORK_CLOSE_MARKER".to_string(),
            marker_path.display().to_string(),
        ),
    ])
    .await?
    else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            r#"import os
exec("pid = os.fork()\nif pid == 0:\n    os._exit(0)\n_, status = os.waitpid(pid, 0)\nprint('FORK_OK', status)")
"#,
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err("python os.fork() remained busy".into());
    }
    assert!(
        text.contains("FORK_OK"),
        "expected fork round-trip output, got: {text:?}"
    );

    session.cancel().await?;

    assert!(
        !marker_path.exists(),
        "expected at-fork cleanup to bypass wrapped stream close, got marker contents: {:?}",
        fs::read_to_string(&marker_path).ok()
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn python_quit_does_not_wait_for_detached_stdio_holders() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(mut session) = start_python_session().await? else {
        return Ok(());
    };

    let holder = arm_detached_stdio_holder(&mut session).await?;

    let start = Instant::now();
    let quit = session.write_stdin_raw_with("quit()", Some(5.0)).await?;
    let elapsed = start.elapsed();
    let quit_text = result_text(&quit);
    if is_busy_response(&quit_text) {
        eprintln!("python_quit_does_not_wait_for_detached_stdio_holders remained busy on quit");
        holder.wait_for_exit().await?;
        session.cancel().await?;
        return Ok(());
    }

    assert!(
        !holder.has_exited(),
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
        holder.wait_for_exit().await?;
        session.cancel().await?;
        return Ok(());
    }

    holder.wait_for_exit().await?;
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
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
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
    let _guard = lock_test_mutex();
    let Some(mut session) = start_python_session().await? else {
        return Ok(());
    };

    let holder = arm_background_ipc_holder(&mut session).await?;

    let start = Instant::now();
    let quit = session.write_stdin_raw_with("quit()", Some(5.0)).await?;
    let elapsed = start.elapsed();
    let quit_text = result_text(&quit);
    if is_busy_response(&quit_text) {
        eprintln!("python_quit_does_not_wait_for_background_ipc_holders remained busy on quit");
        holder.wait_for_exit().await?;
        session.cancel().await?;
        return Ok(());
    }

    assert!(
        !holder.has_exited(),
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
        holder.wait_for_exit().await?;
        session.cancel().await?;
        return Ok(());
    }

    holder.wait_for_exit().await?;
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
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
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
        start_new_session=True,
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
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
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
async fn python_multiline_block_does_not_echo_input_in_visible_reply() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with("def f():\n    return 3\n\nf()", Some(5.0))
        .await?;
    let text = result_text(&result);
    if is_busy_response(&text) {
        eprintln!(
            "python_multiline_block_does_not_echo_input_in_visible_reply remained busy; skipping"
        );
        session.cancel().await?;
        return Ok(());
    }
    let visible = visible_reply_text(&text)?;

    session.cancel().await?;

    assert!(visible.contains("3"), "expected 3, got: {visible:?}");
    assert!(
        !visible.contains("def f():"),
        "did not expect the multiline function definition to echo back, got: {visible:?}"
    );
    assert!(
        !visible.contains("return 3"),
        "did not expect the multiline body to echo back, got: {visible:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_buffered_multiline_prompt_does_not_complete_request_early() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            "if True:\n    pass\n\nimport time\ntime.sleep(0.5)\nprint('DONE')",
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        !is_busy_response(&text),
        "expected buffered multiline request to finish in the original call, got: {text:?}"
    );
    assert!(
        text.contains("DONE"),
        "expected buffered multiline request to include final output, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_incomplete_block_reports_continuation_prompt() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with("if True:\n    print('x')", Some(5.0))
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains("... "),
        "expected incomplete Python block to report continuation prompt, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_commented_block_header_reports_continuation_prompt() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with("if True:  # comment", Some(5.0))
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains("... "),
        "expected commented Python block header to report continuation prompt, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_decorator_reports_continuation_prompt() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with("@staticmethod", Some(5.0))
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains("... "),
        "expected Python decorator to report continuation prompt, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_whitespace_only_reports_primary_prompt() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session.write_stdin_raw_with("   ", Some(5.0)).await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains(">>> "),
        "expected whitespace-only Python input to report primary prompt, got: {text:?}"
    );
    assert!(
        !text.contains("... "),
        "expected whitespace-only Python input not to report continuation prompt, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_invalid_top_level_indent_reports_primary_prompt() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with("    print(1)", Some(5.0))
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains("IndentationError"),
        "expected invalid top-level indent to raise IndentationError, got: {text:?}"
    );
    assert!(
        text.contains(">>> "),
        "expected invalid top-level indent to report primary prompt, got: {text:?}"
    );
    assert!(
        !text.contains("... "),
        "expected invalid top-level indent not to report continuation prompt, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_stdout_replaces_lone_surrogates() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with("print('\\udcff')", Some(5.0))
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        !text.contains("UnicodeEncodeError"),
        "expected stdout to apply replacement error handling, got: {text:?}"
    );
    assert!(
        text.contains("?\n"),
        "expected stdout to write replacement byte for lone surrogate, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_original_stdout_is_flushed_before_reply() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let result = session
        .write_stdin_raw_with(
            "import sys; sys.__stdout__.write('ORIG_STDOUT\\n')",
            Some(5.0),
        )
        .await?;
    let text = result_text(&result);
    session.cancel().await?;

    assert!(
        text.contains("ORIG_STDOUT"),
        "expected original stdout writes to be visible in the completing reply, got: {text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_input_roundtrip() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
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
async fn python_interrupt_unblocks_input_prompt() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };

    let mut text = result_text(
        &session
            .write_stdin_raw_with("value = input('interrupt> ')", Some(1.0))
            .await?,
    );
    if is_busy_response(&text) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && is_busy_response(&text) && !text.contains("interrupt>") {
            sleep(Duration::from_millis(50)).await;
            text = result_text(&session.write_stdin_raw_with("", Some(1.0)).await?);
        }
    }
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err("python input prompt remained busy before interrupt".into());
    }
    assert!(
        text.contains("interrupt>"),
        "expected input prompt, got: {text:?}"
    );

    let interrupt = session.write_stdin_raw_with("\u{3}", Some(5.0)).await?;
    let interrupt_text = result_text(&interrupt);
    assert!(
        !is_busy_response(&interrupt_text),
        "expected input prompt interrupt to complete, got: {interrupt_text:?}"
    );

    let follow_up = session
        .write_stdin_raw_with("print('AFTER_INPUT_INTERRUPT')", Some(5.0))
        .await?;
    let follow_up_text = result_text(&follow_up);
    session.cancel().await?;

    assert!(
        follow_up_text.contains("AFTER_INPUT_INTERRUPT"),
        "expected follow-up to run after input prompt interrupt, got: {follow_up_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_input_roundtrip_under_debug_allocator() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) =
        start_python_session_with_env_vars(vec![("PYTHONMALLOC".to_string(), "debug".to_string())])
            .await?
    else {
        return Ok(());
    };

    let mut text = result_text(
        &session
            .write_stdin_raw_with("value = input('debug> ')", Some(1.0))
            .await?,
    );
    if is_busy_response(&text) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && is_busy_response(&text) && !text.contains("debug>") {
            sleep(Duration::from_millis(50)).await;
            text = result_text(&session.write_stdin_raw_with("", Some(1.0)).await?);
        }
    }
    if is_busy_response(&text) {
        session.cancel().await?;
        return Err("python debug-allocator input roundtrip remained busy before prompt".into());
    }
    assert!(text.contains("debug>"), "expected prompt, got: {text:?}");

    let answer = session
        .write_stdin_raw_with("value\nprint('DEBUG_ALLOCATOR_INPUT', value)", Some(5.0))
        .await?;
    let answer_text = result_text(&answer);
    session.cancel().await?;

    assert!(
        !is_busy_response(&answer_text),
        "expected debug-allocator input reply to complete, got: {answer_text:?}"
    );
    assert!(
        answer_text.contains("DEBUG_ALLOCATOR_INPUT value"),
        "expected input() to survive debug allocator checks, got: {answer_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_help_flows_stay_inline() -> TestResult<()> {
    let _guard = lock_test_mutex();
    if !require_python() {
        return Ok(());
    }

    let session = common::spawn_python_server_with_interactive_pager_files().await?;

    let help_result = session
        .write_stdin_raw_with("help(len)", Some(10.0))
        .await?;
    let help_text = result_text(&help_result);
    if python_backend_unavailable(&help_text) {
        eprintln!("python help backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    if is_busy_response(&help_text) {
        session.cancel().await?;
        return Err(format!("help(len) should complete inline, got: {help_text:?}").into());
    }
    let help_visible = visible_reply_text(&help_text)?;

    assert!(
        help_visible.contains("Help on built-in function len"),
        "expected inline help(len) output, got: {help_visible:?}"
    );
    assert!(
        help_visible.contains("Return the number of items in a container."),
        "expected len() help text, got: {help_visible:?}"
    );
    assert_no_pager_markers(&help_visible, "help(len)");

    let pydoc_result = session
        .write_stdin_raw_with("import pydoc; pydoc.help(len)", Some(10.0))
        .await?;
    let pydoc_text = result_text(&pydoc_result);
    if is_busy_response(&pydoc_text) {
        session.cancel().await?;
        return Err(format!("pydoc.help(len) should complete inline, got: {pydoc_text:?}").into());
    }
    let pydoc_visible = visible_reply_text(&pydoc_text)?;

    assert!(
        pydoc_visible.contains("Help on built-in function len"),
        "expected inline pydoc.help(len) output, got: {pydoc_visible:?}"
    );
    assert!(
        pydoc_visible.contains("Return the number of items in a container."),
        "expected len() help text, got: {pydoc_visible:?}"
    );
    assert_no_pager_markers(&pydoc_visible, "pydoc.help(len)");

    let mut enter_text = result_text(&session.write_stdin_raw_with("help()", Some(5.0)).await?);
    if is_busy_response(&enter_text) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline
            && is_busy_response(&enter_text)
            && !enter_text.contains("help>")
        {
            sleep(Duration::from_millis(50)).await;
            enter_text = result_text(&session.write_stdin_raw_with("", Some(1.0)).await?);
        }
    }
    if is_busy_response(&enter_text) {
        session.cancel().await?;
        return Err(format!("help() did not surface an interactive prompt: {enter_text:?}").into());
    }
    let enter_visible = visible_reply_text(&enter_text)?;

    let mut exit_text = result_text(&session.write_stdin_raw_with("len\nq", Some(5.0)).await?);
    if is_busy_response(&exit_text) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline
            && is_busy_response(&exit_text)
            && !exit_text.contains(">>>")
        {
            sleep(Duration::from_millis(50)).await;
            exit_text = result_text(&session.write_stdin_raw_with("", Some(1.0)).await?);
        }
    }
    if is_busy_response(&exit_text) {
        session.cancel().await?;
        return Err(format!(
            "interactive help() did not return to the Python prompt: {exit_text:?}"
        )
        .into());
    }
    let exit_visible = visible_reply_text(&exit_text)?;

    let follow_up = session.write_stdin_raw_with("1+1", Some(5.0)).await?;
    let follow_up_text = result_text(&follow_up);
    if is_busy_response(&follow_up_text) {
        session.cancel().await?;
        return Err(format!("interactive help() left the session busy: {follow_up_text:?}").into());
    }

    session.cancel().await?;

    assert!(
        enter_visible.contains("help>"),
        "expected help() prompt to stay inline, got: {enter_visible:?}"
    );
    assert_no_pager_markers(&enter_visible, "help()");
    assert!(
        exit_visible.contains("Help on built-in function len"),
        "expected interactive help() to show len help text, got: {exit_visible:?}"
    );
    assert!(
        exit_visible.contains("Return the number of items in a container."),
        "expected len() help text in interactive help(), got: {exit_visible:?}"
    );
    assert_no_pager_markers(&exit_visible, "help() roundtrip");
    assert!(
        exit_visible.contains(">>>"),
        "expected interactive help() to return to the Python prompt, got: {exit_visible:?}"
    );
    assert!(
        follow_up_text.contains("2"),
        "expected a ready prompt after interactive help(), got: {follow_up_text:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_busy_discards_input() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
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
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
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
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
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
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };
    let marker_dir = tempdir()?;
    let marker_path = marker_dir.path().join("detached-idle-written");
    let marker = marker_path
        .to_str()
        .ok_or("detached idle marker path must be valid utf-8")?;
    let marker_literal = serde_json::to_string(marker)?;

    let setup = session
        .write_stdin_raw_with(
            format!(
                r#"import subprocess, sys
script = """import pathlib, sys, time
time.sleep(0.3)
for i in range(160):
    sys.stdout.write("IDLE_%03d " % i + ("x" * 80) + "\\n")
sys.stdout.flush()
pathlib.Path({marker_literal}).write_text("done")
"""
subprocess.Popen(
    [sys.executable, "-c", script],
    stdin=subprocess.DEVNULL,
    close_fds=False,
)
print("parent ready")
"#
            ),
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

    wait_for_detached_holder_exit(&marker_path).await?;
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
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };
    let marker_dir = tempdir()?;
    let tail_marker_path = marker_dir.path().join("idle-tail-written");
    let tail_marker = tail_marker_path
        .to_str()
        .ok_or("idle tail marker path must be valid utf-8")?;
    let tail_marker_literal = serde_json::to_string(tail_marker)?;
    let exit_marker_path = marker_dir.path().join("idle-worker-exiting");
    let exit_marker = exit_marker_path
        .to_str()
        .ok_or("idle exit marker path must be valid utf-8")?;
    let exit_marker_literal = serde_json::to_string(exit_marker)?;
    let release_marker_path = marker_dir.path().join("idle-tail-release");
    let release_marker = release_marker_path
        .to_str()
        .ok_or("idle tail release marker path must be valid utf-8")?;
    let release_marker_literal = serde_json::to_string(release_marker)?;
    let script = format!(
        r#"import os, pathlib, subprocess, sys, threading, time
tail_marker = {tail_marker_literal}
exit_marker = {exit_marker_literal}
release_marker = {release_marker_literal}
writer = """import os, pathlib, sys, time
deadline = time.monotonic() + 5
while not os.path.exists(sys.argv[2]):
    if time.monotonic() >= deadline:
        sys.exit(2)
    time.sleep(0.02)
sys.stdout.write("IDLE_TAIL\\n")
sys.stdout.flush()
pathlib.Path(sys.argv[1]).write_text("done")
time.sleep(0.2)
"""
subprocess.Popen(
    [sys.executable, "-c", writer, tail_marker, release_marker],
    stdin=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
    close_fds=True,
    start_new_session=True,
)
print("armed")
def exit_after_detached_tail():
    while not os.path.exists(tail_marker):
        time.sleep(0.02)
    time.sleep(0.4)
    pathlib.Path(exit_marker).write_text("done")
    os._exit(0)
threading.Thread(target=exit_after_detached_tail, daemon=True).start()"#
    );
    let script_literal = serde_json::to_string(&script)?;

    let arm = session
        .write_stdin_raw_with(format!("exec({script_literal})"), Some(5.0))
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

    fs::write(&release_marker_path, "go")?;
    wait_for_detached_holder_exit(&exit_marker_path).await?;
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
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
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
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };
    let marker_dir = tempdir()?;
    let marker_path = marker_dir.path().join("detached-incomplete-written");
    let marker = marker_path
        .to_str()
        .ok_or("detached incomplete marker path must be valid utf-8")?;
    let marker_literal = serde_json::to_string(marker)?;

    let setup = session
        .write_stdin_raw_with(
            format!(
                r#"import subprocess, sys
script = """import os, pathlib, sys, time
time.sleep(0.3)
for i in range(160):
    os.write(sys.stdout.fileno(), ("IDLE_%03d " % i + ("x" * 80) + "\\n").encode())
os.write(sys.stdout.fileno(), bytes([0xC3]))
pathlib.Path({marker_literal}).write_text("done")
"""
subprocess.Popen(
    [sys.executable, "-c", script],
    stdin=subprocess.DEVNULL,
    close_fds=False,
)
print("parent ready")
"#
            ),
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

    wait_for_detached_holder_exit(&marker_path).await?;
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
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
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
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
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
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
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
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
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
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
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
