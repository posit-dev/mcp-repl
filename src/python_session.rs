use std::ffi::{CStr, CString, c_char, c_int, c_long};
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use crate::ipc;
use crate::python_ffi::{GilGuard, ModuleMethod, PyObject, PyPtr, PyThreadState, PythonApi};
use crate::worker_protocol::TextStream;

use state::{
    RawStdinReadError, SESSION_STATE, SessionState, StdinReadAccounting,
    mark_input_wait_completed_request, remember_emitted_prompt, request_active, session_state,
};
use stdio::{PYTHON_STDIN_FILE, PythonThreadsAllowed, StdioLineRead, open_python_runtime};
#[cfg(all(not(target_family = "unix"), not(windows)))]
use stdio::{read_stdio_line_bytes, read_stdio_line_bytes_allowing_python_threads};

mod state;
mod stdio;
#[cfg(target_family = "unix")]
#[allow(dead_code)]
mod unix_stdin;
#[cfg(windows)]
mod windows_stdin;

const MCP_REPL_PYTHON: &str = include_str!("../python/embedded.py");

pub struct PythonSession;

impl PythonSession {
    pub fn start_on_current_thread() -> Result<(), String> {
        let init = Arc::new(SessionInit::new());
        let session = PythonSession;
        if SESSION.set(session).is_err() {
            return Err("Python session already initialized".to_string());
        }
        run_session_on_current_thread(init)
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug)]
enum InitState {
    Pending,
    Ready,
    Failed,
}

#[derive(Debug)]
struct SessionInit {
    state: Mutex<InitState>,
    cvar: Condvar,
}

impl SessionInit {
    fn new() -> Self {
        Self {
            state: Mutex::new(InitState::Pending),
            cvar: Condvar::new(),
        }
    }

    fn mark_ready(&self) {
        let mut guard = self.state.lock().unwrap();
        *guard = InitState::Ready;
        self.cvar.notify_all();
    }

    fn mark_failed(&self, _message: String) {
        let mut guard = self.state.lock().unwrap();
        *guard = InitState::Failed;
        self.cvar.notify_all();
    }
}

fn request_exit() -> Result<(), String> {
    let Some(state) = SESSION_STATE.get() else {
        return Ok(());
    };
    let mut guard = state.inner.lock().unwrap();
    guard.exit_requested = true;
    drop(guard);
    state.cvar.notify_all();
    notify_windows_input("signal Python session exit")
}

fn take_exit_requested() -> bool {
    let Some(state) = SESSION_STATE.get() else {
        return false;
    };
    let mut guard = state.inner.lock().unwrap();
    let requested = guard.exit_requested;
    guard.exit_requested = false;
    requested
}

pub(crate) fn interrupt() {
    interrupt_for_request_generation(None);
}

fn interrupt_for_request_generation(_request_generation: Option<u64>) {
    #[cfg(windows)]
    note_windows_interrupt_cleanup_pending();
    // Sideband interrupt handling owns queued-input cleanup only. On Windows,
    // the supervisor delivers the native Ctrl-C separately through ConPTY.
    discard_pending_stdin();
    #[cfg(target_family = "unix")]
    unix_stdin::flush_terminal_input();
    #[cfg(not(windows))]
    {
        mark_interrupt_requested();
        request_platform_interrupt();
    }
}

#[cfg(windows)]
fn note_windows_interrupt_cleanup_pending() {
    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    let publication_guard = state.publication_guard.lock().unwrap();
    let mut guard = state.inner.lock().unwrap();
    let target_active =
        guard.request_active || guard.cell_running || guard.input_queue.has_active_read_consumer();
    guard.windows_interrupt_checkpoint_pending = true;
    guard.windows_interrupt_target_active =
        Some(guard.windows_interrupt_target_active.unwrap_or(false) || target_active);
    drop(guard);
    drop(publication_guard);
    state.cvar.notify_all();
}

#[cfg(windows)]
fn mark_windows_interrupt_checkpoint_pending() {
    let state = session_state();
    let publication_guard = state.publication_guard.lock().unwrap();
    let mut guard = state.inner.lock().unwrap();
    guard.windows_interrupt_checkpoint_pending = true;
    drop(guard);
    drop(publication_guard);
    state.cvar.notify_all();
}

#[cfg(windows)]
fn windows_interrupt_checkpoint_pending() -> bool {
    session_state()
        .inner
        .lock()
        .unwrap()
        .windows_interrupt_checkpoint_pending
}

#[cfg(windows)]
fn wait_for_windows_interrupt_checkpoint_on_background(
    release_gil_while_waiting: bool,
) -> Result<(), String> {
    let state = session_state();
    if state.on_runtime_main_thread() {
        return Err(
            "runtime main thread cannot defer its Windows interrupt checkpoint".to_string(),
        );
    }
    let mut guard = state.inner.lock().unwrap();
    while guard.windows_interrupt_checkpoint_pending {
        // Extension callbacks still own the GIL; PyOS_Readline callbacks do
        // not. Release it only when the caller owns it while the runtime main
        // thread handles native completion and the Python signal checkpoint.
        let allow_threads = release_gil_while_waiting.then(PythonThreadsAllowed::new);
        let next_guard = state.cvar.wait(guard).unwrap();
        drop(next_guard);
        drop(allow_threads);
        guard = state.inner.lock().unwrap();
    }
    Ok(())
}

#[cfg(not(windows))]
fn mark_interrupt_requested() {
    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    let mut guard = state.inner.lock().unwrap();
    guard.interrupt_requested = true;
    state.cvar.notify_all();
}

#[cfg(not(windows))]
fn request_platform_interrupt() {}

#[cfg(not(windows))]
fn take_interrupt_requested() -> bool {
    let Some(state) = SESSION_STATE.get() else {
        return false;
    };
    let mut guard = state.inner.lock().unwrap();
    let requested = guard.interrupt_requested;
    guard.interrupt_requested = false;
    requested
}

pub(crate) fn begin_input(input: String) -> Result<(), String> {
    if input.is_empty() {
        return Ok(());
    }
    let state = session_state();
    let should_record_background_plots = {
        let guard = state.inner.lock().unwrap();
        !guard.request_active
    };
    if should_record_background_plots {
        #[cfg(not(windows))]
        {
            clear_python_pending_interrupt();
        }
        record_background_plots();
    }
    {
        let mut guard = state.inner.lock().unwrap();
        if guard.shutdown {
            return Err("Python session is shutting down".to_string());
        }
        guard.input_queue.push_payload(input);
        guard.request_active = true;
        guard.plot_reset_pending = true;
        #[cfg(not(windows))]
        {
            guard.interrupt_requested = false;
        }
    }
    state.cvar.notify_all();
    notify_windows_input("signal accepted Python input")
}

pub(crate) fn request_shutdown() -> Result<(), String> {
    let Some(state) = SESSION_STATE.get() else {
        return Ok(());
    };
    let mut guard = state.inner.lock().unwrap();
    // Preserve already accepted input; reset replies include output produced
    // while the old worker drains to a safe runtime boundary.
    guard.shutdown = true;
    #[cfg(not(windows))]
    {
        guard.interrupt_requested = false;
    }
    drop(guard);
    state.cvar.notify_all();
    notify_windows_input("signal Python session shutdown")
}

fn notify_windows_input(_context: &str) -> Result<(), String> {
    #[cfg(windows)]
    crate::windows_interrupt_observer::notify_input()
        .map_err(|err| format!("{_context}: {err}"))?;
    Ok(())
}

#[cfg(target_family = "unix")]
fn discard_pending_stdin() {
    discard_queued_input();
}

pub(crate) fn record_protocol_failure(message: &str) {
    record_protocol_failure_state(message, true);
}

#[cfg(windows)]
fn record_startup_protocol_failure(message: &str) {
    record_protocol_failure_state(message, false);
}

