//! Completion observation for native Windows Ctrl-C handler dispatch.
//!
//! Windows invokes console-control handlers on a dedicated thread and walks the
//! handler list newest-first. Runtime integrations are allowed to consume
//! `CTRL_C_EVENT`, so observing the event itself is not enough: the runtime main
//! thread must not inspect pending interrupt state until the whole handler chain
//! has returned. This module registers a newest-first observer which duplicates
//! the current handler-thread handle, returns `FALSE`, and lets a watcher wait
//! for that thread to terminate.
//!
//! The handler deliberately performs only atomics and small Win32 calls. In
//! particular, it does not allocate, lock, log, or call into R/Python.

#![allow(unsafe_op_in_unsafe_fn)]

use std::error::Error;
use std::ffi::c_void;
use std::fmt;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
#[cfg(debug_assertions)]
use std::{ffi::OsStr, os::windows::ffi::OsStrExt};

use windows_sys::Win32::Foundation::{
    CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, GetLastError, HANDLE, WAIT_FAILED,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Console::{CTRL_C_EVENT, SetConsoleCtrlHandler};
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, GetCurrentThread, GetCurrentThreadId, INFINITE, SetEvent,
    WaitForMultipleObjects, WaitForSingleObject,
};
#[cfg(debug_assertions)]
use windows_sys::Win32::System::Threading::{
    EVENT_MODIFY_STATE, OpenEventW, SYNCHRONIZATION_SYNCHRONIZE,
};

static OBSERVER: OnceLock<Result<InterruptObserver, ObserverError>> = OnceLock::new();

// These values are published before the observer is registered. They remain
// valid for the process lifetime after successful initialization.
static HANDOFF_EVENT: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static COMPLETION_EVENT: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static FAILURE_EVENT: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static CLEANUP_PENDING: AtomicBool = AtomicBool::new(false);
static HANDLER_THREAD: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static HANDLER_THREAD_ID: AtomicU32 = AtomicU32::new(0);
static INTERRUPT_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

static FAILURE_KIND: AtomicU32 = AtomicU32::new(FailureKind::None as u32);
static FAILURE_CODE: AtomicU32 = AtomicU32::new(0);

const FAILURE_RECORDING: u32 = u32::MAX;
const TEST_INITIALIZATION_FAILURE_ENV: &str = "MCP_REPL_TEST_WINDOWS_OBSERVER_INIT_FAILURE";

// Distinct volatile-read targets keep the alternating callbacks observably
// different under optimized-code identical-function folding.
#[used]
static OBSERVER_A_IDENTITY: u8 = 0xA5;
#[used]
static OBSERVER_B_IDENTITY: u8 = 0x5A;
type ConsoleCtrlHandler = unsafe extern "system" fn(u32) -> i32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
enum FailureKind {
    None = 0,
    CreateHandoffEvent = 1,
    CreateCompletionEvent = 2,
    CreateFailureEvent = 3,
    CreateInputEvent = 4,
    ClearInheritedIgnore = 5,
    RegisterObserver = 6,
    SpawnWatcher = 7,
    NotInitialized = 8,
    HandlerOverlap = 9,
    DuplicateHandlerThread = 10,
    PublishHandlerThread = 11,
    SignalHandoff = 12,
    WaitForHandoff = 13,
    MissingHandlerThread = 14,
    WaitForHandlerThread = 15,
    CloseHandlerThread = 16,
    SignalCompletion = 17,
    SignalFailure = 18,
    RemoveObserver = 20,
    RearmObserver = 21,
    SignalInput = 22,
    WaitForActivity = 23,
    CompletionWithoutInterrupt = 24,
    CreateCleanupEvent = 25,
    CleanupOverlap = 26,
    SignalCleanup = 27,
    WaitForCleanup = 28,
    CompletionWithoutCleanup = 29,
    ObserverIdentity = 30,
    TestInitialization = 31,
}

