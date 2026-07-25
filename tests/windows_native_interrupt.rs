#![cfg(windows)]
#![allow(clippy::await_holding_lock)]

mod common;

use std::ffi::OsStr;
use std::future::Future;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::pin::Pin;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::task::Poll;

use common::TestResult;
use rmcp::model::{CallToolResult, RawContent};
use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, ResetEvent, SetEvent, WaitForMultipleObjects, WaitForSingleObject,
};

const INTERRUPT_ADMISSION_GATE_EVENT_ENV: &str =
    "MCP_REPL_TEST_WINDOWS_INTERRUPT_ADMISSION_GATE_EVENT";
#[cfg(debug_assertions)]
const REARM_GATE_ENABLED_EVENT_ENV: &str = "MCP_REPL_TEST_WINDOWS_REARM_GATE_ENABLED_EVENT";
#[cfg(debug_assertions)]
const REARM_GATE_ENTERED_EVENT_ENV: &str = "MCP_REPL_TEST_WINDOWS_REARM_GATE_ENTERED_EVENT";
#[cfg(debug_assertions)]
const REARM_GATE_RELEASE_EVENT_ENV: &str = "MCP_REPL_TEST_WINDOWS_REARM_GATE_RELEASE_EVENT";
#[cfg(debug_assertions)]
const REARM_REQUESTED_EVENT_ENV: &str = "MCP_REPL_TEST_WINDOWS_REARM_REQUESTED_EVENT";
#[cfg(debug_assertions)]
const COMPLETION_READY_EVENT_ENV: &str = "MCP_REPL_TEST_WINDOWS_COMPLETION_READY_EVENT";
#[cfg(debug_assertions)]
const SIDEBAND_PUBLICATION_GATE_ENTERED_EVENT_ENV: &str =
    "MCP_REPL_TEST_WINDOWS_SIDEBAND_PUBLICATION_GATE_ENTERED_EVENT";
#[cfg(debug_assertions)]
const SIDEBAND_PUBLICATION_GATE_RELEASE_EVENT_ENV: &str =
    "MCP_REPL_TEST_WINDOWS_SIDEBAND_PUBLICATION_GATE_RELEASE_EVENT";
#[cfg(debug_assertions)]
const SIDEBAND_PUBLICATION_DEFERRED_EVENT_ENV: &str =
    "MCP_REPL_TEST_WINDOWS_SIDEBAND_PUBLICATION_DEFERRED_EVENT";
#[cfg(debug_assertions)]
const SIDEBAND_PUBLICATION_SENT_EVENT_ENV: &str =
    "MCP_REPL_TEST_WINDOWS_SIDEBAND_PUBLICATION_SENT_EVENT";
#[cfg(debug_assertions)]
const PYTHON_CHECKPOINT_GATE_ENTERED_EVENT_ENV: &str =
    "MCP_REPL_TEST_WINDOWS_PYTHON_CHECKPOINT_GATE_ENTERED_EVENT";
#[cfg(debug_assertions)]
const PYTHON_CHECKPOINT_GATE_RELEASE_EVENT_ENV: &str =
    "MCP_REPL_TEST_WINDOWS_PYTHON_CHECKPOINT_GATE_RELEASE_EVENT";
#[cfg(debug_assertions)]
const OBSERVER_INITIALIZATION_FAILURE_ENV: &str = "MCP_REPL_TEST_WINDOWS_OBSERVER_INIT_FAILURE";

fn test_mutex() -> &'static Mutex<()> {
    static TEST_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
    TEST_MUTEX.get_or_init(|| Mutex::new(()))
}

fn lock_test_mutex() -> MutexGuard<'static, ()> {
    match test_mutex().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn result_text(result: &CallToolResult) -> String {
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

fn is_busy_response(text: &str) -> bool {
    text.contains("<<repl status: busy")
        || text.contains("worker is busy")
        || text.contains("request already running")
        || text.contains("input discarded while worker busy")
}

async fn poll_future_once<F: Future>(mut future: Pin<&mut F>) -> Option<F::Output> {
    std::future::poll_fn(|cx| {
        Poll::Ready(match future.as_mut().poll(cx) {
            Poll::Ready(output) => Some(output),
            Poll::Pending => None,
        })
    })
    .await
}

struct NamedEvent {
    handle: HANDLE,
    name: String,
}

impl NamedEvent {
    fn new(label: &str, unique: u64) -> TestResult<Self> {
        let name = format!(
            "Local\\mcp-repl-native-interrupt-{}-{unique}-{label}",
            std::process::id()
        );
        let wide = OsStr::new(&name)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let handle = unsafe { CreateEventW(ptr::null(), 1, 0, wide.as_ptr()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Self { handle, name })
    }

    fn new_unique(label: &str) -> TestResult<Self> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let unique = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        Self::new(label, unique)
    }

    fn set(&self) -> TestResult<()> {
        if unsafe { SetEvent(self.handle) } == 0 {
            Err(std::io::Error::last_os_error().into())
        } else {
            Ok(())
        }
    }

    fn reset(&self) -> TestResult<()> {
        if unsafe { ResetEvent(self.handle) } == 0 {
            Err(std::io::Error::last_os_error().into())
        } else {
            Ok(())
        }
    }

    fn is_set(&self) -> TestResult<bool> {
        match unsafe { WaitForSingleObject(self.handle, 0) } {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            WAIT_FAILED => Err(std::io::Error::last_os_error().into()),
            other => Err(format!("named event wait returned unexpected result {other}").into()),
        }
    }

    fn wait_until_set(&self, description: &str) -> TestResult<()> {
        match unsafe { WaitForSingleObject(self.handle, 10_000) } {
            WAIT_OBJECT_0 => Ok(()),
            WAIT_FAILED => Err(std::io::Error::last_os_error().into()),
            other => Err(format!("{description} was not signaled: wait result {other}").into()),
        }
    }
}

impl Drop for NamedEvent {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeControlEvent {
    CtrlC,
    CtrlBreak,
}

struct NativeHandlerGates {
    ctrl_c_entered: NamedEvent,
    ctrl_break_entered: NamedEvent,
    release: NamedEvent,
}

impl NativeHandlerGates {
    fn new() -> TestResult<Self> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let unique = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        Ok(Self {
            ctrl_c_entered: NamedEvent::new("ctrl-c", unique)?,
            ctrl_break_entered: NamedEvent::new("ctrl-break", unique)?,
            release: NamedEvent::new("release", unique)?,
        })
    }

    fn wait_for_entry(&self) -> TestResult<NativeControlEvent> {
        let handles = [self.ctrl_c_entered.handle, self.ctrl_break_entered.handle];
        let result =
            unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, 10_000) };
        match result {
            WAIT_OBJECT_0 => Ok(NativeControlEvent::CtrlC),
            value if value == WAIT_OBJECT_0 + 1 => Ok(NativeControlEvent::CtrlBreak),
            WAIT_FAILED => Err(std::io::Error::last_os_error().into()),
            other => {
                Err(format!("native console handler did not enter: wait result {other}").into())
            }
        }
    }

    fn handler_entered(&self) -> TestResult<bool> {
        Ok(self.ctrl_c_entered.is_set()? || self.ctrl_break_entered.is_set()?)
    }

    fn release(&self) -> TestResult<()> {
        self.release.set()
    }

    fn reset(&self) -> TestResult<()> {
        self.ctrl_c_entered.reset()?;
        self.ctrl_break_entered.reset()?;
        self.release.reset()
    }

    fn python_install_script(&self) -> TestResult<String> {
        let ctrl_c_name = serde_json::to_string(&self.ctrl_c_entered.name)?;
        let ctrl_break_name = serde_json::to_string(&self.ctrl_break_entered.name)?;
        let release_name = serde_json::to_string(&self.release.name)?;
        Ok(format!(
            r#"
import ctypes
from ctypes import wintypes

_kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
_HANDLER_ROUTINE = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.DWORD)
_kernel32.OpenEventW.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.LPCWSTR]
_kernel32.OpenEventW.restype = wintypes.HANDLE
_kernel32.SetEvent.argtypes = [wintypes.HANDLE]
_kernel32.SetEvent.restype = wintypes.BOOL
_kernel32.WaitForSingleObject.argtypes = [wintypes.HANDLE, wintypes.DWORD]
_kernel32.WaitForSingleObject.restype = wintypes.DWORD
_kernel32.SetConsoleCtrlHandler.argtypes = [_HANDLER_ROUTINE, wintypes.BOOL]
_kernel32.SetConsoleCtrlHandler.restype = wintypes.BOOL

_EVENT_ACCESS = 0x00100002
_ctrl_c_entered = _kernel32.OpenEventW(_EVENT_ACCESS, False, {ctrl_c_name})
_ctrl_break_entered = _kernel32.OpenEventW(_EVENT_ACCESS, False, {ctrl_break_name})
_native_release = _kernel32.OpenEventW(_EVENT_ACCESS, False, {release_name})
if not _ctrl_c_entered or not _ctrl_break_entered or not _native_release:
    raise ctypes.WinError(ctypes.get_last_error())

native_event_codes = []
native_interrupt_should_set = False

@_HANDLER_ROUTINE
def _native_handler(event):
    global native_interrupt_should_set
    if event not in (0, 1):
        return False
    native_event_codes.append(int(event))
    entered = _ctrl_c_entered if event == 0 else _ctrl_break_entered
    _kernel32.SetEvent(entered)
    _kernel32.WaitForSingleObject(_native_release, 0xFFFFFFFF)
    if native_interrupt_should_set:
        ctypes.pythonapi.PyErr_SetInterrupt()
    _kernel32.SetConsoleCtrlHandler(_native_handler, False)
    _kernel32.SetConsoleCtrlHandler(_native_handler, True)
    return True

if not _kernel32.SetConsoleCtrlHandler(_native_handler, True):
    raise ctypes.WinError(ctypes.get_last_error())
"#
        ))
    }
}