fn record_protocol_failure_state(message: &str, emit_diagnostic: bool) {
    let Some(state) = SESSION_STATE.get() else {
        if emit_diagnostic {
            emit_protocol_failure_diagnostic(message);
        }
        return;
    };

    let mut guard = state.inner.lock().unwrap();
    if guard.session_end_emitted {
        return;
    }
    if guard.protocol_failure.is_none() {
        guard.protocol_failure = Some(message.to_string());
        // Keep the state lock until any diagnostic is sent so finalization
        // cannot publish the terminal session_end first.
        if emit_diagnostic {
            emit_protocol_failure_diagnostic(message);
        }
    }
    guard.exit_requested = true;
    guard.shutdown = true;
    guard.request_active = false;
    guard.cell_running = false;
    guard.visible_input_prompt = None;
    guard.input_queue.clear_after_interrupt();
    #[cfg(not(windows))]
    {
        guard.interrupt_requested = false;
    }
    drop(guard);

    state.cvar.notify_all();
    #[cfg(windows)]
    {
        // Observer failures signal their own failure event. This best-effort
        // wake also handles protocol failures discovered by the IPC reader.
        let _ = crate::windows_interrupt_observer::notify_input();
    }
}

pub(crate) fn protocol_failure_recorded() -> bool {
    SESSION_STATE
        .get()
        .is_some_and(|state| state.inner.lock().unwrap().protocol_failure.is_some())
}

fn emit_protocol_failure(message: &str) {
    record_protocol_failure(message);
}

fn emit_protocol_failure_diagnostic(message: &str) {
    let mut bytes = message.as_bytes().to_vec();
    if !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    if ipc::emit_output_text(TextStream::Stderr, &bytes).is_err() {
        crate::output_stream::write_stderr_bytes(&bytes);
    }
}

#[cfg(windows)]
fn discard_pending_stdin() {
    if let Some(state) = SESSION_STATE.get() {
        let mut guard = state.inner.lock().unwrap();
        // The live reader owns this bit. Interrupt cleanup discards only data
        // which the runtime has not consumed; it must not revoke ownership.
        guard.input_queue.discard_unconsumed_input();
        state.cvar.notify_all();
    }
    windows_stdin::discard_pending_stdin();
}

#[cfg(not(any(target_family = "unix", windows)))]
fn discard_pending_stdin() {
    discard_queued_input();
}

#[cfg(not(windows))]
fn discard_queued_input() {
    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    let mut guard = state.inner.lock().unwrap();
    guard.input_queue.clear_after_interrupt();
    state.cvar.notify_all();
}

fn run_session_on_current_thread(init: Arc<SessionInit>) -> Result<(), String> {
    crate::diagnostics::startup_log("python-session: init begin");
    let state = Arc::new(SessionState::new());
    if SESSION_STATE.set(state.clone()).is_err() {
        let message = "Python session state already initialized".to_string();
        init.mark_failed(message.clone());
        return Err(message);
    }
    #[cfg(windows)]
    if let Err(err) = crate::windows_interrupt_observer::initialize_after_console_attach() {
        let message = format!(
            "failed to initialize the Windows interrupt observer after console attachment: {err}"
        );
        record_startup_protocol_failure(&message);
        finish_session_end();
        init.mark_failed(message.clone());
        return Err(message);
    }

    let runtime_config = match crate::python_runtime::resolve_python_runtime_config() {
        Ok(runtime_config) => runtime_config,
        Err(err) => {
            init.mark_failed(err.clone());
            return Err(err);
        }
    };
    let api = match PythonApi::initialize(&runtime_config.libpython) {
        Ok(api) => api,
        Err(err) => {
            init.mark_failed(err.clone());
            return Err(err);
        }
    };
    let thread_state = match initialize_python(api, &runtime_config.executable) {
        Ok(thread_state) => thread_state,
        Err(err) => {
            init.mark_failed(err.clone());
            return Err(err);
        }
    };
    if thread_state.is_null() {
        let err = "failed to release initialized Python thread state".to_string();
        init.mark_failed(err.clone());
        return Err(err);
    }
    if let Err(err) = open_python_runtime() {
        init.mark_failed(err.clone());
        return Err(err);
    }

    if let Err(err) = configure_python(api) {
        let _gil = GilGuard::acquire();
        api.print_error();
        init.mark_failed(err.clone());
        return Err(err);
    }

    init.mark_ready();
    ipc::emit_worker_ready("python", plot_capable());

    let result = run_cell_loop();
    if let Err(err) = &result {
        record_protocol_failure(err);
    }
    // Py_FinalizeEx follows CPython shutdown semantics, including waiting for
    // user-created non-daemon threads. While that wait is in progress the
    // worker is still alive, so the server must not synthesize session_end.
    crate::diagnostics::startup_log("python-session: cell loop exited; finalizing python");
    let finalize_result = finalize_python(api, thread_state);
    match &finalize_result {
        Ok(()) => crate::diagnostics::startup_log("python-session: python finalized"),
        Err(err) => {
            crate::diagnostics::startup_log(format!("python-session: finalize failed: {err}"))
        }
    }
    crate::diagnostics::startup_log("python-session: emitting session_end");
    finish_session_end();
    crate::diagnostics::startup_log("python-session: emitted session_end");
    result?;
    finalize_result?;
    Ok(())
}

fn initialize_python(
    api: &'static PythonApi,
    executable: &Path,
) -> Result<*mut PyThreadState, String> {
    let module_name = CString::new("_mcp_repl").expect("module name must not contain NUL");
    let module_name = module_name.into_raw();
    let rc = unsafe { (api.py_import_append_inittab)(module_name, initialize_mcp_repl_module) };
    if rc != 0 {
        return Err("failed to register _mcp_repl embedded Python module".to_string());
    }

    unsafe {
        if (api.py_is_initialized)() != 0 {
            return Err("embedded Python interpreter was already initialized".to_string());
        }
        api.set_program_name(executable)?;
        api.set_interactive_flags()?;
        (api.py_initialize_ex)(1);
        api.install_readline_function(mcp_repl_readline)?;
        let thread_state = (api.py_eval_save_thread)();
        api.install_input_hook(pyos_input_hook)?;
        Ok(thread_state)
    }
}

fn configure_python(api: &'static PythonApi) -> Result<(), String> {
    let _gil = GilGuard::acquire();
    let builtins = api.import_module("builtins")?;
    let runtime_error = api.get_attr_string(builtins.as_ptr(), "RuntimeError")?;
    RUNTIME_ERROR.store(runtime_error.as_ptr(), Ordering::SeqCst);
    let _runtime_error = runtime_error.into_raw();

    let main = api.import_module("__main__")?;
    let globals = unsafe { (api.py_module_get_dict)(main.as_ptr()) };
    if globals.is_null() {
        return Err("failed to get __main__ globals".to_string());
    }
    api.run_code(MCP_REPL_PYTHON, globals)?;
    Ok(())
}

fn run_cell_loop() -> Result<(), String> {
    let api = PythonApi::global();
    emit_ready()?;
    loop {
        let Some(cell) = wait_for_next_cell()? else {
            flush_original_stdio();
            return Ok(());
        };
        {
            let _gil = GilGuard::acquire();
            clear_python_stdin_buffers(api)?;
            run_python_cell(api, &cell.source);
            capture_python_prompts(api)?;
            flush_original_stdio();
        }
        if take_exit_requested() {
            mark_cell_running(false);
            flush_original_stdio();
            return Ok(());
        }
        emit_plots();
        finish_cell_request()?;
    }
}

struct CellInput {
    source: String,
}

fn emit_ready() -> Result<(), String> {
    let api = PythonApi::global();
    {
        let _gil = GilGuard::acquire();
        capture_python_prompts(api)?;
    }
    #[cfg(not(windows))]
    {
        let state = session_state();
        let mut guard = state.inner.lock().unwrap();
        guard.request_active = false;
        guard.cell_running = false;
        guard.visible_input_prompt = None;
    }
    emit_ready_handling_python_error()?;
    Ok(())
}

fn mark_cell_running(running: bool) {
    let state = session_state();
    let mut guard = state.inner.lock().unwrap();
    guard.cell_running = running;
}