impl FailureKind {
    fn from_raw(raw: u32) -> Self {
        match raw {
            1 => Self::CreateHandoffEvent,
            2 => Self::CreateCompletionEvent,
            3 => Self::CreateFailureEvent,
            4 => Self::CreateInputEvent,
            5 => Self::ClearInheritedIgnore,
            6 => Self::RegisterObserver,
            7 => Self::SpawnWatcher,
            8 => Self::NotInitialized,
            9 => Self::HandlerOverlap,
            10 => Self::DuplicateHandlerThread,
            11 => Self::PublishHandlerThread,
            12 => Self::SignalHandoff,
            13 => Self::WaitForHandoff,
            14 => Self::MissingHandlerThread,
            15 => Self::WaitForHandlerThread,
            16 => Self::CloseHandlerThread,
            17 => Self::SignalCompletion,
            18 => Self::SignalFailure,
            20 => Self::RemoveObserver,
            21 => Self::RearmObserver,
            22 => Self::SignalInput,
            23 => Self::WaitForActivity,
            24 => Self::CompletionWithoutInterrupt,
            25 => Self::CreateCleanupEvent,
            26 => Self::CleanupOverlap,
            27 => Self::SignalCleanup,
            28 => Self::WaitForCleanup,
            29 => Self::CompletionWithoutCleanup,
            30 => Self::ObserverIdentity,
            31 => Self::TestInitialization,
            _ => Self::SignalFailure,
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::None => "no observer failure",
            Self::CreateHandoffEvent => "create Ctrl-C handler handoff event",
            Self::CreateCompletionEvent => "create Ctrl-C completion event",
            Self::CreateFailureEvent => "create Ctrl-C observer failure event",
            Self::CreateInputEvent => "create runtime input notification event",
            Self::ClearInheritedIgnore => "clear inherited Ctrl-C ignore state",
            Self::RegisterObserver => "register Ctrl-C completion observer",
            Self::SpawnWatcher => "spawn Ctrl-C completion watcher",
            Self::NotInitialized => "use Ctrl-C completion observer before initialization",
            Self::HandlerOverlap => "observe overlapping Ctrl-C handler dispatches",
            Self::DuplicateHandlerThread => "duplicate Ctrl-C handler thread handle",
            Self::PublishHandlerThread => "publish Ctrl-C handler thread handle",
            Self::SignalHandoff => "signal Ctrl-C handler-thread handoff",
            Self::WaitForHandoff => "wait for Ctrl-C handler-thread handoff",
            Self::MissingHandlerThread => "receive handoff without a handler thread",
            Self::WaitForHandlerThread => "wait for Ctrl-C handler completion",
            Self::CloseHandlerThread => "close duplicated Ctrl-C handler thread handle",
            Self::SignalCompletion => "signal Ctrl-C handler completion",
            Self::SignalFailure => "signal Ctrl-C observer failure",
            Self::RemoveObserver => "remove Ctrl-C observer before rearming",
            Self::RearmObserver => "re-register Ctrl-C observer newest-first",
            Self::SignalInput => "signal runtime input availability",
            Self::WaitForActivity => "wait for runtime input or Ctrl-C completion",
            Self::CompletionWithoutInterrupt => {
                "consume Ctrl-C completion without an in-flight interrupt"
            }
            Self::CreateCleanupEvent => "create interrupt cleanup event",
            Self::CleanupOverlap => "observe overlapping interrupt cleanup",
            Self::SignalCleanup => "signal interrupt cleanup completion",
            Self::WaitForCleanup => "wait for interrupt cleanup completion",
            Self::CompletionWithoutCleanup => "consume Ctrl-C completion without sideband cleanup",
            Self::ObserverIdentity => "preserve distinct Ctrl-C observer callback identities",
            Self::TestInitialization => "inject a test Ctrl-C observer initialization failure",
        }
    }
}

/// A fail-closed observer error.
///
/// The diagnostic includes a Win32 error code when the failed operation sets
/// one. Invariant failures use zero; unexpected waits use the raw wait result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ObserverError {
    kind: FailureKind,
    code: u32,
}

impl fmt::Display for ObserverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.code == 0 {
            write!(
                f,
                "Windows Ctrl-C observer failed to {}",
                self.kind.description()
            )
        } else {
            write!(
                f,
                "Windows Ctrl-C observer failed to {} (Win32 code {})",
                self.kind.description(),
                self.code
            )
        }
    }
}

impl Error for ObserverError {}

/// The activity which woke a managed runtime wait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WaitOutcome {
    Input,
    InterruptCompleted,
}

/// Whether the observer was made newest or a native dispatch must be joined
/// before the caller retries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RearmOutcome {
    Rearmed,
    InterruptInFlight,
}

struct InterruptObserver {
    handoff_event: HANDLE,
    completion_event: HANDLE,
    failure_event: HANDLE,
    input_event: HANDLE,
    cleanup_event: HANDLE,
    rearm_guard: Mutex<()>,
    registration_a_active: AtomicBool,
}

// Win32 event handles may be waited on and signaled from different threads.
unsafe impl Send for InterruptObserver {}
unsafe impl Sync for InterruptObserver {}