async fn start_python_session() -> TestResult<Option<common::McpTestSession>> {
    start_python_session_with_env_vars(Vec::new()).await
}

async fn start_python_session_with_env_vars(
    env_vars: Vec<(String, String)>,
) -> TestResult<Option<common::McpTestSession>> {
    start_python_session_with_env_vars_and_sandbox(env_vars, "danger-full-access").await
}

fn python_sandbox_unavailable(text: &str) -> bool {
    common::backend_unavailable(text)
        || text.contains("worker sandbox error")
        || text.contains("prepared capability SID requires an unrestricted base token")
}

async fn start_python_session_with_env_vars_and_sandbox(
    env_vars: Vec<(String, String)>,
    sandbox: &str,
) -> TestResult<Option<common::McpTestSession>> {
    start_python_session_with_env_vars_and_sandbox_in_cwd(env_vars, sandbox, None).await
}

async fn start_python_session_with_env_vars_and_sandbox_in_cwd(
    env_vars: Vec<(String, String)>,
    sandbox: &str,
    cwd: Option<&Path>,
) -> TestResult<Option<common::McpTestSession>> {
    if !common::python_available() {
        eprintln!("python not available; skipping Windows native interrupt regressions");
        return Ok(None);
    }
    let args = vec![
        "--interpreter".to_string(),
        "python".to_string(),
        "--oversized-output".to_string(),
        "files".to_string(),
        "--sandbox".to_string(),
        sandbox.to_string(),
    ];
    let session =
        common::spawn_server_with_args_env_and_cwd(args, env_vars, cwd.map(Path::to_path_buf))
            .await?;
    let probe = session
        .write_stdin_raw_with("print('WINDOWS_NATIVE_PYTHON_READY')", Some(30.0))
        .await?;
    let text = result_text(&probe);
    if python_sandbox_unavailable(&text) {
        eprintln!(
            "Python backend or {sandbox} sandbox unavailable; \
             skipping Windows native interrupt regression"
        );
        session.cancel().await?;
        return Ok(None);
    }
    if is_busy_response(&text) || !text.contains("WINDOWS_NATIVE_PYTHON_READY") {
        session.cancel().await?;
        return Err(format!("Python worker did not become ready: {text:?}").into());
    }
    Ok(Some(session))
}

async fn start_r_session() -> TestResult<Option<common::McpTestSession>> {
    let session = common::spawn_server_with_args(vec![
        "--oversized-output".to_string(),
        "files".to_string(),
        "--sandbox".to_string(),
        "danger-full-access".to_string(),
    ])
    .await?;
    let probe = session
        .write_stdin_raw_with("cat('WINDOWS_NATIVE_R_READY\\n')", Some(30.0))
        .await?;
    let text = result_text(&probe);
    if common::backend_unavailable(&text) {
        eprintln!("R backend unavailable; skipping Windows native interrupt regressions");
        session.cancel().await?;
        return Ok(None);
    }
    if is_busy_response(&text) || !text.contains("WINDOWS_NATIVE_R_READY") {
        session.cancel().await?;
        return Err(format!("R worker did not become ready: {text:?}").into());
    }
    Ok(Some(session))
}