#[cfg(not(windows))]
fn wait_for_next_cell() -> Result<Option<CellInput>, String> {
    let state = session_state();
    let mut guard = state.inner.lock().unwrap();
    loop {
        if guard.exit_requested {
            return Ok(None);
        }
        if guard.interrupt_requested {
            guard.interrupt_requested = false;
            guard.request_active = false;
            guard.cell_running = false;
            guard.visible_input_prompt = None;
            state.cvar.notify_all();
            drop(guard);
            clear_python_pending_interrupt();
            ipc::emit_ready();
            guard = state.inner.lock().unwrap();
            continue;
        }
        if !guard.input_queue.has_active_read_consumer()
            && let Some(source) = guard.input_queue.take_cell_payload()
        {
            guard.cell_running = true;
            return Ok(Some(CellInput { source }));
        }
        if guard.shutdown {
            return Ok(None);
        }
        guard = state.cvar.wait(guard).unwrap();
    }
}

#[cfg(windows)]
fn wait_for_next_cell() -> Result<Option<CellInput>, String> {
    use crate::windows_interrupt_observer::WaitOutcome;

    let state = session_state();
    let mut guard = state.inner.lock().unwrap();
    loop {
        // A native handler may have started while Python was executing or
        // between readiness and accepted input. Complete its dispatch and run
        // the Python checkpoint before any queued cell can be consumed.
        drop(guard);
        if let Some(signal_pending) = finish_and_check_windows_interrupt(false)? {
            if signal_pending {
                let _gil = GilGuard::acquire();
                PythonApi::global().print_error();
            }
            emit_ready()?;
        }
        guard = state.inner.lock().unwrap();

        if guard.exit_requested {
            return Ok(None);
        }
        if !guard.input_queue.has_active_read_consumer()
            && let Some(source) = guard.input_queue.take_cell_payload()
        {
            guard.cell_running = true;
            return Ok(Some(CellInput { source }));
        }
        if guard.shutdown {
            return Ok(None);
        }

        drop(guard);
        let outcome = crate::windows_interrupt_observer::wait_for_activity()
            .map_err(|err| format!("failed while waiting for Python cell activity: {err}"))?;
        let completion = match outcome {
            WaitOutcome::InterruptCompleted => Some(check_joined_windows_interrupt()?),
            WaitOutcome::Input => finish_and_check_windows_interrupt(false)?,
        };
        if let Some(signal_pending) = completion {
            if signal_pending {
                let _gil = GilGuard::acquire();
                PythonApi::global().print_error();
            }
            // Publish fresh readiness only after the native handler chain and
            // Python checkpoint have completed.
            emit_ready()?;
        }
        guard = state.inner.lock().unwrap();
    }
}

fn emit_ready_handling_python_error() -> Result<(), String> {
    loop {
        if !try_emit_ready_sideband(false)? {
            return Ok(());
        }
        let _gil = GilGuard::acquire();
        PythonApi::global().print_error();
    }
}

/// Returns `true` with the native Python exception left pending when the
/// observer completed an in-flight handler dispatch. In that case no readiness
/// is published and the caller must either propagate or print the exception.
fn try_emit_ready_sideband(release_gil_while_waiting: bool) -> Result<bool, String> {
    #[cfg(windows)]
    {
        publish_windows_sideband_readiness(
            release_gil_while_waiting,
            "ready",
            |guard| {
                if guard.input_queue.has_active_read_consumer() {
                    return false;
                }
                guard.request_active = false;
                guard.cell_running = false;
                guard.visible_input_prompt = None;
                true
            },
            ipc::emit_ready_checked,
        )
    }

    #[cfg(not(windows))]
    {
        let _ = release_gil_while_waiting;
        ipc::emit_ready();
        Ok(false)
    }
}

/// Returns `true` with the native Python exception left pending instead of
/// publishing `input_wait`.
fn emit_input_wait_sideband(prompt: &str, release_gil_while_waiting: bool) -> Result<bool, String> {
    #[cfg(windows)]
    {
        publish_windows_sideband_readiness(
            release_gil_while_waiting,
            "input_wait",
            |_| true,
            || ipc::emit_input_wait_checked(prompt),
        )
    }

    #[cfg(not(windows))]
    {
        let _ = release_gil_while_waiting;
        ipc::emit_input_wait(prompt);
        Ok(false)
    }
}

#[cfg(windows)]
fn publish_windows_sideband_readiness(
    release_gil_while_waiting: bool,
    kind: &str,
    commit: impl Fn(&mut state::SessionStateInner) -> bool,
    publish: impl Fn() -> std::io::Result<()>,
) -> Result<bool, String> {
    let state = session_state();
    loop {
        if prepare_windows_sideband_publication(release_gil_while_waiting)? {
            return Ok(true);
        }

        #[cfg(debug_assertions)]
        let background_publication = !state.on_runtime_main_thread();
        #[cfg(debug_assertions)]
        if background_publication {
            crate::windows_interrupt_observer::wait_at_test_sideband_publication_boundary()
                .map_err(|err| format!("failed at test sideband publication boundary: {err}"))?;
        }

        let publication_guard = state.publication_guard.lock().unwrap();
        let mut guard = state.inner.lock().unwrap();
        let checkpoint_pending = guard.windows_interrupt_checkpoint_pending;
        if checkpoint_pending {
            drop(guard);
            drop(publication_guard);
            #[cfg(debug_assertions)]
            if background_publication {
                crate::windows_interrupt_observer::signal_test_sideband_publication_deferred()
                    .map_err(|err| {
                        format!("failed to signal deferred test sideband publication: {err}")
                    })?;
            }
            continue;
        }

        if !commit(&mut guard) {
            drop(guard);
            drop(publication_guard);
            return Ok(false);
        }

        // Keep consumer ownership stable until the readiness fact is written.
        publish().map_err(|err| format!("failed to publish Windows {kind} readiness: {err}"))?;
        #[cfg(debug_assertions)]
        if background_publication {
            crate::windows_interrupt_observer::signal_test_sideband_publication_sent()
                .map_err(|err| format!("failed to signal test sideband publication: {err}"))?;
        }
        drop(guard);
        drop(publication_guard);
        return Ok(false);
    }
}

#[cfg(windows)]
fn with_python_threads_allowed_if<T>(
    release_gil_while_waiting: bool,
    operation: impl FnOnce() -> T,
) -> T {
    let allow_threads = release_gil_while_waiting.then(PythonThreadsAllowed::new);
    let result = operation();
    drop(allow_threads);
    result
}

#[cfg(windows)]
fn prepare_windows_sideband_publication(release_gil_while_waiting: bool) -> Result<bool, String> {
    use crate::windows_interrupt_observer::{RearmOutcome, WaitOutcome};

    let state = session_state();
    loop {
        if state.on_runtime_main_thread() {
            if let Some(signal_pending) =
                finish_and_check_windows_interrupt(release_gil_while_waiting)?
            {
                return Ok(signal_pending);
            }
            if windows_interrupt_checkpoint_pending() {
                let outcome = with_python_threads_allowed_if(
                    release_gil_while_waiting,
                    crate::windows_interrupt_observer::wait_for_activity,
                )
                .map_err(|err| {
                    format!("failed while waiting for Windows interrupt completion: {err}")
                })?;
                match outcome {
                    WaitOutcome::InterruptCompleted => {
                        return check_joined_windows_interrupt();
                    }
                    WaitOutcome::Input => continue,
                }
            }
            match with_python_threads_allowed_if(
                release_gil_while_waiting,
                crate::windows_interrupt_observer::rearm,
            )
            .map_err(|err| format!("failed to arm the Windows interrupt observer: {err}"))?
            {
                RearmOutcome::Rearmed => {
                    // Cleanup can arrive while this caller was blocked behind
                    // another rearm. Do not publish stale readiness ahead of
                    // the native dispatch prepared by that cleanup.
                    if !windows_interrupt_checkpoint_pending() {
                        return Ok(false);
                    }
                }
                RearmOutcome::InterruptInFlight => {
                    mark_windows_interrupt_checkpoint_pending();
                    if let Some(signal_pending) =
                        finish_and_check_windows_interrupt(release_gil_while_waiting)?
                    {
                        return Ok(signal_pending);
                    }
                }
            }
        } else {
            if windows_interrupt_checkpoint_pending() {
                wait_for_windows_interrupt_checkpoint_on_background(release_gil_while_waiting)?;
                continue;
            }
            // A package's native handler may be a Python ctypes callback. If
            // this callback owns the GIL, release it while rearming so a
            // concurrent handler can acquire it and finish.
            let rearm = with_python_threads_allowed_if(
                release_gil_while_waiting,
                crate::windows_interrupt_observer::rearm,
            );
            match rearm
                .map_err(|err| format!("failed to arm the Windows interrupt observer: {err}"))?
            {
                RearmOutcome::Rearmed => {
                    if windows_interrupt_checkpoint_pending() {
                        wait_for_windows_interrupt_checkpoint_on_background(
                            release_gil_while_waiting,
                        )?;
                    } else {
                        return Ok(false);
                    }
                }
                RearmOutcome::InterruptInFlight => {
                    mark_windows_interrupt_checkpoint_pending();
                    wait_for_windows_interrupt_checkpoint_on_background(release_gil_while_waiting)?;
                }
            }
        }
    }
}