impl InterruptObserver {
    fn new() -> Result<Self, ObserverError> {
        let observer_a: ConsoleCtrlHandler = ctrl_c_completion_observer_a;
        if cfg!(debug_assertions) && std::env::var_os(TEST_INITIALIZATION_FAILURE_ENV).is_some() {
            return Err(ObserverError {
                kind: FailureKind::TestInitialization,
                code: 0,
            });
        }

        let observer_b: ConsoleCtrlHandler = ctrl_c_completion_observer_b;
        if ptr::fn_addr_eq(observer_a, observer_b) {
            return Err(ObserverError {
                kind: FailureKind::ObserverIdentity,
                code: 0,
            });
        }

        let handoff_event = create_event(false, FailureKind::CreateHandoffEvent)?;
        let completion_event = match create_event(false, FailureKind::CreateCompletionEvent) {
            Ok(handle) => handle,
            Err(err) => {
                close_handle(handoff_event);
                return Err(err);
            }
        };
        let failure_event = match create_event(true, FailureKind::CreateFailureEvent) {
            Ok(handle) => handle,
            Err(err) => {
                close_handle(completion_event);
                close_handle(handoff_event);
                return Err(err);
            }
        };
        let input_event = match create_event(false, FailureKind::CreateInputEvent) {
            Ok(handle) => handle,
            Err(err) => {
                close_handle(failure_event);
                close_handle(completion_event);
                close_handle(handoff_event);
                return Err(err);
            }
        };
        let cleanup_event = match create_event(false, FailureKind::CreateCleanupEvent) {
            Ok(handle) => handle,
            Err(err) => {
                close_handle(input_event);
                close_handle(failure_event);
                close_handle(completion_event);
                close_handle(handoff_event);
                return Err(err);
            }
        };

        let observer = Self {
            handoff_event,
            completion_event,
            failure_event,
            input_event,
            cleanup_event,
            rearm_guard: Mutex::new(()),
            registration_a_active: AtomicBool::new(true),
        };
        observer.publish_handler_state();

        // CREATE_NEW_PROCESS_GROUP makes Ctrl-C ignored by default. Restore
        // normal handling only after final console attachment and before adding
        // our observer.
        if unsafe { SetConsoleCtrlHandler(None, 0) } == 0 {
            let err = sync_error(FailureKind::ClearInheritedIgnore);
            observer.unpublish_handler_state();
            observer.close_all();
            return Err(err);
        }

        let watcher = match std::thread::Builder::new()
            .name("mcp-repl-ctrl-c-observer".to_string())
            .spawn(completion_watcher)
        {
            Ok(watcher) => watcher,
            Err(_) => {
                let err = ObserverError {
                    kind: FailureKind::SpawnWatcher,
                    code: 0,
                };
                observer.unpublish_handler_state();
                observer.close_all();
                return Err(err);
            }
        };

        if unsafe { SetConsoleCtrlHandler(Some(ctrl_c_completion_observer_a), 1) } == 0 {
            let err = poison(FailureKind::RegisterObserver, unsafe { GetLastError() });
            let _ = watcher.join();
            observer.unpublish_handler_state();
            observer.close_all();
            return Err(err);
        }
        drop(watcher);

        Ok(observer)
    }

    fn publish_handler_state(&self) {
        HANDOFF_EVENT.store(self.handoff_event, Ordering::Release);
        COMPLETION_EVENT.store(self.completion_event, Ordering::Release);
        FAILURE_EVENT.store(self.failure_event, Ordering::Release);
        HANDLER_THREAD.store(ptr::null_mut(), Ordering::Release);
        HANDLER_THREAD_ID.store(0, Ordering::Release);
        INTERRUPT_IN_FLIGHT.store(false, Ordering::Release);
        CLEANUP_PENDING.store(false, Ordering::Release);
        FAILURE_CODE.store(0, Ordering::Release);
        FAILURE_KIND.store(FailureKind::None as u32, Ordering::Release);
    }

    fn unpublish_handler_state(&self) {
        HANDOFF_EVENT.store(ptr::null_mut(), Ordering::Release);
        COMPLETION_EVENT.store(ptr::null_mut(), Ordering::Release);
        FAILURE_EVENT.store(ptr::null_mut(), Ordering::Release);
    }

    fn close_all(&self) {
        close_handle(self.cleanup_event);
        close_handle(self.input_event);
        close_handle(self.failure_event);
        close_handle(self.completion_event);
        close_handle(self.handoff_event);
    }
}