#[cfg(debug_assertions)]
#[tokio::test(flavor = "multi_thread")]
async fn startup_observer_failure_preserves_diagnostic_for_builtin_workers() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let expected =
        "Windows Ctrl-C observer failed to inject a test Ctrl-C observer initialization failure";

    for (backend, interpreter) in [("R", None), ("Python", Some("python"))] {
        let mut args = Vec::new();
        if let Some(interpreter) = interpreter {
            args.extend(["--interpreter".to_string(), interpreter.to_string()]);
        }
        args.extend([
            "--oversized-output".to_string(),
            "files".to_string(),
            "--sandbox".to_string(),
            "danger-full-access".to_string(),
        ]);
        let session = common::spawn_server_with_args_env(
            args,
            vec![(
                OBSERVER_INITIALIZATION_FAILURE_ENV.to_string(),
                "1".to_string(),
            )],
        )
        .await?;

        let result = session.write_stdin_raw_with("1", Some(10.0)).await?;
        let text = result_text(&result);
        assert!(
            text.contains(expected),
            "{backend} startup should surface the observer diagnostic, got: {text:?}"
        );
        assert!(
            !text.contains("first worker sideband message must be worker_ready"),
            "{backend} startup should not replace the observer diagnostic, got: {text:?}"
        );
        session.cancel().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_native_ctrl_c_waits_for_consuming_handler_and_preserves_its_policy()
-> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };
    let gates = NativeHandlerGates::new()?;
    let install_script = gates.python_install_script()?;
    let first_input = format!(
        "exec({})\nnative_interrupt_should_set = True\nfirst_native_value = input('native-set> ')",
        serde_json::to_string(&install_script)?
    );
    let first_prompt = session
        .write_stdin_raw_with(first_input, Some(10.0))
        .await?;
    let first_prompt_text = result_text(&first_prompt);
    assert!(
        first_prompt_text.contains("native-set> "),
        "expected first managed input prompt, got: {first_prompt_text:?}"
    );

    let mut first_interrupt =
        Box::pin(session.write_stdin_raw_unterminated_with("\u{3}", Some(10.0)));
    let mut first_early = poll_future_once(first_interrupt.as_mut()).await;
    let first_event = tokio::task::block_in_place(|| gates.wait_for_entry())?;
    if first_early.is_none() {
        first_early = poll_future_once(first_interrupt.as_mut()).await;
    }
    let first_settled_before_release = first_early.is_some();
    gates.release()?;
    let first_result = match first_early {
        Some(result) => result?,
        None => first_interrupt.as_mut().await?,
    };
    let first_text = result_text(&first_result);
    assert_eq!(
        first_event,
        NativeControlEvent::CtrlC,
        "runtime handler received the wrong native control event"
    );
    assert!(
        !first_settled_before_release,
        "interrupt request settled before the consuming handler completed: {first_text:?}"
    );
    assert!(
        first_text.contains("KeyboardInterrupt"),
        "Python did not process the interrupt state set by the consuming handler: {first_text:?}"
    );

    gates.reset()?;
    let second_prompt = session
        .write_stdin_raw_with(
            "native_interrupt_should_set = False\nsecond_native_value = input('native-consume> ')\nprint('NATIVE_CONSUMED', second_native_value)",
            Some(10.0),
        )
        .await?;
    let second_prompt_text = result_text(&second_prompt);
    assert!(
        second_prompt_text.contains("native-consume> "),
        "expected second managed input prompt, got: {second_prompt_text:?}"
    );

    let mut second_interrupt =
        Box::pin(session.write_stdin_raw_unterminated_with("\u{3}", Some(10.0)));
    let mut second_early = poll_future_once(second_interrupt.as_mut()).await;
    let second_event = tokio::task::block_in_place(|| gates.wait_for_entry())?;
    if second_early.is_none() {
        second_early = poll_future_once(second_interrupt.as_mut()).await;
    }
    let second_settled_before_release = second_early.is_some();
    gates.release()?;
    let second_result = match second_early {
        Some(result) => result?,
        None => second_interrupt.as_mut().await?,
    };
    let second_text = result_text(&second_result);
    assert_eq!(
        second_event,
        NativeControlEvent::CtrlC,
        "re-registered consuming handler received the wrong native control event"
    );
    assert!(
        !second_settled_before_release,
        "second interrupt settled before the consuming handler completed: {second_text:?}"
    );
    assert!(
        !second_text.contains("KeyboardInterrupt"),
        "mcp-repl synthesized KeyboardInterrupt after the handler consumed Ctrl-C: {second_text:?}"
    );

    let answer = session
        .write_stdin_raw_with("accepted-answer", Some(10.0))
        .await?;
    let answer_text = result_text(&answer);
    assert!(
        answer_text.contains("NATIVE_CONSUMED accepted-answer"),
        "consumed Ctrl-C should leave input waiting for a normal answer: {answer_text:?}"
    );
    assert!(
        !answer_text.contains("KeyboardInterrupt"),
        "a stale KeyboardInterrupt reached the later answer: {answer_text:?}"
    );

    let follow_up = session
        .write_stdin_raw_with(
            "print('NATIVE_EVENT_CODES', native_event_codes)\nprint('NATIVE_FOLLOWUP_OK')",
            Some(10.0),
        )
        .await?;
    let follow_up_text = result_text(&follow_up);
    assert!(
        follow_up_text.contains("NATIVE_EVENT_CODES [0, 0]"),
        "expected two sequential CTRL_C_EVENT deliveries: {follow_up_text:?}"
    );
    assert!(
        follow_up_text.contains("NATIVE_FOLLOWUP_OK")
            && !follow_up_text.contains("KeyboardInterrupt"),
        "Python session was not clean after sequential interrupts: {follow_up_text:?}"
    );

    let _ = session
        .write_stdin_raw_with(
            "_kernel32.SetConsoleCtrlHandler(_native_handler, False)",
            Some(10.0),
        )
        .await?;
    drop(second_interrupt);
    drop(first_interrupt);
    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_native_ctrl_c_survives_supported_windows_sandboxes() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let install_script = r#"
import ctypes
from ctypes import wintypes

_sandbox_kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
_SANDBOX_HANDLER_ROUTINE = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.DWORD)
_sandbox_kernel32.SetConsoleCtrlHandler.argtypes = [
    _SANDBOX_HANDLER_ROUTINE,
    wintypes.BOOL,
]
_sandbox_kernel32.SetConsoleCtrlHandler.restype = wintypes.BOOL
sandbox_native_event_codes = []

@_SANDBOX_HANDLER_ROUTINE
def _sandbox_native_handler(event):
    sandbox_native_event_codes.append(int(event))
    if event == 0:
        ctypes.pythonapi.PyErr_SetInterrupt()
        return True
    return False

if not _sandbox_kernel32.SetConsoleCtrlHandler(_sandbox_native_handler, True):
    raise ctypes.WinError(ctypes.get_last_error())