#[cfg(windows)]
fn finish_and_check_windows_interrupt(
    release_gil_while_waiting: bool,
) -> Result<Option<bool>, String> {
    if !session_state().on_runtime_main_thread() {
        return Err(
            "Windows interrupt completion must run on the Python runtime main thread".to_string(),
        );
    }
    let completed = with_python_threads_allowed_if(
        release_gil_while_waiting,
        crate::windows_interrupt_observer::finish_in_flight_interrupt,
    )
    .map_err(|err| format!("failed to finish Windows interrupt dispatch: {err}"))?;
    if !completed {
        return Ok(None);
    }
    Ok(Some(check_joined_windows_interrupt()?))
}

#[cfg(windows)]
fn check_joined_windows_interrupt() -> Result<bool, String> {
    let state = session_state();
    if !state.on_runtime_main_thread() {
        return Err(
            "Windows interrupt checkpoint must run on the Python runtime main thread".to_string(),
        );
    }
    let target_active = state
        .inner
        .lock()
        .unwrap()
        .windows_interrupt_target_active
        .ok_or_else(|| {
            "Windows interrupt completed before cleanup reached the Python runtime".to_string()
        })?;
    #[cfg(debug_assertions)]
    crate::windows_interrupt_observer::wait_at_test_python_checkpoint_boundary()
        .map_err(|err| format!("failed at test Python checkpoint boundary: {err}"))?;
    let signal_pending = check_windows_signals(target_active);
    // Python can acknowledge checkpoint completion after PyErr_CheckSignals
    // returns; unlike R, no non-local jump can skip this marker.
    let publication_guard = state.publication_guard.lock().unwrap();
    crate::ipc::emit_interrupt_complete()
        .map_err(|err| format!("failed to report joined Windows interrupt: {err}"))?;
    {
        let mut guard = state.inner.lock().unwrap();
        guard.windows_interrupt_checkpoint_pending = false;
        guard.windows_interrupt_target_active = None;
    }
    drop(publication_guard);
    state.cvar.notify_all();
    Ok(signal_pending)
}

#[cfg(windows)]
fn check_windows_signals(target_active: bool) -> bool {
    let api = PythonApi::global();
    let _gil = GilGuard::acquire();
    if api.check_signals().is_ok() {
        return false;
    }
    if target_active {
        true
    } else {
        // An idle control-prefix interrupt is an ordering barrier, not an
        // active Python operation. Consume its native pending exception here
        // so the admitted tail cell cannot inherit KeyboardInterrupt.
        api.clear_error();
        false
    }
}

fn run_python_cell(api: &'static PythonApi, source: &str) {
    let main = match api.import_module("__main__") {
        Ok(main) => main,
        Err(_) => {
            api.print_error();
            return;
        }
    };
    let func = match api.get_attr_string(main.as_ptr(), "_mcp_repl_run_cell") {
        Ok(func) => func,
        Err(_) => {
            api.print_error();
            return;
        }
    };
    match api.call_one_string_arg(func.as_ptr(), source) {
        Ok(result) => drop(result),
        Err(_) => api.print_error(),
    }
}

fn finish_cell_request() -> Result<(), String> {
    let api = PythonApi::global();
    {
        let _gil = GilGuard::acquire();
        clear_python_stdin_buffers(api)?;
    }
    let state = session_state();
    let emit_ready = {
        let mut guard = state.inner.lock().unwrap();
        guard.cell_running = false;
        guard.input_queue.clear_after_cell_finish();
        if !guard.input_queue.has_active_read_consumer() {
            guard.request_active = false;
            guard.visible_input_prompt = None;
            true
        } else {
            false
        }
    };
    if emit_ready {
        emit_ready_handling_python_error()?;
    }
    Ok(())
}

fn capture_python_prompts(api: &'static PythonApi) -> Result<(), String> {
    let main = api.import_module("__main__")?;
    let func = api.get_attr_string(main.as_ptr(), "_mcp_repl_capture_prompts")?;
    let result = unsafe { (api.py_object_call_object)(func.as_ptr(), ptr::null_mut()) };
    let result = PyPtr::from_owned(result, "Python prompt capture failed")?;
    drop(result);
    Ok(())
}

fn clear_python_stdin_buffers(api: &'static PythonApi) -> Result<(), String> {
    let main = api.import_module("__main__")?;
    let func = api.get_attr_string(main.as_ptr(), "_mcp_repl_clear_stdin_buffers")?;
    let result = unsafe { (api.py_object_call_object)(func.as_ptr(), ptr::null_mut()) };
    let result = PyPtr::from_owned(result, "Python stdin buffer cleanup failed")?;
    drop(result);
    Ok(())
}

#[cfg(not(windows))]
fn clear_python_pending_interrupt() {
    let api = PythonApi::global();
    let _gil = GilGuard::acquire();
    api.clear_pending_signals();
}

fn finalize_python(
    api: &'static PythonApi,
    thread_state: *mut PyThreadState,
) -> Result<(), String> {
    unsafe {
        (api.py_eval_restore_thread)(thread_state);
        match (api.py_finalize_ex)() {
            0 => Ok(()),
            _ => Err("CPython finalization failed".to_string()),
        }
    }
}

fn set_python_prompts(primary: String, continuation: String) {
    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    let mut guard = state.inner.lock().unwrap();
    guard.python_primary_prompt = primary;
    guard.python_continuation_prompt = continuation;
}

fn handle_input_hook() {
    let Some(state) = SESSION_STATE.get() else {
        return;
    };
    state.cvar.notify_all();
}

unsafe extern "C" fn pyos_input_hook() -> c_int {
    handle_input_hook();
    0
}

enum QueueReadAction {
    Line {
        bytes: Vec<u8>,
        prompt_already_visible: bool,
        detached_request: bool,
        emit_input_line: bool,
    },
    InputWait {
        prompt: String,
    },
    Interrupted,
    Shutdown,
}

fn wait_for_queue_notification<'a>(
    state: &'a Arc<SessionState>,
    guard: std::sync::MutexGuard<'a, state::SessionStateInner>,
    release_gil_while_waiting: bool,
) -> Result<
    (
        std::sync::MutexGuard<'a, state::SessionStateInner>,
        Option<bool>,
    ),
    String,
> {
    #[cfg(windows)]
    {
        use crate::windows_interrupt_observer::WaitOutcome;

        if state.on_runtime_main_thread() {
            let allow_threads = release_gil_while_waiting.then(PythonThreadsAllowed::new);
            drop(guard);
            let outcome = crate::windows_interrupt_observer::wait_for_activity()
                .map_err(|err| format!("failed while waiting for Python input activity: {err}"))?;
            drop(allow_threads);
            let completion = match outcome {
                WaitOutcome::InterruptCompleted => Some(check_joined_windows_interrupt()?),
                WaitOutcome::Input => {
                    finish_and_check_windows_interrupt(release_gil_while_waiting)?
                }
            };
            Ok((state.inner.lock().unwrap(), completion))
        } else {
            // The observer completion event is single-consumer state owned by
            // the runtime main thread. A background managed-stdin callback
            // waits only for durable queue/session predicates.
            // PyOS_Readline has already released the GIL; extension callbacks
            // still own it and release it here.
            let allow_threads = release_gil_while_waiting.then(PythonThreadsAllowed::new);
            let guard = state.cvar.wait(guard).unwrap();
            drop(guard);
            drop(allow_threads);
            Ok((state.inner.lock().unwrap(), None))
        }
    }

    #[cfg(not(windows))]
    {
        let guard = if release_gil_while_waiting {
            let allow_threads = PythonThreadsAllowed::new();
            let guard = state.cvar.wait(guard).unwrap();
            drop(guard);
            drop(allow_threads);
            state.inner.lock().unwrap()
        } else {
            state.cvar.wait(guard).unwrap()
        };
        Ok((guard, None))
    }
}