/// Initializes the process-global observer after the worker has attached its
/// final console and before either runtime installs its handlers.
pub(crate) fn initialize_after_console_attach() -> Result<(), ObserverError> {
    match OBSERVER.get_or_init(InterruptObserver::new) {
        Ok(_) => Ok(()),
        Err(err) => Err(*err),
    }
}

/// Adds an alternate observer, then removes the old one, so an observer remains
/// registered while becoming newest in the console-handler list.
///
/// Runtime integrations such as reticulate can remove and re-add their own
/// handler after every Ctrl-C. Call this immediately before each managed wait
/// or readiness publication. During the brief overlap both observer callbacks
/// may run on the same handler thread; the second invocation is a no-op.
pub(crate) fn rearm() -> Result<RearmOutcome, ObserverError> {
    let observer = initialized_observer()?;
    #[cfg(debug_assertions)]
    signal_test_rearm_requested()?;
    let _rearm_guard = observer
        .rearm_guard
        .lock()
        .map_err(|_| poison(FailureKind::RearmObserver, 0))?;
    check_failure()?;

    #[cfg(debug_assertions)]
    wait_at_test_rearm_boundary()?;
    if INTERRUPT_IN_FLIGHT.load(Ordering::Acquire) {
        return Ok(RearmOutcome::InterruptInFlight);
    }

    let current_is_a = observer.registration_a_active.load(Ordering::Acquire);
    let add_result = if current_is_a {
        unsafe { SetConsoleCtrlHandler(Some(ctrl_c_completion_observer_b), 1) }
    } else {
        unsafe { SetConsoleCtrlHandler(Some(ctrl_c_completion_observer_a), 1) }
    };
    if add_result == 0 {
        return Err(poison(FailureKind::RearmObserver, unsafe {
            GetLastError()
        }));
    }

    observer
        .registration_a_active
        .store(!current_is_a, Ordering::Release);
    let remove_result = if current_is_a {
        unsafe { SetConsoleCtrlHandler(Some(ctrl_c_completion_observer_a), 0) }
    } else {
        unsafe { SetConsoleCtrlHandler(Some(ctrl_c_completion_observer_b), 0) }
    };
    if remove_result == 0 {
        return Err(poison(FailureKind::RemoveObserver, unsafe {
            GetLastError()
        }));
    }

    // A dispatch can begin after the precheck or during add-before-remove.
    // Registration is newest-first now, but readiness must still wait for the
    // runtime-main caller to join/checkpoint and retry rearming.
    check_failure()?;
    if INTERRUPT_IN_FLIGHT.load(Ordering::Acquire) {
        Ok(RearmOutcome::InterruptInFlight)
    } else {
        Ok(RearmOutcome::Rearmed)
    }
}

#[cfg(debug_assertions)]
fn signal_test_rearm_requested() -> Result<(), ObserverError> {
    const EVENT_ENV: &str = "MCP_REPL_TEST_WINDOWS_REARM_REQUESTED_EVENT";

    let Some(event) = open_test_event(EVENT_ENV)? else {
        return Ok(());
    };
    if unsafe { SetEvent(event) } == 0 {
        let code = unsafe { GetLastError() };
        close_handle(event);
        return Err(poison(FailureKind::RearmObserver, code));
    }
    close_handle(event);
    Ok(())
}

#[cfg(debug_assertions)]
fn wait_at_test_rearm_boundary() -> Result<(), ObserverError> {
    const ENABLED_ENV: &str = "MCP_REPL_TEST_WINDOWS_REARM_GATE_ENABLED_EVENT";
    const ENTERED_ENV: &str = "MCP_REPL_TEST_WINDOWS_REARM_GATE_ENTERED_EVENT";
    const RELEASE_ENV: &str = "MCP_REPL_TEST_WINDOWS_REARM_GATE_RELEASE_EVENT";

    let Some(enabled) = open_test_event(ENABLED_ENV)? else {
        return Ok(());
    };
    let enabled_wait = unsafe { WaitForSingleObject(enabled, 0) };
    close_handle(enabled);
    match enabled_wait {
        WAIT_TIMEOUT => return Ok(()),
        WAIT_OBJECT_0 => {}
        WAIT_FAILED => {
            return Err(poison(FailureKind::RearmObserver, unsafe {
                GetLastError()
            }));
        }
        other => return Err(poison(FailureKind::RearmObserver, other)),
    }

    let Some(entered) = open_test_event(ENTERED_ENV)? else {
        return Err(poison(FailureKind::RearmObserver, 0));
    };
    let Some(release) = open_test_event(RELEASE_ENV)? else {
        close_handle(entered);
        return Err(poison(FailureKind::RearmObserver, 0));
    };

    if unsafe { SetEvent(entered) } == 0 {
        let code = unsafe { GetLastError() };
        close_handle(release);
        close_handle(entered);
        return Err(poison(FailureKind::RearmObserver, code));
    }

    let release_wait = unsafe { WaitForSingleObject(release, INFINITE) };
    close_handle(release);
    close_handle(entered);
    match release_wait {
        WAIT_OBJECT_0 => Ok(()),
        WAIT_FAILED => Err(poison(FailureKind::RearmObserver, unsafe {
            GetLastError()
        })),
        other => Err(poison(FailureKind::RearmObserver, other)),
    }
}