"#;

    for sandbox in ["read-only", "workspace-write"] {
        let workspace = tempfile::tempdir()?;
        let Some(session) = start_python_session_with_env_vars_and_sandbox_in_cwd(
            Vec::new(),
            sandbox,
            Some(workspace.path()),
        )
        .await?
        else {
            continue;
        };
        let prompt = session
            .write_stdin_raw_with(
                format!(
                    "exec({})\nsandbox_native_value = input('sandbox-native> ')",
                    serde_json::to_string(install_script)?
                ),
                Some(20.0),
            )
            .await?;
        let prompt_text = result_text(&prompt);
        assert!(
            prompt_text.contains("sandbox-native> "),
            "{sandbox} worker did not reach managed input before Ctrl-C: {prompt_text:?}"
        );

        let interrupted = session
            .write_stdin_raw_unterminated_with("\u{3}", Some(20.0))
            .await?;
        let interrupt_text = result_text(&interrupted);
        assert!(
            interrupt_text.contains("KeyboardInterrupt"),
            "{sandbox} worker did not process its runtime-native interrupt: {interrupt_text:?}"
        );
        assert!(
            !interrupt_text.contains("protocol error")
                && !interrupt_text.contains("observer failed"),
            "{sandbox} worker failed the native interrupt transaction: {interrupt_text:?}"
        );

        let follow_up = session
            .write_stdin_raw_with(
                format!(
                    "print('SANDBOX_NATIVE_MODE', {})\nprint('SANDBOX_NATIVE_EVENT_CODES', sandbox_native_event_codes)\nprint('SANDBOX_NATIVE_FOLLOWUP_OK')",
                    serde_json::to_string(sandbox)?
                ),
                Some(20.0),
            )
            .await?;
        let follow_up_text = result_text(&follow_up);
        assert!(
            follow_up_text.contains(&format!("SANDBOX_NATIVE_MODE {sandbox}"))
                && follow_up_text.contains("SANDBOX_NATIVE_EVENT_CODES [0]")
                && follow_up_text.contains("SANDBOX_NATIVE_FOLLOWUP_OK"),
            "{sandbox} session did not recover after native Ctrl-C: {follow_up_text:?}"
        );
        assert!(
            !follow_up_text.contains("KeyboardInterrupt"),
            "{sandbox} session retained stale Python interrupt state: {follow_up_text:?}"
        );

        let _ = session
            .write_stdin_raw_with(
                "_sandbox_kernel32.SetConsoleCtrlHandler(_sandbox_native_handler, False)",
                Some(20.0),
            )
            .await?;
        session.cancel().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_zero_timeout_interrupt_coalesces_overlapping_ctrl_c() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };
    let gates = NativeHandlerGates::new()?;
    let install_script = gates.python_install_script()?;
    let prompt = session
        .write_stdin_raw_with(
            format!(
                "exec({})\nnative_interrupt_should_set = False\ncoalesced_value = input('coalesced> ')",
                serde_json::to_string(&install_script)?
            ),
            Some(10.0),
        )
        .await?;
    let prompt_text = result_text(&prompt);
    assert!(
        prompt_text.contains("coalesced> "),
        "expected managed input before zero-timeout interrupt: {prompt_text:?}"
    );

    let first = session
        .write_stdin_raw_unterminated_with("\u{3}", Some(0.0))
        .await?;
    let first_text = result_text(&first);
    assert!(
        first_text.contains("timed out") || first_text.contains("timeout"),
        "zero-timeout interrupt should report its still-running transaction: {first_text:?}"
    );
    assert_eq!(
        tokio::task::block_in_place(|| gates.wait_for_entry())?,
        NativeControlEvent::CtrlC,
        "zero-timeout delivery must remain native CTRL_C_EVENT"
    );

    let overlapping = session
        .write_stdin_raw_unterminated_with("\u{3}", Some(0.0))
        .await?;
    let overlapping_text = result_text(&overlapping);
    assert!(
        overlapping_text.contains("timed out") || overlapping_text.contains("timeout"),
        "overlapping zero-timeout Ctrl-C should remain joined to the first transaction: {overlapping_text:?}"
    );
    assert!(
        !overlapping_text.contains("observer failed"),
        "overlapping Ctrl-C should coalesce instead of poisoning the observer: {overlapping_text:?}"
    );

    gates.release()?;
    let answer = session
        .write_stdin_raw_with("coalesced-answer", Some(10.0))
        .await?;
    let answer_text = result_text(&answer);
    assert!(
        !answer_text.contains("KeyboardInterrupt"),
        "coalesced delivery left stale Python interrupt state: {answer_text:?}"
    );

    let follow_up = session
        .write_stdin_raw_with(
            "print('COALESCED_EVENT_CODES', native_event_codes)\nprint('COALESCED_VALUE', coalesced_value)",
            Some(10.0),
        )
        .await?;
    let follow_up_text = result_text(&follow_up);
    assert!(
        follow_up_text.contains("COALESCED_EVENT_CODES [0]"),
        "two client Ctrl-C calls must produce one in-flight native delivery: {follow_up_text:?}"
    );
    assert!(
        follow_up_text.contains("COALESCED_VALUE coalesced-answer"),
        "Python input did not recover after the coalesced transaction: {follow_up_text:?}"
    );

    let _ = session
        .write_stdin_raw_with(
            "_kernel32.SetConsoleCtrlHandler(_native_handler, False)",
            Some(10.0),
        )
        .await?;
    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_zero_timeout_interrupt_gates_follow_up_input_until_native_completion()