fn release_read_consumer(state: &Arc<SessionState>) {
    let mut guard = state.inner.lock().unwrap();
    guard.input_queue.end_read_consumer();
    state.cvar.notify_all();
}

fn next_queue_line_action(
    state: &Arc<SessionState>,
    prompt: &str,
    prompt_wait_emitted: &mut bool,
    owns_consumer: &mut bool,
    release_gil_while_waiting: bool,
) -> Result<QueueReadAction, String> {
    #[cfg(windows)]
    let on_runtime_main_thread = state.on_runtime_main_thread();
    #[cfg(windows)]
    let completed_interrupt = if on_runtime_main_thread {
        finish_and_check_windows_interrupt(release_gil_while_waiting)?
    } else {
        None
    };
    let mut guard = state.inner.lock().unwrap();
    #[cfg(windows)]
    if let Some(signal_pending) = completed_interrupt {
        if signal_pending {
            if *owns_consumer {
                guard.input_queue.end_read_consumer();
                *owns_consumer = false;
                state.cvar.notify_all();
            }
            return Ok(QueueReadAction::Interrupted);
        }
        // Re-publish the managed wait after the checkpoint before considering
        // input which may already be queued.
        guard.visible_input_prompt = (!prompt.is_empty()).then(|| prompt.to_string());
        *prompt_wait_emitted = true;
        return Ok(QueueReadAction::InputWait {
            prompt: prompt.to_string(),
        });
    }
    loop {
        #[cfg(windows)]
        if !on_runtime_main_thread && guard.windows_interrupt_checkpoint_pending {
            let (next_guard, _) =
                wait_for_queue_notification(state, guard, release_gil_while_waiting)?;
            guard = next_guard;
            continue;
        }
        if guard.exit_requested {
            if *owns_consumer {
                guard.input_queue.end_read_consumer();
                *owns_consumer = false;
            }
            state.cvar.notify_all();
            return Ok(QueueReadAction::Shutdown);
        }
        #[cfg(not(windows))]
        if guard.interrupt_requested {
            guard.interrupt_requested = false;
            if *owns_consumer {
                guard.input_queue.end_read_consumer();
                *owns_consumer = false;
            }
            state.cvar.notify_all();
            return Ok(QueueReadAction::Interrupted);
        }
        if !*owns_consumer {
            if guard.input_queue.begin_read_consumer() {
                *owns_consumer = true;
            } else {
                let (next_guard, completion) =
                    wait_for_queue_notification(state, guard, release_gil_while_waiting)?;
                guard = next_guard;
                if let Some(signal_pending) = completion {
                    if signal_pending {
                        if *owns_consumer {
                            guard.input_queue.end_read_consumer();
                            *owns_consumer = false;
                        }
                        state.cvar.notify_all();
                        return Ok(QueueReadAction::Interrupted);
                    }
                    *prompt_wait_emitted = true;
                    guard.visible_input_prompt = (!prompt.is_empty()).then(|| prompt.to_string());
                    return Ok(QueueReadAction::InputWait {
                        prompt: prompt.to_string(),
                    });
                }
                continue;
            }
        }
        if let Some(read) = guard.input_queue.consume_line() {
            let emit_input_line = guard.request_active;
            let prompt_already_visible = guard.visible_input_prompt.as_deref() == Some(prompt);
            let detached_request = !guard.cell_running;
            guard.visible_input_prompt = None;
            guard.request_active = true;
            if *owns_consumer {
                guard.input_queue.end_read_consumer();
                *owns_consumer = false;
                state.cvar.notify_all();
            }
            return Ok(QueueReadAction::Line {
                bytes: read.protocol_bytes,
                prompt_already_visible,
                detached_request,
                emit_input_line,
            });
        }
        if guard.shutdown {
            if *owns_consumer {
                guard.input_queue.end_read_consumer();
                *owns_consumer = false;
            }
            state.cvar.notify_all();
            return Ok(QueueReadAction::Shutdown);
        }
        if !*prompt_wait_emitted {
            *prompt_wait_emitted = true;
            guard.visible_input_prompt = (!prompt.is_empty()).then(|| prompt.to_string());
            return Ok(QueueReadAction::InputWait {
                prompt: prompt.to_string(),
            });
        }
        let (next_guard, completion) =
            wait_for_queue_notification(state, guard, release_gil_while_waiting)?;
        guard = next_guard;
        if let Some(signal_pending) = completion {
            if signal_pending {
                if *owns_consumer {
                    guard.input_queue.end_read_consumer();
                    *owns_consumer = false;
                }
                state.cvar.notify_all();
                return Ok(QueueReadAction::Interrupted);
            }
            // A handler consumed Ctrl-C without scheduling a Python signal.
            // Publish a fresh wait only after PyErr_CheckSignals found no
            // pending exception, then continue the same input operation.
            guard.visible_input_prompt = (!prompt.is_empty()).then(|| prompt.to_string());
            return Ok(QueueReadAction::InputWait {
                prompt: prompt.to_string(),
            });
        }
    }
}

fn read_queue_line(
    prompt: &str,
    emit_prompt_to_stdout: bool,
    release_gil_while_waiting: bool,
) -> Result<StdioLineRead, String> {
    let state = SESSION_STATE
        .get()
        .ok_or_else(|| "Python session state is not initialized".to_string())?;
    let mut prompt_wait_emitted = false;
    let mut owns_consumer = false;
    loop {
        match next_queue_line_action(
            state,
            prompt,
            &mut prompt_wait_emitted,
            &mut owns_consumer,
            release_gil_while_waiting,
        )? {
            QueueReadAction::Line {
                bytes,
                prompt_already_visible,
                detached_request,
                emit_input_line,
            } => {
                if emit_input_line {
                    ipc::emit_input_line(prompt, &String::from_utf8_lossy(&bytes));
                }
                if emit_prompt_to_stdout && !prompt.is_empty() && !prompt_already_visible {
                    emit_output_text(TextStream::Stdout, prompt.as_bytes());
                }
                if detached_request && complete_detached_read_request(release_gil_while_waiting)? {
                    return Ok(StdioLineRead {
                        bytes: Vec::new(),
                        interrupted: true,
                    });
                }
                return Ok(StdioLineRead {
                    bytes,
                    interrupted: false,
                });
            }
            QueueReadAction::InputWait { prompt } => {
                emit_plots();
                mark_input_wait_completed_request();
                remember_emitted_prompt(&prompt);
                if emit_input_wait_sideband(&prompt, release_gil_while_waiting)? {
                    if owns_consumer {
                        release_read_consumer(state);
                    }
                    return Ok(StdioLineRead {
                        bytes: Vec::new(),
                        interrupted: true,
                    });
                }
            }
            QueueReadAction::Interrupted => {
                if owns_consumer {
                    release_read_consumer(state);
                }
                return Ok(StdioLineRead {
                    bytes: Vec::new(),
                    interrupted: true,
                });
            }
            QueueReadAction::Shutdown => {
                return Ok(StdioLineRead {
                    bytes: Vec::new(),
                    interrupted: false,
                });
            }
        }
    }
}