#[cfg(debug_assertions)]
fn open_test_event(env: &str) -> Result<Option<HANDLE>, ObserverError> {
    let Some(name) = std::env::var_os(env).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let wide_name = OsStr::new(&name)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let event = unsafe {
        OpenEventW(
            EVENT_MODIFY_STATE | SYNCHRONIZATION_SYNCHRONIZE,
            0,
            wide_name.as_ptr(),
        )
    };
    if event.is_null() {
        Err(poison(FailureKind::RearmObserver, unsafe {
            GetLastError()
        }))
    } else {
        Ok(Some(event))
    }
}

#[cfg(debug_assertions)]
fn signal_test_completion_ready() -> Result<(), ObserverError> {
    const EVENT_ENV: &str = "MCP_REPL_TEST_WINDOWS_COMPLETION_READY_EVENT";

    let Some(event) = open_test_event(EVENT_ENV)? else {
        return Ok(());
    };
    if unsafe { SetEvent(event) } == 0 {
        let code = unsafe { GetLastError() };
        close_handle(event);
        return Err(poison(FailureKind::SignalCompletion, code));
    }
    close_handle(event);
    Ok(())
}

#[cfg(debug_assertions)]
pub(crate) fn wait_at_test_sideband_publication_boundary() -> Result<(), ObserverError> {
    wait_at_test_gate(
        "MCP_REPL_TEST_WINDOWS_SIDEBAND_PUBLICATION_GATE_ENTERED_EVENT",
        "MCP_REPL_TEST_WINDOWS_SIDEBAND_PUBLICATION_GATE_RELEASE_EVENT",
        FailureKind::WaitForActivity,
    )
}

#[cfg(debug_assertions)]
pub(crate) fn signal_test_sideband_publication_sent() -> Result<(), ObserverError> {
    signal_test_event(
        "MCP_REPL_TEST_WINDOWS_SIDEBAND_PUBLICATION_SENT_EVENT",
        FailureKind::SignalInput,
    )
}

#[cfg(debug_assertions)]
pub(crate) fn signal_test_sideband_publication_deferred() -> Result<(), ObserverError> {
    signal_test_event(
        "MCP_REPL_TEST_WINDOWS_SIDEBAND_PUBLICATION_DEFERRED_EVENT",
        FailureKind::SignalInput,
    )
}

#[cfg(debug_assertions)]
pub(crate) fn wait_at_test_python_checkpoint_boundary() -> Result<(), ObserverError> {
    wait_at_test_gate(
        "MCP_REPL_TEST_WINDOWS_PYTHON_CHECKPOINT_GATE_ENTERED_EVENT",
        "MCP_REPL_TEST_WINDOWS_PYTHON_CHECKPOINT_GATE_RELEASE_EVENT",
        FailureKind::WaitForActivity,
    )
}

#[cfg(debug_assertions)]
fn wait_at_test_gate(
    entered_env: &str,
    release_env: &str,
    failure_kind: FailureKind,
) -> Result<(), ObserverError> {
    let Some(entered) = open_test_event(entered_env)? else {
        return Ok(());
    };
    let Some(release) = open_test_event(release_env)? else {
        close_handle(entered);
        return Err(poison(failure_kind, 0));
    };

    if unsafe { SetEvent(entered) } == 0 {
        let code = unsafe { GetLastError() };
        close_handle(release);
        close_handle(entered);
        return Err(poison(failure_kind, code));
    }

    let wait = unsafe { WaitForSingleObject(release, INFINITE) };
    close_handle(release);
    close_handle(entered);
    match wait {
        WAIT_OBJECT_0 => Ok(()),
        WAIT_FAILED => Err(poison(failure_kind, unsafe { GetLastError() })),
        other => Err(poison(failure_kind, other)),
    }
}