-> TestResult<()> {
    let _guard = lock_test_mutex();
    let admission_gate = NamedEvent::new_unique("admission-gate")?;
    let Some(session) = start_python_session_with_env_vars(vec![(
        INTERRUPT_ADMISSION_GATE_EVENT_ENV.to_string(),
        admission_gate.name.clone(),
    )])
    .await?
    else {
        return Ok(());
    };
    let gates = NativeHandlerGates::new()?;
    let install_script = gates.python_install_script()?;
    let prompt = session
        .write_stdin_raw_with(
            format!(
                "exec({})\nnative_interrupt_should_set = False\ngated_value = input('gated> ')\nprint('GATED_VALUE', gated_value)",
                serde_json::to_string(&install_script)?
            ),
            Some(10.0),
        )
        .await?;
    let prompt_text = result_text(&prompt);
    assert!(
        prompt_text.contains("gated> "),
        "expected managed input before zero-timeout interrupt: {prompt_text:?}"
    );

    let interrupted = session
        .write_stdin_raw_unterminated_with("\u{3}", Some(0.0))
        .await?;
    let interrupted_text = result_text(&interrupted);
    assert!(
        interrupted_text.contains("timed out") || interrupted_text.contains("timeout"),
        "zero-timeout interrupt should leave an outstanding transaction: {interrupted_text:?}"
    );
    assert_eq!(
        tokio::task::block_in_place(|| gates.wait_for_entry())?,
        NativeControlEvent::CtrlC,
        "follow-up admission test requires native CTRL_C_EVENT"
    );
    admission_gate.reset()?;

    let mut answer = Box::pin(session.write_stdin_raw_with("gated-answer", Some(10.0)));
    let early = poll_future_once(answer.as_mut()).await;
    assert!(
        early.is_none(),
        "follow-up input settled while the native handler was blocked"
    );
    tokio::task::block_in_place(|| admission_gate.wait_until_set("interrupt admission gate"))?;
    let early = poll_future_once(answer.as_mut()).await;
    assert!(
        early.is_none(),
        "follow-up input overtook the outstanding native interrupt transaction"
    );

    gates.release()?;
    let answer_result = answer.as_mut().await?;
    let answer_text = result_text(&answer_result);
    assert!(
        answer_text.contains("GATED_VALUE gated-answer"),
        "follow-up input was not consumed after native completion: {answer_text:?}"
    );
    assert!(
        !answer_text.contains("KeyboardInterrupt"),
        "follow-up input received a synthesized or stale interrupt: {answer_text:?}"
    );

    let follow_up = session
        .write_stdin_raw_with(
            "print('GATED_EVENT_CODES', native_event_codes)\nprint('GATED_FOLLOWUP_OK')",
            Some(10.0),
        )
        .await?;
    let follow_up_text = result_text(&follow_up);
    assert!(
        follow_up_text.contains("GATED_EVENT_CODES [0]")
            && follow_up_text.contains("GATED_FOLLOWUP_OK"),
        "session did not recover after gated follow-up input: {follow_up_text:?}"
    );

    let _ = session
        .write_stdin_raw_with(
            "_kernel32.SetConsoleCtrlHandler(_native_handler, False)",
            Some(10.0),
        )
        .await?;
    drop(answer);
    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_native_ctrl_c_prefix_defers_same_call_tail_until_checkpoint() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let tail_consumed = NamedEvent::new_unique("same-call-tail-consumed")?;
    let Some(session) = start_python_session().await? else {
        return Ok(());
    };
    let gates = NativeHandlerGates::new()?;
    let install_script = gates.python_install_script()?;
    let tail_consumed_name = serde_json::to_string(&tail_consumed.name)?;
    let prompt = session
        .write_stdin_raw_with(
            format!(
                r#"exec({})
native_interrupt_should_set = False
_same_call_tail_consumed = _kernel32.OpenEventW(
    _EVENT_ACCESS, False, {tail_consumed_name}
)
if not _same_call_tail_consumed:
    raise ctypes.WinError(ctypes.get_last_error())
same_call_value = input('same-call> ')
_kernel32.SetEvent(_same_call_tail_consumed)
print('SAME_CALL_VALUE', same_call_value)"#,
                serde_json::to_string(&install_script)?,
            ),
            Some(10.0),
        )
        .await?;
    let prompt_text = result_text(&prompt);
    assert!(
        prompt_text.contains("same-call> "),
        "expected managed input before same-call Ctrl-C prefix: {prompt_text:?}"
    );

    let mut interrupt_and_tail =
        Box::pin(session.write_stdin_raw_unterminated_with("\u{3}same-call-answer", Some(30.0)));
    assert!(
        poll_future_once(interrupt_and_tail.as_mut())
            .await
            .is_none(),
        "same-call Ctrl-C prefix settled before native handler entry"
    );
    assert_eq!(
        tokio::task::block_in_place(|| gates.wait_for_entry())?,
        NativeControlEvent::CtrlC,
        "same-call prefix must deliver native CTRL_C_EVENT"
    );
    assert!(
        !tail_consumed.is_set()?,
        "same-call tail reached the runtime while its native handler was blocked"
    );
    assert!(
        poll_future_once(interrupt_and_tail.as_mut())
            .await
            .is_none(),
        "same-call tail overtook the blocked native handler and runtime checkpoint"
    );

    gates.release()?;
    let result = interrupt_and_tail.as_mut().await?;
    let text = result_text(&result);
    tokio::task::block_in_place(|| {
        tail_consumed.wait_until_set("same-call tail runtime consumption")
    })?;
    assert!(
        text.contains("SAME_CALL_VALUE same-call-answer"),
        "same-call tail was not admitted after native completion: {text:?}"
    );
    assert!(
        !text.contains("KeyboardInterrupt"),
        "same-call tail inherited synthesized or stale Python interrupt state: {text:?}"
    );

    let follow_up = session
        .write_stdin_raw_with(
            "print('SAME_CALL_EVENT_CODES', native_event_codes)\nprint('SAME_CALL_FOLLOWUP_OK')",
            Some(10.0),
        )
        .await?;
    let follow_up_text = result_text(&follow_up);
    assert!(
        follow_up_text.contains("SAME_CALL_EVENT_CODES [0]")
            && follow_up_text.contains("SAME_CALL_FOLLOWUP_OK"),
        "session did not recover after same-call Ctrl-C and tail: {follow_up_text:?}"
    );

    let _ = session
        .write_stdin_raw_with(
            "_kernel32.SetConsoleCtrlHandler(_native_handler, False)",
            Some(10.0),
        )
        .await?;
    drop(interrupt_and_tail);
    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_interrupt_gate_timeout_preserves_existing_session_state() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let admission_gate = NamedEvent::new_unique("timeout-admission-gate")?;
    let Some(session) = start_python_session_with_env_vars(vec![(
        INTERRUPT_ADMISSION_GATE_EVENT_ENV.to_string(),
        admission_gate.name.clone(),
    )])
    .await?
    else {
        return Ok(());
    };
    let gates = NativeHandlerGates::new()?;
    let install_script = gates.python_install_script()?;
    let prompt = session
        .write_stdin_raw_with(
            format!(
                "exec({})\nnative_interrupt_should_set = False\npreserved_state = 'SESSION_STATE_SURVIVED'\ntimeout_gate_value = input('timeout-gate> ')\nprint('TIMEOUT_GATE_VALUE', timeout_gate_value)",
                serde_json::to_string(&install_script)?
            ),
            Some(10.0),
        )
        .await?;
    let prompt_text = result_text(&prompt);
    assert!(
        prompt_text.contains("timeout-gate> "),
        "expected managed input before admission-timeout regression: {prompt_text:?}"
    );

    let interrupted = session
        .write_stdin_raw_unterminated_with("\u{3}", Some(0.0))
        .await?;
    let interrupted_text = result_text(&interrupted);
    assert!(
        interrupted_text.contains("timed out") || interrupted_text.contains("timeout"),
        "zero-timeout interrupt should leave an outstanding transaction: {interrupted_text:?}"
    );
    assert_eq!(
        tokio::task::block_in_place(|| gates.wait_for_entry())?,
        NativeControlEvent::CtrlC,
        "admission-timeout regression requires native CTRL_C_EVENT"
    );

    admission_gate.reset()?;
    let mut rejected = Box::pin(session.write_stdin_raw_with("must-not-be-sent", Some(1.0)));
    let early = poll_future_once(rejected.as_mut()).await;
    assert!(
        early.is_none(),
        "pre-admission request settled before reaching the interrupt gate"
    );
    tokio::task::block_in_place(|| {
        admission_gate.wait_until_set("interrupt admission timeout gate")
    })?;
    let rejected_result = rejected.as_mut().await?;
    let rejected_text = result_text(&rejected_result);
    assert!(
        rejected_text.contains("timed out") || rejected_text.contains("timeout"),
        "blocked pre-admission request should time out: {rejected_text:?}"
    );

    gates.release()?;
    let answer = session
        .write_stdin_raw_with("surviving-answer", Some(10.0))
        .await?;
    let answer_text = result_text(&answer);
    assert!(
        answer_text.contains("TIMEOUT_GATE_VALUE surviving-answer"),
        "pre-admission timeout reset the worker or leaked its unsent input: {answer_text:?}"
    );

    let follow_up = session
        .write_stdin_raw_with(
            "print('PRESERVED_STATE', preserved_state)\nprint('TIMEOUT_EVENT_CODES', native_event_codes)",
            Some(10.0),
        )
        .await?;
    let follow_up_text = result_text(&follow_up);
    assert!(
        follow_up_text.contains("PRESERVED_STATE SESSION_STATE_SURVIVED"),
        "pre-admission timeout replaced the existing Python session: {follow_up_text:?}"
    );
    assert!(
        follow_up_text.contains("TIMEOUT_EVENT_CODES [0]"),
        "pre-admission timeout changed native interrupt delivery: {follow_up_text:?}"
    );

    let _ = session
        .write_stdin_raw_with(
            "_kernel32.SetConsoleCtrlHandler(_native_handler, False)",
            Some(10.0),
        )
        .await?;
    drop(rejected);
    session.cancel().await?;
    Ok(())
}