fn read_queue_raw_bytes(size: usize) -> Result<Vec<u8>, RawStdinReadError> {
    if size == 0 {
        return Ok(Vec::new());
    }
    let state = SESSION_STATE.get().ok_or_else(|| {
        RawStdinReadError::Runtime("Python session state is not initialized".to_string())
    })?;
    let mut output = Vec::new();
    let mut prompt_wait_emitted = false;
    let mut owns_consumer = false;
    #[cfg(windows)]
    let on_runtime_main_thread = state.on_runtime_main_thread();
    while output.len() < size {
        let action = 'action: {
            #[cfg(windows)]
            let completed_interrupt = if on_runtime_main_thread {
                finish_and_check_windows_interrupt(true).map_err(RawStdinReadError::Runtime)?
            } else {
                None
            };
            #[cfg(not(windows))]
            let completed_interrupt: Option<bool> = None;
            let mut guard = state.inner.lock().unwrap();
            if let Some(signal_pending) = completed_interrupt {
                if signal_pending {
                    if owns_consumer {
                        guard.input_queue.end_read_consumer();
                        state.cvar.notify_all();
                    }
                    return Err(RawStdinReadError::Interrupted);
                }
                prompt_wait_emitted = true;
                guard.visible_input_prompt = None;
                break 'action QueueReadAction::InputWait {
                    prompt: String::new(),
                };
            }
            loop {
                #[cfg(windows)]
                if !on_runtime_main_thread && guard.windows_interrupt_checkpoint_pending {
                    let (next_guard, _) = wait_for_queue_notification(state, guard, true)
                        .map_err(RawStdinReadError::Runtime)?;
                    guard = next_guard;
                    continue;
                }
                if guard.exit_requested {
                    if owns_consumer {
                        guard.input_queue.end_read_consumer();
                        state.cvar.notify_all();
                    }
                    return Ok(output);
                }
                #[cfg(not(windows))]
                if guard.interrupt_requested {
                    guard.interrupt_requested = false;
                    if owns_consumer {
                        guard.input_queue.end_read_consumer();
                        state.cvar.notify_all();
                    }
                    return Err(RawStdinReadError::Interrupted);
                }
                if !output.is_empty() {
                    return Ok(output);
                }
                if !owns_consumer {
                    if guard.input_queue.begin_read_consumer() {
                        owns_consumer = true;
                    } else {
                        let (next_guard, completion) =
                            wait_for_queue_notification(state, guard, true)
                                .map_err(RawStdinReadError::Runtime)?;
                        guard = next_guard;
                        if let Some(signal_pending) = completion {
                            if signal_pending {
                                if owns_consumer {
                                    guard.input_queue.end_read_consumer();
                                    state.cvar.notify_all();
                                }
                                return Err(RawStdinReadError::Interrupted);
                            }
                            prompt_wait_emitted = true;
                            guard.visible_input_prompt = None;
                            break QueueReadAction::InputWait {
                                prompt: String::new(),
                            };
                        }
                        continue;
                    }
                }
                let remaining = size - output.len();
                if let Some(read) = guard.input_queue.consume_bytes(remaining) {
                    let emit_input_line = guard.request_active;
                    guard.visible_input_prompt = None;
                    guard.request_active = true;
                    if owns_consumer {
                        guard.input_queue.end_read_consumer();
                        owns_consumer = false;
                        state.cvar.notify_all();
                    }
                    break QueueReadAction::Line {
                        bytes: read.protocol_bytes,
                        prompt_already_visible: true,
                        detached_request: !guard.cell_running,
                        emit_input_line,
                    };
                }
                if guard.shutdown {
                    if owns_consumer {
                        guard.input_queue.end_read_consumer();
                        state.cvar.notify_all();
                    }
                    return Ok(output);
                }
                if !prompt_wait_emitted {
                    prompt_wait_emitted = true;
                    guard.visible_input_prompt = None;
                    break QueueReadAction::InputWait {
                        prompt: String::new(),
                    };
                }
                let (next_guard, completion) = wait_for_queue_notification(state, guard, true)
                    .map_err(RawStdinReadError::Runtime)?;
                guard = next_guard;
                if let Some(signal_pending) = completion {
                    if signal_pending {
                        if owns_consumer {
                            guard.input_queue.end_read_consumer();
                            state.cvar.notify_all();
                        }
                        return Err(RawStdinReadError::Interrupted);
                    }
                    guard.visible_input_prompt = None;
                    break QueueReadAction::InputWait {
                        prompt: String::new(),
                    };
                }
            }
        };

        match action {
            QueueReadAction::Line {
                bytes,
                detached_request,
                emit_input_line,
                ..
            } => {
                if emit_input_line {
                    ipc::emit_input_line("", &String::from_utf8_lossy(&bytes));
                }
                output.extend(bytes);
                if detached_request
                    && complete_detached_read_request(true).map_err(RawStdinReadError::Runtime)?
                {
                    return Err(RawStdinReadError::Interrupted);
                }
            }
            QueueReadAction::InputWait { prompt } => {
                emit_plots();
                mark_input_wait_completed_request();
                remember_emitted_prompt(&prompt);
                if emit_input_wait_sideband(&prompt, true).map_err(RawStdinReadError::Runtime)? {
                    if owns_consumer {
                        release_read_consumer(state);
                    }
                    return Err(RawStdinReadError::Interrupted);
                }
            }
            QueueReadAction::Interrupted => {
                if owns_consumer {
                    release_read_consumer(state);
                }
                return Err(RawStdinReadError::Interrupted);
            }
            QueueReadAction::Shutdown => return Ok(output),
        }
    }
    if owns_consumer {
        release_read_consumer(state);
    }
    Ok(output)
}

fn complete_detached_read_request(release_gil_while_waiting: bool) -> Result<bool, String> {
    let state = session_state();
    {
        let mut guard = state.inner.lock().unwrap();
        guard.request_active = false;
        guard.visible_input_prompt = None;
    }
    try_emit_ready_sideband(release_gil_while_waiting)
}

unsafe extern "C" fn mcp_repl_readline(
    stdin: *mut libc::FILE,
    _stdout: *mut libc::FILE,
    prompt: *const c_char,
) -> *mut c_char {
    #[cfg(any(target_family = "unix", windows))]
    let _ = stdin;
    let prompt_text = if prompt.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(prompt) }
            .to_string_lossy()
            .into_owned()
    };
    #[cfg(target_family = "unix")]
    if ipc::worker_ipc_disabled_for_process() {
        return allocate_readline_result(&[]);
    }
    #[cfg(target_family = "unix")]
    flush_original_stdio();
    #[cfg(all(not(target_family = "unix"), not(windows)))]
    handle_input_hook();

    #[cfg(windows)]
    flush_original_stdio();
    #[cfg(any(target_family = "unix", windows))]
    let read = match read_queue_line(&prompt_text, !prompt_text.is_empty(), false) {
        Ok(read) => read,
        Err(err) => {
            record_protocol_failure(&err);
            set_callback_error(&err);
            return ptr::null_mut();
        }
    };
    #[cfg(all(not(target_family = "unix"), not(windows)))]
    let read = read_stdio_line_bytes(stdin);
    if read.interrupted {
        #[cfg(target_family = "unix")]
        unix_stdin::flush_terminal_input();
    }
    let accounting = match note_cpython_readline_bytes_read(&prompt_text, &read.bytes) {
        Ok(accounting) => accounting,
        Err(err) => {
            emit_protocol_failure(&err);
            set_callback_error(&err);
            return ptr::null_mut();
        }
    };
    if accounting.discarded_after_interrupt() {
        return allocate_readline_result(b"\n");
    }
    if read.interrupted {
        #[cfg(not(windows))]
        PythonApi::global().set_interrupt();
        return ptr::null_mut();
    }
    #[cfg(not(windows))]
    if take_interrupt_requested() {
        PythonApi::global().set_interrupt();
        return ptr::null_mut();
    }

    allocate_readline_result(&read.bytes)
}

fn allocate_readline_result(bytes: &[u8]) -> *mut c_char {
    let api = PythonApi::global();
    let result = unsafe { (api.py_mem_raw_malloc)(bytes.len().saturating_add(1)) }.cast::<c_char>();
    if result.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr().cast::<c_char>(), result, bytes.len());
        *result.add(bytes.len()) = 0;
    }
    result
}

#[cfg(target_family = "unix")]
fn note_cpython_readline_bytes_read(
    prompt: &str,
    bytes: &[u8],
) -> Result<StdinReadAccounting, String> {
    unix_stdin::note_cpython_readline_bytes_read(prompt, bytes)
}