#[cfg(debug_assertions)]
fn signal_test_event(env: &str, failure_kind: FailureKind) -> Result<(), ObserverError> {
    let Some(event) = open_test_event(env)? else {
        return Ok(());
    };
    if unsafe { SetEvent(event) } == 0 {
        let code = unsafe { GetLastError() };
        close_handle(event);
        return Err(poison(failure_kind, code));
    }
    close_handle(event);
    Ok(())
}

/// Signals that a runtime queue predicate may now be true.
///
/// The event is auto-reset and notifications may coalesce. Consumers must
/// inspect the queue predicate before blocking and again after this wake.
pub(crate) fn notify_input() -> Result<(), ObserverError> {
    let observer = initialized_observer()?;
    check_failure()?;
    if unsafe { SetEvent(observer.input_event) } == 0 {
        return Err(poison(FailureKind::SignalInput, unsafe { GetLastError() }));
    }
    Ok(())
}

/// Records that the cleanup-only interrupt sideband has drained queued input.
///
/// This does not wake the runtime and does not create interrupt state. Native
/// console delivery remains the sole runtime interrupt authority.
pub(crate) fn notify_interrupt_cleanup_complete() -> Result<(), ObserverError> {
    let observer = initialized_observer()?;
    check_failure()?;
    if CLEANUP_PENDING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(poison(FailureKind::CleanupOverlap, 0));
    }
    if unsafe { SetEvent(observer.cleanup_event) } == 0 {
        return Err(poison(FailureKind::SignalCleanup, unsafe {
            GetLastError()
        }));
    }
    Ok(())
}

/// Blocks until input arrives, Ctrl-C handler dispatch completes, or the
/// observer fails.
///
/// Failure has priority over completion, and completion has priority over
/// input. Returning `InterruptCompleted` consumes the completion signal and
/// clears the overlap guard; the runtime may then perform its native interrupt
/// checkpoint on its main thread.
pub(crate) fn wait_for_activity() -> Result<WaitOutcome, ObserverError> {
    let observer = initialized_observer()?;
    loop {
        check_failure()?;

        let handles = [
            observer.failure_event,
            observer.completion_event,
            observer.input_event,
        ];
        let wait =
            unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, INFINITE) };
        match wait {
            WAIT_OBJECT_0 => {
                return Err(current_failure().unwrap_or(ObserverError {
                    kind: FailureKind::SignalFailure,
                    code: 0,
                }));
            }
            value if value == WAIT_OBJECT_0 + 1 => {
                wait_for_cleanup(observer)?;
                consume_completion()?;
                return Ok(WaitOutcome::InterruptCompleted);
            }
            value if value == WAIT_OBJECT_0 + 2 => {
                // Do not let queued input overtake a handler still in flight.
                // The queue itself remains the durable predicate after this
                // auto-reset notification is consumed.
                if !INTERRUPT_IN_FLIGHT.load(Ordering::Acquire) {
                    return Ok(WaitOutcome::Input);
                }
            }
            WAIT_FAILED => {
                return Err(poison(FailureKind::WaitForActivity, unsafe {
                    GetLastError()
                }));
            }
            other => return Err(poison(FailureKind::WaitForActivity, other)),
        }
    }
}

/// Non-blockingly consumes a completed Ctrl-C dispatch.
///
/// This is useful before consuming already-queued runtime input so a completed
/// native interrupt checkpoint wins when both are ready.
pub(crate) fn take_completed_interrupt() -> Result<bool, ObserverError> {
    let observer = initialized_observer()?;
    check_failure()?;

    let handles = [observer.failure_event, observer.completion_event];
    let wait = unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, 0) };
    match wait {
        WAIT_OBJECT_0 => Err(current_failure().unwrap_or(ObserverError {
            kind: FailureKind::SignalFailure,
            code: 0,
        })),
        value if value == WAIT_OBJECT_0 + 1 => {
            wait_for_cleanup(observer)?;
            consume_completion()?;
            Ok(true)
        }
        WAIT_TIMEOUT => Ok(false),
        WAIT_FAILED => Err(poison(FailureKind::WaitForActivity, unsafe {
            GetLastError()
        })),
        other => Err(poison(FailureKind::WaitForActivity, other)),
    }
}