#[cfg(debug_assertions)]
#[tokio::test(flavor = "multi_thread")]
async fn python_ctrl_c_between_finish_and_rearm_retries_without_poisoning() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let rearm_enabled = NamedEvent::new_unique("rearm-enabled")?;
    let rearm_entered = NamedEvent::new_unique("rearm-entered")?;
    let rearm_release = NamedEvent::new_unique("rearm-release")?;
    let rearm_requested = NamedEvent::new_unique("rearm-requested")?;
    let Some(session) = start_python_session_with_env_vars(vec![
        (
            REARM_GATE_ENABLED_EVENT_ENV.to_string(),
            rearm_enabled.name.clone(),
        ),
        (
            REARM_GATE_ENTERED_EVENT_ENV.to_string(),
            rearm_entered.name.clone(),
        ),
        (
            REARM_GATE_RELEASE_EVENT_ENV.to_string(),
            rearm_release.name.clone(),
        ),
        (
            REARM_REQUESTED_EVENT_ENV.to_string(),
            rearm_requested.name.clone(),
        ),
    ])
    .await?
    else {
        return Ok(());
    };
    let gates = NativeHandlerGates::new()?;
    let install_script = gates.python_install_script()?;
    let installed = session
        .write_stdin_raw_with(
            format!(
                "exec({})\nnative_interrupt_should_set = False\nprint('REARM_RACE_HANDLER_READY')",
                serde_json::to_string(&install_script)?
            ),
            Some(10.0),
        )
        .await?;
    let installed_text = result_text(&installed);
    assert!(
        installed_text.contains("REARM_RACE_HANDLER_READY"),
        "failed to install consuming handler for rearm race: {installed_text:?}"
    );

    rearm_enabled.set()?;
    let mut cell = Box::pin(
        session.write_stdin_raw_with("rearm_race_value = 42\nprint('REARM_RACE_CELL')", Some(1.0)),
    );
    let early = poll_future_once(cell.as_mut()).await;
    assert!(
        early.is_none(),
        "cell settled before reaching the deterministic rearm boundary"
    );
    tokio::task::block_in_place(|| rearm_entered.wait_until_set("observer rearm boundary"))?;
    rearm_requested.reset()?;
    let timed_out = cell.as_mut().await?;
    let timed_out_text = result_text(&timed_out);
    assert!(
        timed_out_text.contains("timed out") || timed_out_text.contains("timeout"),
        "gated cell must remain pending in the worker after its client timeout: {timed_out_text:?}"
    );
    assert!(
        timed_out_text.contains("REARM_RACE_CELL"),
        "the timeout reply lost output produced before the rearm boundary: {timed_out_text:?}"
    );
    drop(cell);

    let mut interrupted = Box::pin(session.write_stdin_raw_unterminated_with("\u{3}", Some(30.0)));
    assert!(
        poll_future_once(interrupted.as_mut()).await.is_none(),
        "rearm-race interrupt settled before the worker acknowledged safe native delivery"
    );
    tokio::task::block_in_place(|| {
        rearm_requested.wait_until_set("sideband interrupt rearm request")
    })?;
    assert!(
        !gates.handler_entered()?,
        "native Ctrl-C must wait until the worker has rearmed its newest observer"
    );

    // Let the current rearm finish. The worker-side interrupt preparation
    // acknowledgment may then rearm once more before the server writes ETX.
    rearm_enabled.reset()?;
    rearm_release.set()?;
    assert_eq!(
        tokio::task::block_in_place(|| gates.wait_for_entry())?,
        NativeControlEvent::CtrlC,
        "rearm race must use native CTRL_C_EVENT"
    );
    assert!(
        poll_future_once(interrupted.as_mut()).await.is_none(),
        "stale readiness settled the interrupt drain while its native handler was still running"
    );

    // The runtime main thread must now join/checkpoint and retry rearming
    // without pausing a second time.
    gates.release()?;
    let interrupt_result = interrupted.as_mut().await?;
    let interrupt_text = result_text(&interrupt_result);
    assert!(
        !interrupt_text.contains("protocol error")
            && !interrupt_text.contains("observer failed")
            && !interrupt_text.contains("KeyboardInterrupt"),
        "finish-to-rearm interrupt did not settle cleanly: {interrupt_text:?}"
    );

    let follow_up = session
        .write_stdin_raw_with(
            "print('REARM_RACE_EVENT_CODES', native_event_codes)\nprint('REARM_RACE_VALUE', rearm_race_value)",
            Some(10.0),
        )
        .await?;
    let follow_up_text = result_text(&follow_up);
    assert!(
        follow_up_text.contains("REARM_RACE_EVENT_CODES [0]")
            && follow_up_text.contains("REARM_RACE_VALUE 42"),
        "session did not recover cleanly from finish-to-rearm race: {follow_up_text:?}"
    );

    let _ = session
        .write_stdin_raw_with(
            "_kernel32.SetConsoleCtrlHandler(_native_handler, False)",
            Some(10.0),
        )
        .await?;
    drop(interrupted);
    session.cancel().await?;
    Ok(())
}