#[cfg(not(target_family = "unix"))]
fn note_cpython_readline_bytes_read(
    _prompt: &str,
    bytes: &[u8],
) -> Result<StdinReadAccounting, String> {
    note_stdin_line_read("", bytes)
}

enum CStdinLine {
    Line(String),
    Eof,
    Error,
}

fn read_c_stdin_line(prompt: &str) -> CStdinLine {
    #[cfg(target_family = "unix")]
    if ipc::worker_ipc_disabled_for_process() {
        return unix_stdin::fork_child_stdin_eof(prompt);
    }

    let stdin = PYTHON_STDIN_FILE.load(Ordering::SeqCst);
    if stdin.is_null() {
        set_callback_error("Python stdio files are not initialized");
        return CStdinLine::Error;
    }

    let prompt_for_sideband = match CString::new(prompt) {
        Ok(prompt) => prompt,
        Err(err) => {
            set_callback_error(&format!("readline prompt contains NUL: {err}"));
            return CStdinLine::Error;
        }
    };

    #[cfg(target_family = "unix")]
    flush_original_stdio();
    #[cfg(all(not(target_family = "unix"), not(windows)))]
    {
        flush_original_stdio();
        handle_input_hook();
        emit_output_text(TextStream::Stdout, prompt.as_bytes());
    }
    #[cfg(windows)]
    flush_original_stdio();
    #[cfg(any(target_family = "unix", windows))]
    let read = match read_queue_line(
        prompt_for_sideband.to_str().unwrap_or(""),
        !prompt.is_empty(),
        true,
    ) {
        Ok(read) => read,
        Err(err) => {
            record_protocol_failure(&err);
            set_callback_error(&err);
            return CStdinLine::Error;
        }
    };
    #[cfg(all(not(target_family = "unix"), not(windows)))]
    let read = read_stdio_line_bytes_allowing_python_threads(stdin);
    if read.interrupted {
        #[cfg(target_family = "unix")]
        unix_stdin::flush_terminal_input();
    }
    let accounting =
        match note_stdin_line_read(prompt_for_sideband.to_str().unwrap_or(""), &read.bytes) {
            Ok(accounting) => accounting,
            Err(err) => {
                emit_protocol_failure(&err);
                set_callback_error(&err);
                return CStdinLine::Error;
            }
        };
    if accounting.discarded_after_interrupt() {
        return CStdinLine::Line("\n".to_string());
    }
    if read.interrupted {
        #[cfg(not(windows))]
        PythonApi::global().set_interrupt();
        return CStdinLine::Error;
    }
    #[cfg(not(windows))]
    if take_interrupt_requested() {
        PythonApi::global().set_interrupt();
        return CStdinLine::Error;
    }
    if read.bytes.is_empty() {
        CStdinLine::Eof
    } else {
        CStdinLine::Line(String::from_utf8_lossy(&read.bytes).to_string())
    }
}

#[cfg(target_family = "unix")]
fn read_raw_stdin_bytes(size: usize) -> Result<Vec<u8>, RawStdinReadError> {
    if ipc::worker_ipc_disabled_for_process() {
        return Ok(Vec::new());
    }
    read_queue_raw_bytes(size)
}

#[cfg(windows)]
fn read_raw_stdin_bytes(size: usize) -> Result<Vec<u8>, RawStdinReadError> {
    read_queue_raw_bytes(size)
}

#[cfg(not(any(target_family = "unix", windows)))]
fn read_raw_stdin_bytes(_size: usize) -> Result<Vec<u8>, RawStdinReadError> {
    Ok(Vec::new())
}

#[cfg(target_family = "unix")]
fn note_stdin_line_read(prompt: &str, bytes: &[u8]) -> Result<StdinReadAccounting, String> {
    unix_stdin::note_stdin_line_read(prompt, bytes)
}

#[cfg(not(target_family = "unix"))]
fn note_stdin_line_read(_prompt: &str, _bytes: &[u8]) -> Result<StdinReadAccounting, String> {
    Ok(StdinReadAccounting::Accounted)
}

fn plot_capable() -> bool {
    let _gil = GilGuard::acquire();
    let api = PythonApi::global();
    let Ok(main) = api.import_module("__main__") else {
        return false;
    };
    let Ok(func) = api.get_attr_string(main.as_ptr(), "_mcp_repl_plot_capable") else {
        api.clear_error();
        return false;
    };
    let result = unsafe { (api.py_object_call_object)(func.as_ptr(), ptr::null_mut()) };
    let Ok(result) = PyPtr::from_owned(result, "plot capability call failed") else {
        api.clear_error();
        return false;
    };
    unsafe { (api.py_object_is_true)(result.as_ptr()) == 1 }
}

fn emit_plots() {
    if !request_active() {
        return;
    }
    let _gil = GilGuard::acquire();
    let api = PythonApi::global();
    let Ok(main) = api.import_module("__main__") else {
        api.clear_error();
        return;
    };
    let Ok(func) = api.get_attr_string(main.as_ptr(), "_mcp_repl_emit_plots") else {
        api.clear_error();
        return;
    };
    let result = unsafe { (api.py_object_call_object)(func.as_ptr(), ptr::null_mut()) };
    if result.is_null() {
        api.clear_error();
    } else {
        drop(PyPtr::from_owned(result, "plot emission result"));
    }
}

#[cfg(target_family = "unix")]
fn record_background_plots() {
    let _gil = GilGuard::acquire();
    let api = PythonApi::global();
    let Ok(main) = api.import_module("__main__") else {
        return;
    };
    let Ok(func) = api.get_attr_string(main.as_ptr(), "_mcp_repl_record_background_plots") else {
        return;
    };
    let result = unsafe { (api.py_object_call_object)(func.as_ptr(), ptr::null_mut()) };
    if let Ok(result) = PyPtr::from_owned(result, "Python background plot recording failed") {
        drop(result);
    }
}

#[cfg(not(target_family = "unix"))]
fn record_background_plots() {}

fn flush_original_stdio() {
    {
        let _gil = GilGuard::acquire();
        let api = PythonApi::global();
        let Ok(main) = api.import_module("__main__") else {
            api.clear_error();
            unsafe {
                libc::fflush(ptr::null_mut());
            }
            return;
        };
        let Ok(func) = api.get_attr_string(main.as_ptr(), "_mcp_repl_flush_original_stdio") else {
            api.clear_error();
            unsafe {
                libc::fflush(ptr::null_mut());
            }
            return;
        };
        let result = unsafe { (api.py_object_call_object)(func.as_ptr(), ptr::null_mut()) };
        if result.is_null() {
            api.clear_error();
        } else {
            drop(PyPtr::from_owned(result, "original stdio flush result"));
        }
    }
    unsafe {
        libc::fflush(ptr::null_mut());
    }
}

fn finish_session_end() {
    let state = session_state();
    let mut guard = state.inner.lock().unwrap();
    let should_emit = !guard.session_end_emitted;
    let protocol_failure = guard.protocol_failure.clone();
    guard.session_end_emitted = true;
    guard.shutdown = true;
    guard.request_active = false;
    guard.cell_running = false;
    guard.visible_input_prompt = None;
    guard.input_queue.clear_after_interrupt();
    drop(guard);
    state.cvar.notify_all();

    if !should_emit {
        return;
    }
    if let Some(message) = protocol_failure {
        ipc::emit_session_end_with_reason_and_message("protocol_error", &message);
    } else {
        ipc::emit_session_end();
    }
}

fn emit_output_text(stream: TextStream, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    match ipc::emit_output_text(stream, bytes) {
        Ok(()) => {}
        Err(_) if ipc::worker_ipc_disabled_for_process() => match stream {
            TextStream::Stdout => crate::output_stream::write_stdout_bytes(bytes),
            TextStream::Stderr => crate::output_stream::write_stderr_bytes(bytes),
        },
        Err(err) => panic!("failed to send Python output over worker IPC: {err}"),
    }
}