/// Finishes a handler dispatch which may have started during runtime execution.
///
/// If no dispatch is in flight, this delegates to the nonblocking completion
/// check. Otherwise it waits for failure or completion. Input notifications are
/// provisionally consumed but never returned ahead of handler completion.
pub(crate) fn finish_in_flight_interrupt() -> Result<bool, ObserverError> {
    let observer = initialized_observer()?;
    check_failure()?;

    if !INTERRUPT_IN_FLIGHT.load(Ordering::Acquire) {
        return take_completed_interrupt();
    }

    loop {
        check_failure()?;
        let handles = [
            observer.failure_event,
            observer.completion_event,
            observer.input_event,
        ];
        let wait =
            unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, INFINITE) };
        match wait {
            WAIT_OBJECT_0 => {
                return Err(current_failure().unwrap_or(ObserverError {
                    kind: FailureKind::SignalFailure,
                    code: 0,
                }));
            }
            value if value == WAIT_OBJECT_0 + 1 => {
                wait_for_cleanup(observer)?;
                consume_completion()?;
                return Ok(true);
            }
            value if value == WAIT_OBJECT_0 + 2 => {}
            WAIT_FAILED => {
                return Err(poison(FailureKind::WaitForActivity, unsafe {
                    GetLastError()
                }));
            }
            other => return Err(poison(FailureKind::WaitForActivity, other)),
        }
    }
}

#[inline(never)]
unsafe extern "system" fn ctrl_c_completion_observer_a(event: u32) -> i32 {
    unsafe { observe_ctrl_c_completion(event, ptr::addr_of!(OBSERVER_A_IDENTITY)) }
}

#[inline(never)]
unsafe extern "system" fn ctrl_c_completion_observer_b(event: u32) -> i32 {
    unsafe { observe_ctrl_c_completion(event, ptr::addr_of!(OBSERVER_B_IDENTITY)) }
}

unsafe fn observe_ctrl_c_completion(event: u32, identity: *const u8) -> i32 {
    // Keep a distinct relocation in each wrapper even under release LTO/ICF.
    let _ = unsafe { ptr::read_volatile(identity) };
    if event != CTRL_C_EVENT {
        return 0;
    }

    let current_thread_id = unsafe { GetCurrentThreadId() };
    if INTERRUPT_IN_FLIGHT
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        // Add-before-remove rearming briefly registers both observer entry
        // points. Windows invokes both on the same handler thread, so only the
        // first performs the handoff. A different thread ID is a genuinely
        // overlapping native dispatch and remains a fail-closed error.
        if HANDLER_THREAD_ID.load(Ordering::Acquire) == current_thread_id {
            return 0;
        }
        handler_failure(FailureKind::HandlerOverlap, 0);
        return 0;
    }
    HANDLER_THREAD_ID.store(current_thread_id, Ordering::Release);

    let process = unsafe { GetCurrentProcess() };
    let mut duplicated: HANDLE = ptr::null_mut();
    if unsafe {
        DuplicateHandle(
            process,
            GetCurrentThread(),
            process,
            &mut duplicated,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    } == 0
    {
        handler_failure(FailureKind::DuplicateHandlerThread, unsafe {
            GetLastError()
        });
        return 0;
    }

    if HANDLER_THREAD
        .compare_exchange(
            ptr::null_mut(),
            duplicated,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_err()
    {
        unsafe {
            let _ = CloseHandle(duplicated);
        }
        handler_failure(FailureKind::PublishHandlerThread, 0);
        return 0;
    }

    if unsafe { SetEvent(HANDOFF_EVENT.load(Ordering::Acquire)) } == 0 {
        handler_failure(FailureKind::SignalHandoff, unsafe { GetLastError() });
    }

    // Never consume the event. Runtime handlers later in the newest-first list
    // retain sole ownership of native interrupt policy.
    0
}

fn completion_watcher() {
    loop {
        let handles = [
            FAILURE_EVENT.load(Ordering::Acquire),
            HANDOFF_EVENT.load(Ordering::Acquire),
        ];
        let wait =
            unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, INFINITE) };
        match wait {
            WAIT_OBJECT_0 => return,
            value if value == WAIT_OBJECT_0 + 1 => {}
            WAIT_FAILED => {
                let _ = poison(FailureKind::WaitForHandoff, unsafe { GetLastError() });
                return;
            }
            other => {
                let _ = poison(FailureKind::WaitForHandoff, other);
                return;
            }
        }

        let handler_thread = HANDLER_THREAD.swap(ptr::null_mut(), Ordering::AcqRel);
        if handler_thread.is_null() {
            let _ = poison(FailureKind::MissingHandlerThread, 0);
            return;
        }

        let wait = unsafe { WaitForSingleObject(handler_thread, INFINITE) };
        if wait != WAIT_OBJECT_0 {
            let code = if wait == WAIT_FAILED {
                unsafe { GetLastError() }
            } else {
                wait
            };
            unsafe {
                let _ = CloseHandle(handler_thread);
            }
            let _ = poison(FailureKind::WaitForHandlerThread, code);
            return;
        }
        if unsafe { CloseHandle(handler_thread) } == 0 {
            let _ = poison(FailureKind::CloseHandlerThread, unsafe { GetLastError() });
            return;
        }

        if unsafe { SetEvent(COMPLETION_EVENT.load(Ordering::Acquire)) } == 0 {
            let _ = poison(FailureKind::SignalCompletion, unsafe { GetLastError() });
            return;
        }
        #[cfg(debug_assertions)]
        if signal_test_completion_ready().is_err() {
            return;
        }
    }
}