#[cfg(debug_assertions)]
#[tokio::test(flavor = "multi_thread")]
async fn python_background_stdin_defers_interrupt_checkpoint_to_runtime_main() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let rearm_enabled = NamedEvent::new_unique("background-rearm-enabled")?;
    let rearm_entered = NamedEvent::new_unique("background-rearm-entered")?;
    let rearm_release = NamedEvent::new_unique("background-rearm-release")?;
    let rearm_requested = NamedEvent::new_unique("background-rearm-requested")?;
    let completion_ready = NamedEvent::new_unique("background-completion-ready")?;
    let runtime_main_blocked = NamedEvent::new_unique("runtime-main-blocked")?;
    let runtime_main_release = NamedEvent::new_unique("runtime-main-release")?;
    let Some(session) = start_python_session_with_env_vars(vec![
        (
            REARM_GATE_ENABLED_EVENT_ENV.to_string(),
            rearm_enabled.name.clone(),
        ),
        (
            REARM_GATE_ENTERED_EVENT_ENV.to_string(),
            rearm_entered.name.clone(),
        ),
        (
            REARM_GATE_RELEASE_EVENT_ENV.to_string(),
            rearm_release.name.clone(),
        ),
        (
            REARM_REQUESTED_EVENT_ENV.to_string(),
            rearm_requested.name.clone(),
        ),
        (
            COMPLETION_READY_EVENT_ENV.to_string(),
            completion_ready.name.clone(),
        ),
    ])
    .await?
    else {
        return Ok(());
    };
    let gates = NativeHandlerGates::new()?;
    let install_script = gates.python_install_script()?;
    let blocked_name = serde_json::to_string(&runtime_main_blocked.name)?;
    let release_name = serde_json::to_string(&runtime_main_release.name)?;
    let source = format!(
        r#"
exec({})
import threading
native_interrupt_should_set = False
_runtime_main_blocked = _kernel32.OpenEventW(_EVENT_ACCESS, False, {blocked_name})
_runtime_main_release = _kernel32.OpenEventW(_EVENT_ACCESS, False, {release_name})
if not _runtime_main_blocked or not _runtime_main_release:
    raise ctypes.WinError(ctypes.get_last_error())

background_interrupt_answer = None
background_interrupt_waiting = threading.Event()

def background_interrupt_reader():
    global background_interrupt_answer
    background_interrupt_waiting.set()
    background_interrupt_answer = input('background-main-thread> ')
    print('BACKGROUND_MAIN_THREAD_ANSWER', background_interrupt_answer, flush=True)

background_interrupt_thread = threading.Thread(
    target=background_interrupt_reader,
    daemon=True,
)
background_interrupt_thread.start()
background_interrupt_waiting.wait()
_kernel32.SetEvent(_runtime_main_blocked)
_kernel32.WaitForSingleObject(_runtime_main_release, 0xFFFFFFFF)
print('RUNTIME_MAIN_RELEASED', flush=True)
"#,
        serde_json::to_string(&install_script)?
    );

    // Stop the background input_wait publication immediately before rearm.
    // The Python runtime main thread remains blocked in the explicit native
    // wait, so the caller at this boundary is deterministically the managed
    // stdin thread.
    rearm_enabled.set()?;
    let mut cell = Box::pin(session.write_stdin_raw_with(source, Some(1.0)));
    assert!(
        poll_future_once(cell.as_mut()).await.is_none(),
        "background-input cell settled before reaching its deterministic gates"
    );
    tokio::task::block_in_place(|| {
        runtime_main_blocked.wait_until_set("Python runtime main-thread block")
    })?;
    tokio::task::block_in_place(|| {
        rearm_entered.wait_until_set("background managed-stdin rearm boundary")
    })?;
    rearm_requested.reset()?;
    let timed_out = cell.as_mut().await?;
    let timed_out_text = result_text(&timed_out);
    assert!(
        timed_out_text.contains("timed out") || timed_out_text.contains("timeout"),
        "background-input cell must remain pending after its client timeout: {timed_out_text:?}"
    );
    assert!(
        !timed_out_text.contains("RUNTIME_MAIN_RELEASED"),
        "runtime main unexpectedly crossed its explicit block: {timed_out_text:?}"
    );
    drop(cell);

    let mut interrupted = Box::pin(session.write_stdin_raw_unterminated_with("\u{3}", Some(30.0)));
    assert!(
        poll_future_once(interrupted.as_mut()).await.is_none(),
        "background interrupt settled before entering its native handler"
    );
    tokio::task::block_in_place(|| {
        rearm_requested.wait_until_set("background sideband interrupt rearm request")
    })?;
    assert!(
        !gates.handler_entered()?,
        "native Ctrl-C must wait until background publication releases the rearm boundary"
    );

    // Let background publication finish its rearm. The IPC thread can then
    // make the observer newest and acknowledge safe native delivery.
    rearm_enabled.reset()?;
    rearm_release.set()?;
    assert_eq!(
        tokio::task::block_in_place(|| gates.wait_for_entry())?,
        NativeControlEvent::CtrlC,
        "background managed-stdin regression must use native CTRL_C_EVENT"
    );

    // The background caller must defer to the saved runtime main thread, not
    // consume completion, emit interrupt_complete, or check Python signals.
    gates.release()?;
    tokio::task::block_in_place(|| {
        completion_ready.wait_until_set("native handler completion publication")
    })?;
    assert!(
        poll_future_once(interrupted.as_mut()).await.is_none(),
        "interrupt transaction settled before the runtime-main checkpoint"
    );
    runtime_main_release.set()?;

    let interrupt_result = interrupted.as_mut().await?;
    let interrupt_text = result_text(&interrupt_result);
    assert!(
        interrupt_text.contains("RUNTIME_MAIN_RELEASED"),
        "runtime main thread did not own completion after release: {interrupt_text:?}"
    );
    assert!(
        !interrupt_text.contains("protocol error")
            && !interrupt_text.contains("observer failed")
            && !interrupt_text.contains("KeyboardInterrupt"),
        "main-thread checkpoint did not settle the interrupt cleanly: {interrupt_text:?}"
    );

    let answer = session
        .write_stdin_raw_with("background-answer", Some(10.0))
        .await?;
    let answer_text = result_text(&answer);
    assert!(
        answer_text.contains("BACKGROUND_MAIN_THREAD_ANSWER background-answer"),
        "background managed stdin did not resume after the main checkpoint: {answer_text:?}"
    );
    assert!(
        !answer_text.contains("KeyboardInterrupt"),
        "stale Python interrupt reached the background answer: {answer_text:?}"
    );

    let _ = session
        .write_stdin_raw_with(
            "_kernel32.SetConsoleCtrlHandler(_native_handler, False)",
            Some(10.0),
        )
        .await?;
    drop(interrupted);
    session.cancel().await?;
    Ok(())
}