unsafe extern "C" fn initialize_mcp_repl_module() -> *mut PyObject {
    let api = PythonApi::global();
    let methods = [
        ModuleMethod {
            name: "readline",
            function: py_readline,
        },
        ModuleMethod {
            name: "write",
            function: py_write,
        },
        ModuleMethod {
            name: "write_bytes",
            function: py_write_bytes,
        },
        ModuleMethod {
            name: "raw_stdin_read",
            function: py_raw_stdin_read,
        },
        ModuleMethod {
            name: "restore_readline_function",
            function: py_restore_readline_function,
        },
        ModuleMethod {
            name: "request_exit",
            function: py_request_exit,
        },
        ModuleMethod {
            name: "emit_plot_image",
            function: py_emit_output_image,
        },
        ModuleMethod {
            name: "set_python_prompts",
            function: py_set_python_prompts,
        },
        ModuleMethod {
            name: "has_request_active",
            function: py_has_request_active,
        },
        ModuleMethod {
            name: "take_plot_reset_pending",
            function: py_take_plot_reset_pending,
        },
    ];
    api.create_module("_mcp_repl", &methods)
}

unsafe extern "C" fn py_readline(_self: *mut PyObject, args: *mut PyObject) -> *mut PyObject {
    let api = PythonApi::global();
    if api.tuple_size(args) != 1 {
        set_callback_error("readline expects exactly one argument");
        return ptr::null_mut();
    }
    let Some(prompt) = api.unicode_arg(args, 0) else {
        return ptr::null_mut();
    };
    match read_c_stdin_line(&prompt) {
        CStdinLine::Line(line) => match api.unicode(&line) {
            Ok(value) => value.into_raw(),
            Err(_) => ptr::null_mut(),
        },
        CStdinLine::Eof => api.none(),
        CStdinLine::Error => ptr::null_mut(),
    }
}

unsafe extern "C" fn py_write(_self: *mut PyObject, args: *mut PyObject) -> *mut PyObject {
    let api = PythonApi::global();
    if api.tuple_size(args) != 2 {
        set_callback_error("write expects exactly two arguments");
        return ptr::null_mut();
    }
    let Some(stream) = api.unicode_arg(args, 0) else {
        return ptr::null_mut();
    };
    let Some(message) = api.unicode_arg(args, 1) else {
        return ptr::null_mut();
    };
    let stream = match stream.as_str() {
        "stdout" => TextStream::Stdout,
        "stderr" => TextStream::Stderr,
        _ => {
            set_callback_error("write stream must be 'stdout' or 'stderr'");
            return ptr::null_mut();
        }
    };
    emit_output_text(stream, message.as_bytes());
    api.long_result(message.chars().count() as c_long)
}

unsafe extern "C" fn py_write_bytes(_self: *mut PyObject, args: *mut PyObject) -> *mut PyObject {
    let api = PythonApi::global();
    if api.tuple_size(args) != 2 {
        set_callback_error("write_bytes expects exactly two arguments");
        return ptr::null_mut();
    }
    let Some(stream) = api.unicode_arg(args, 0) else {
        return ptr::null_mut();
    };
    let Some(bytes) = api.bytes_arg(args, 1) else {
        return ptr::null_mut();
    };
    let stream = match stream.as_str() {
        "stdout" => TextStream::Stdout,
        "stderr" => TextStream::Stderr,
        _ => {
            set_callback_error("write_bytes stream must be 'stdout' or 'stderr'");
            return ptr::null_mut();
        }
    };
    emit_output_text(stream, &bytes);
    api.long_result(bytes.len() as c_long)
}

unsafe extern "C" fn py_raw_stdin_read(_self: *mut PyObject, args: *mut PyObject) -> *mut PyObject {
    let api = PythonApi::global();
    if api.tuple_size(args) != 1 {
        set_callback_error("raw_stdin_read expects exactly one argument");
        return ptr::null_mut();
    }
    let Some(size) = api.long_arg(args, 0) else {
        return ptr::null_mut();
    };
    let Ok(size) = usize::try_from(size) else {
        set_callback_error("raw_stdin_read size must be non-negative");
        return ptr::null_mut();
    };
    let bytes = match read_raw_stdin_bytes(size) {
        Ok(bytes) => bytes,
        Err(RawStdinReadError::Interrupted) => {
            #[cfg(not(windows))]
            api.set_interrupt();
            return ptr::null_mut();
        }
        Err(RawStdinReadError::Runtime(message)) => {
            record_protocol_failure(&message);
            set_callback_error(&message);
            return ptr::null_mut();
        }
    };
    match api.bytes(&bytes) {
        Ok(value) => value.into_raw(),
        Err(_) => ptr::null_mut(),
    }
}

unsafe extern "C" fn py_restore_readline_function(
    _self: *mut PyObject,
    args: *mut PyObject,
) -> *mut PyObject {
    let api = PythonApi::global();
    if api.tuple_size(args) != 0 {
        set_callback_error("restore_readline_function expects no arguments");
        return ptr::null_mut();
    }
    if let Err(err) = api.install_readline_function(mcp_repl_readline) {
        set_callback_error(&err);
        return ptr::null_mut();
    }
    api.none()
}

unsafe extern "C" fn py_request_exit(_self: *mut PyObject, args: *mut PyObject) -> *mut PyObject {
    let api = PythonApi::global();
    if api.tuple_size(args) != 0 {
        set_callback_error("request_exit expects no arguments");
        return ptr::null_mut();
    }
    if let Err(err) = request_exit() {
        record_protocol_failure(&err);
        set_callback_error(&err);
        return ptr::null_mut();
    }
    api.none()
}

unsafe extern "C" fn py_emit_output_image(
    _self: *mut PyObject,
    args: *mut PyObject,
) -> *mut PyObject {
    let api = PythonApi::global();
    if api.tuple_size(args) != 4 {
        set_callback_error("emit_plot_image expects exactly four arguments");
        return ptr::null_mut();
    }
    let Some(mime_type) = api.unicode_arg(args, 0) else {
        return ptr::null_mut();
    };
    let Some(data) = api.unicode_arg(args, 1) else {
        return ptr::null_mut();
    };
    let is_update = unsafe { (api.py_tuple_get_item)(args, 2) };
    if is_update.is_null() {
        return ptr::null_mut();
    }
    let is_update = unsafe { (api.py_object_is_true)(is_update) };
    if is_update < 0 {
        return ptr::null_mut();
    }
    let Some(source) = api.unicode_arg(args, 3) else {
        return ptr::null_mut();
    };
    ipc::emit_output_image(&mime_type, &data, is_update == 1, Some(&source));
    api.none()
}

unsafe extern "C" fn py_set_python_prompts(
    _self: *mut PyObject,
    args: *mut PyObject,
) -> *mut PyObject {
    let api = PythonApi::global();
    if api.tuple_size(args) != 2 {
        set_callback_error("set_python_prompts expects exactly two arguments");
        return ptr::null_mut();
    }
    let Some(primary) = api.unicode_arg(args, 0) else {
        return ptr::null_mut();
    };
    let Some(continuation) = api.unicode_arg(args, 1) else {
        return ptr::null_mut();
    };
    set_python_prompts(primary, continuation);
    api.none()
}

unsafe extern "C" fn py_has_request_active(
    _self: *mut PyObject,
    _args: *mut PyObject,
) -> *mut PyObject {
    PythonApi::global().bool_result(request_active())
}

unsafe extern "C" fn py_take_plot_reset_pending(
    _self: *mut PyObject,
    _args: *mut PyObject,
) -> *mut PyObject {
    let Some(state) = SESSION_STATE.get() else {
        return PythonApi::global().bool_result(false);
    };
    let mut guard = state.inner.lock().unwrap();
    let pending = guard.plot_reset_pending;
    guard.plot_reset_pending = false;
    PythonApi::global().bool_result(pending)
}

fn set_callback_error(message: &str) {
    let exception = RUNTIME_ERROR.load(Ordering::SeqCst);
    if exception.is_null() {
        return;
    }
    PythonApi::global().set_runtime_error(exception, message);
}

static SESSION: OnceLock<PythonSession> = OnceLock::new();
static RUNTIME_ERROR: AtomicPtr<PyObject> = AtomicPtr::new(ptr::null_mut());