fn initialized_observer() -> Result<&'static InterruptObserver, ObserverError> {
    match OBSERVER.get() {
        Some(Ok(observer)) => Ok(observer),
        Some(Err(err)) => Err(*err),
        None => Err(ObserverError {
            kind: FailureKind::NotInitialized,
            code: 0,
        }),
    }
}

fn wait_for_cleanup(observer: &InterruptObserver) -> Result<(), ObserverError> {
    let handles = [observer.failure_event, observer.cleanup_event];
    let wait =
        unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, INFINITE) };
    match wait {
        WAIT_OBJECT_0 => Err(current_failure().unwrap_or(ObserverError {
            kind: FailureKind::SignalFailure,
            code: 0,
        })),
        value if value == WAIT_OBJECT_0 + 1 => Ok(()),
        WAIT_FAILED => Err(poison(FailureKind::WaitForCleanup, unsafe {
            GetLastError()
        })),
        other => Err(poison(FailureKind::WaitForCleanup, other)),
    }
}

fn consume_completion() -> Result<(), ObserverError> {
    HANDLER_THREAD_ID.store(0, Ordering::Release);
    if !INTERRUPT_IN_FLIGHT.swap(false, Ordering::AcqRel) {
        return Err(poison(FailureKind::CompletionWithoutInterrupt, 0));
    }
    if !CLEANUP_PENDING.swap(false, Ordering::AcqRel) {
        return Err(poison(FailureKind::CompletionWithoutCleanup, 0));
    }
    Ok(())
}

fn check_failure() -> Result<(), ObserverError> {
    match current_failure() {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

fn current_failure() -> Option<ObserverError> {
    loop {
        let raw = FAILURE_KIND.load(Ordering::Acquire);
        if raw == FailureKind::None as u32 {
            return None;
        }
        if raw == FAILURE_RECORDING {
            std::hint::spin_loop();
            continue;
        }
        return Some(ObserverError {
            kind: FailureKind::from_raw(raw),
            code: FAILURE_CODE.load(Ordering::Acquire),
        });
    }
}

fn poison(kind: FailureKind, code: u32) -> ObserverError {
    record_failure(kind, code);
    let failure_event = FAILURE_EVENT.load(Ordering::Acquire);
    if !failure_event.is_null() && unsafe { SetEvent(failure_event) } == 0 {
        record_failure(FailureKind::SignalFailure, unsafe { GetLastError() });
    }
    current_failure().unwrap_or(ObserverError { kind, code })
}

fn handler_failure(kind: FailureKind, code: u32) {
    record_failure(kind, code);
    let failure_event = FAILURE_EVENT.load(Ordering::Acquire);
    if !failure_event.is_null() && unsafe { SetEvent(failure_event) } == 0 {
        record_failure(FailureKind::SignalFailure, unsafe { GetLastError() });
    }
}

fn record_failure(kind: FailureKind, code: u32) {
    if FAILURE_KIND
        .compare_exchange(
            FailureKind::None as u32,
            FAILURE_RECORDING,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_ok()
    {
        FAILURE_CODE.store(code, Ordering::Relaxed);
        FAILURE_KIND.store(kind as u32, Ordering::Release);
    }
}

fn create_event(manual_reset: bool, kind: FailureKind) -> Result<HANDLE, ObserverError> {
    let handle = unsafe { CreateEventW(ptr::null(), i32::from(manual_reset), 0, ptr::null()) };
    if handle.is_null() {
        Err(sync_error(kind))
    } else {
        Ok(handle)
    }
}

fn sync_error(kind: FailureKind) -> ObserverError {
    ObserverError {
        kind,
        code: unsafe { GetLastError() },
    }
}

fn close_handle(handle: HANDLE) {
    if !handle.is_null() {
        unsafe {
            let _ = CloseHandle(handle);
        }
    }
}