#[cfg(debug_assertions)]
#[tokio::test(flavor = "multi_thread")]
async fn python_background_publication_rechecks_cleanup_before_checkpoint() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let publication_entered = NamedEvent::new_unique("publication-entered")?;
    let publication_release = NamedEvent::new_unique("publication-release")?;
    let publication_deferred = NamedEvent::new_unique("publication-deferred")?;
    let publication_sent = NamedEvent::new_unique("publication-sent")?;
    let checkpoint_entered = NamedEvent::new_unique("checkpoint-entered")?;
    let checkpoint_release = NamedEvent::new_unique("checkpoint-release")?;
    let completion_ready = NamedEvent::new_unique("publication-completion-ready")?;
    let runtime_main_blocked = NamedEvent::new_unique("publication-runtime-main-blocked")?;
    let runtime_main_release = NamedEvent::new_unique("publication-runtime-main-release")?;
    let Some(session) = start_python_session_with_env_vars(vec![
        (
            SIDEBAND_PUBLICATION_GATE_ENTERED_EVENT_ENV.to_string(),
            publication_entered.name.clone(),
        ),
        (
            SIDEBAND_PUBLICATION_GATE_RELEASE_EVENT_ENV.to_string(),
            publication_release.name.clone(),
        ),
        (
            SIDEBAND_PUBLICATION_DEFERRED_EVENT_ENV.to_string(),
            publication_deferred.name.clone(),
        ),
        (
            SIDEBAND_PUBLICATION_SENT_EVENT_ENV.to_string(),
            publication_sent.name.clone(),
        ),
        (
            PYTHON_CHECKPOINT_GATE_ENTERED_EVENT_ENV.to_string(),
            checkpoint_entered.name.clone(),
        ),
        (
            PYTHON_CHECKPOINT_GATE_RELEASE_EVENT_ENV.to_string(),
            checkpoint_release.name.clone(),
        ),
        (
            COMPLETION_READY_EVENT_ENV.to_string(),
            completion_ready.name.clone(),
        ),
    ])
    .await?
    else {
        return Ok(());
    };
    let gates = NativeHandlerGates::new()?;
    let install_script = gates.python_install_script()?;
    let blocked_name = serde_json::to_string(&runtime_main_blocked.name)?;
    let release_name = serde_json::to_string(&runtime_main_release.name)?;
    let source = format!(
        r#"
exec({})
import threading
native_interrupt_should_set = False
_publication_runtime_main_blocked = _kernel32.OpenEventW(
    _EVENT_ACCESS, False, {blocked_name}
)
_publication_runtime_main_release = _kernel32.OpenEventW(
    _EVENT_ACCESS, False, {release_name}
)
if not _publication_runtime_main_blocked or not _publication_runtime_main_release:
    raise ctypes.WinError(ctypes.get_last_error())

publication_guard_answer = None
publication_guard_waiting = threading.Event()

def publication_guard_reader():
    global publication_guard_answer
    publication_guard_waiting.set()
    publication_guard_answer = input('publication-guard> ')
    print('PUBLICATION_GUARD_ANSWER', publication_guard_answer, flush=True)

publication_guard_thread = threading.Thread(
    target=publication_guard_reader,
    daemon=True,
)
publication_guard_thread.start()
publication_guard_waiting.wait()
_kernel32.SetEvent(_publication_runtime_main_blocked)
_kernel32.WaitForSingleObject(_publication_runtime_main_release, 0xFFFFFFFF)
print('PUBLICATION_RUNTIME_MAIN_RELEASED', flush=True)
"#,
        serde_json::to_string(&install_script)?
    );

    let mut cell = Box::pin(session.write_stdin_raw_with(source, Some(1.0)));
    assert!(
        poll_future_once(cell.as_mut()).await.is_none(),
        "publication-race cell settled before reaching its deterministic gates"
    );
    tokio::task::block_in_place(|| {
        runtime_main_blocked.wait_until_set("publication-race runtime main block")
    })?;
    tokio::task::block_in_place(|| {
        publication_entered.wait_until_set("background sideband pre-commit boundary")
    })?;
    let timed_out = cell.as_mut().await?;
    let timed_out_text = result_text(&timed_out);
    assert!(
        timed_out_text.contains("timed out") || timed_out_text.contains("timeout"),
        "publication-race cell must remain pending after its client timeout: {timed_out_text:?}"
    );
    assert!(
        !timed_out_text.contains("PUBLICATION_RUNTIME_MAIN_RELEASED"),
        "runtime main unexpectedly crossed its explicit block: {timed_out_text:?}"
    );
    drop(cell);

    let mut interrupted = Box::pin(session.write_stdin_raw_unterminated_with("\u{3}", Some(30.0)));
    assert!(
        poll_future_once(interrupted.as_mut()).await.is_none(),
        "publication-race interrupt settled before native delivery"
    );
    assert_eq!(
        tokio::task::block_in_place(|| gates.wait_for_entry())?,
        NativeControlEvent::CtrlC,
        "publication-race regression must use native CTRL_C_EVENT"
    );

    gates.release()?;
    tokio::task::block_in_place(|| {
        completion_ready.wait_until_set("publication-race native handler completion")
    })?;
    assert!(
        poll_future_once(interrupted.as_mut()).await.is_none(),
        "background thread consumed completion before the runtime main checkpoint"
    );

    runtime_main_release.set()?;
    tokio::task::block_in_place(|| {
        checkpoint_entered.wait_until_set("Python pre-PyErr_CheckSignals checkpoint")
    })?;
    assert!(
        !publication_sent.is_set()?,
        "background readiness was published before its final cleanup recheck"
    );

    publication_release.set()?;
    tokio::task::block_in_place(|| {
        publication_deferred.wait_until_set("background readiness cleanup deferral")
    })?;
    assert!(
        !publication_sent.is_set()?,
        "background readiness overtook the held runtime checkpoint"
    );
    assert!(
        poll_future_once(interrupted.as_mut()).await.is_none(),
        "interrupt settled from stale readiness before PyErr_CheckSignals"
    );

    checkpoint_release.set()?;
    let interrupt_result = interrupted.as_mut().await?;
    let interrupt_text = result_text(&interrupt_result);
    assert!(
        interrupt_text.contains("PUBLICATION_RUNTIME_MAIN_RELEASED"),
        "runtime main did not reach its owned checkpoint: {interrupt_text:?}"
    );
    assert!(
        !interrupt_text.contains("protocol error")
            && !interrupt_text.contains("observer failed")
            && !interrupt_text.contains("KeyboardInterrupt"),
        "publication/checkpoint transaction did not settle cleanly: {interrupt_text:?}"
    );
    tokio::task::block_in_place(|| {
        publication_sent.wait_until_set("post-checkpoint background readiness publication")
    })?;

    let answer = session
        .write_stdin_raw_with("publication-answer", Some(10.0))
        .await?;
    let answer_text = result_text(&answer);
    assert!(
        answer_text.contains("PUBLICATION_GUARD_ANSWER publication-answer"),
        "background managed stdin did not resume after the checkpoint: {answer_text:?}"
    );
    assert!(
        !answer_text.contains("KeyboardInterrupt"),
        "stale Python interrupt reached the post-checkpoint answer: {answer_text:?}"
    );

    let _ = session
        .write_stdin_raw_with(
            "_kernel32.SetConsoleCtrlHandler(_native_handler, False)",
            Some(10.0),
        )
        .await?;
    drop(interrupted);
    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn r_native_interrupts_execution_and_managed_input_without_stale_state() -> TestResult<()> {
    let _guard = lock_test_mutex();
    let Some(session) = start_r_session().await? else {
        return Ok(());
    };

    let running = session
        .write_stdin_raw_with(
            r#"
cat("R_EXECUTION_WAITING\n")
flush.console()
tryCatch(
  repeat Sys.sleep(1),
  interrupt = function(e) cat("R_EXECUTION_INTERRUPTED\n")
)
"#,
            Some(0.5),
        )
        .await?;
    let running_text = result_text(&running);
    assert!(
        running_text.contains("R_EXECUTION_WAITING") && is_busy_response(&running_text),
        "expected R to be executing before Ctrl-C: {running_text:?}"
    );

    let running_interrupt = session
        .write_stdin_raw_unterminated_with("\u{3}", Some(10.0))
        .await?;
    let running_interrupt_text = result_text(&running_interrupt);
    assert!(
        running_interrupt_text.contains("R_EXECUTION_INTERRUPTED"),
        "ordinary R execution interrupt condition did not run: {running_interrupt_text:?}"
    );

    let input_wait = session
        .write_stdin_raw_with(
            r#"
tryCatch(
  {
    value <- readline("r-native-input> ")
    cat("R_UNEXPECTED_INPUT", value, "\n")
  },
  interrupt = function(e) cat("R_INPUT_INTERRUPTED\n")
)
"#,
            Some(10.0),
        )
        .await?;
    let input_wait_text = result_text(&input_wait);
    assert!(
        input_wait_text.contains("r-native-input> "),
        "expected R managed ReadConsole wait: {input_wait_text:?}"
    );

    let input_interrupt = session
        .write_stdin_raw_unterminated_with("\u{3}", Some(10.0))
        .await?;
    let input_interrupt_text = result_text(&input_interrupt);
    assert!(
        input_interrupt_text.contains("R_INPUT_INTERRUPTED"),
        "R managed input did not process its ordinary interrupt condition: {input_interrupt_text:?}"
    );
    assert!(
        !input_interrupt_text.contains("R_UNEXPECTED_INPUT"),
        "managed input returned data instead of interrupting: {input_interrupt_text:?}"
    );

    let follow_up = session
        .write_stdin_raw_with("cat('R_NATIVE_FOLLOWUP_OK\\n')", Some(10.0))
        .await?;
    let follow_up_text = result_text(&follow_up);
    assert!(
        follow_up_text.contains("R_NATIVE_FOLLOWUP_OK"),
        "R session did not recover after sequential interrupts: {follow_up_text:?}"
    );
    assert!(
        !follow_up_text.contains("interrupt"),
        "a stale R interrupt reached the next evaluation: {follow_up_text:?}"
    );

    session.cancel().await?;
    Ok(())
}
