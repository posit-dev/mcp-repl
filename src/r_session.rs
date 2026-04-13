use std::collections::{HashMap, VecDeque};
use std::ffi::{CStr, CString};
use std::hash::{Hash, Hasher};
use std::os::raw::{c_char, c_int, c_uchar};
use std::path::Path;
#[cfg(target_family = "unix")]
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, OnceLock, mpsc};
use std::thread;

use crate::ipc;
#[cfg(target_family = "unix")]
use crate::sandbox::R_SESSION_TMPDIR_ENV;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use harp::command::{r_command, r_home_setup};
use harp::exec::{RFunction, RFunctionExt};
use harp::library::RLibraries;

#[cfg(target_family = "unix")]
use libr::{
    R_Consolefile, R_Outputfile, ptr_R_Busy, ptr_R_ReadConsole, ptr_R_ShowMessage, ptr_R_Suicide,
    ptr_R_WriteConsole, ptr_R_WriteConsoleEx,
};
#[cfg(target_family = "windows")]
use libr::{
    R_DefParamsEx, R_SetParams, R_common_command_line, Rboolean_FALSE, Rboolean_TRUE, Rstart,
    UImode_RTerm, cmdlineoptions, get_R_HOME, getRUser, readconsolecfg,
};
#[cfg(target_family = "windows")]
use std::mem::MaybeUninit;
#[cfg(target_family = "windows")]
use windows_sys::Win32::Globalization::{GetACP, MultiByteToWideChar};

const MCP_REPL_R_SCRIPT: &str = include_str!("../r/mcp_repl.R");

#[derive(Debug)]
pub struct RequestCompleted;

pub struct RSession {
    sender: mpsc::Sender<RRequest>,
    init: Arc<SessionInit>,
}

impl RSession {
    pub fn global() -> Result<&'static RSession, String> {
        SESSION
            .get()
            .ok_or_else(|| "R session not initialized".to_string())
    }

    pub fn start_on_current_thread() -> Result<(), String> {
        let (request_tx, request_rx) = mpsc::channel();
        let init = Arc::new(SessionInit::new());
        let session = RSession {
            sender: request_tx,
            init: init.clone(),
        };
        let session_set = SESSION.set(session);
        if session_set.is_err() {
            return Err("R session already initialized".to_string());
        }
        run_session_on_current_thread(request_rx, init)
    }

    pub fn wait_until_ready(&self) -> Result<(), String> {
        self.init.wait_ready()
    }

    pub fn send_request(&self, input: String) -> Result<mpsc::Receiver<RequestCompleted>, String> {
        self.wait_until_ready()?;
        let (reply_tx, reply_rx) = mpsc::channel();
        let request = RRequest {
            input,
            reply: reply_tx,
        };
        self.sender
            .send(request)
            .map_err(|_| "R session is unavailable".to_string())?;
        Ok(reply_rx)
    }
}

struct RRequest {
    input: String,
    reply: mpsc::Sender<RequestCompleted>,
}

#[derive(Debug)]
enum InitState {
    Pending,
    Ready,
    Failed(String),
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

    fn mark_failed(&self, message: String) {
        let mut guard = self.state.lock().unwrap();
        *guard = InitState::Failed(message);
        self.cvar.notify_all();
    }

    fn wait_ready(&self) -> Result<(), String> {
        let mut guard = self.state.lock().unwrap();
        loop {
            match &*guard {
                InitState::Pending => {
                    guard = self.cvar.wait(guard).unwrap();
                }
                InitState::Ready => return Ok(()),
                InitState::Failed(message) => return Err(message.clone()),
            }
        }
    }
}

pub fn request_shutdown() -> bool {
    let Some(state) = SESSION_STATE.get() else {
        return false;
    };
    let mut guard = state.inner.lock().unwrap();
    guard.shutdown = true;
    state.cvar.notify_all();
    true
}

pub(crate) fn clear_pending_input() -> bool {
    let Some(state) = SESSION_STATE.get() else {
        return false;
    };
    let mut guard = state.inner.lock().unwrap();
    let had_pending = !guard.input_queue.is_empty();
    guard.input_queue.clear();
    had_pending
}

#[cfg(target_family = "windows")]
pub(crate) fn complete_active_request_if_idle() -> bool {
    let Some(state) = SESSION_STATE.get() else {
        return false;
    };
    let mut guard = state.inner.lock().unwrap();
    if !guard.input_queue.is_empty() {
        return false;
    }
    let active = guard.active_request.take();
    let had_active = active.is_some();
    drop(guard);
    if had_active {
        complete_active_request(state, active, false);
    }
    had_active
}

fn run_session_on_current_thread(
    requests: mpsc::Receiver<RRequest>,
    init: Arc<SessionInit>,
) -> Result<(), String> {
    crate::diagnostics::startup_log("r-session: init begin");
    let state = Arc::new(SessionState::new());
    if SESSION_STATE.set(state.clone()).is_err() {
        let message = "R session state already initialized".to_string();
        init.mark_failed(message.clone());
        return Err(message);
    }

    let init_start = std::time::Instant::now();
    let init_result = initialize_r();
    if let Err(err) = init_result {
        init.mark_failed(err.clone());
        return Err(err);
    }
    crate::diagnostics::startup_log(format!(
        "r-session: init complete ({} ms)",
        crate::diagnostics::elapsed_ms(init_start.elapsed())
    ));

    init.mark_ready();

    let request_state = state.clone();
    thread::Builder::new()
        .name("r-session-requests".to_string())
        .spawn(move || handle_requests(requests, request_state))
        .expect("failed to spawn request handler thread");

    unsafe {
        libr::run_Rmainloop();
    }

    Ok(())
}

struct SessionState {
    inner: Mutex<SessionStateInner>,
    cvar: Condvar,
}

struct SessionStateInner {
    input_queue: VecDeque<String>,
    active_request: Option<ActiveRequest>,
    shutdown: bool,
    session_end_emitted: bool,
}

struct ActiveRequest {
    reply: mpsc::Sender<RequestCompleted>,
    plot_hashes: HashMap<String, u64>,
}

impl SessionState {
    fn new() -> Self {
        Self {
            inner: Mutex::new(SessionStateInner {
                input_queue: VecDeque::new(),
                active_request: None,
                shutdown: false,
                session_end_emitted: false,
            }),
            cvar: Condvar::new(),
        }
    }
}

fn handle_requests(requests: mpsc::Receiver<RRequest>, state: Arc<SessionState>) {
    for request in requests {
        let mut guard = state.inner.lock().unwrap();
        while guard.active_request.is_some() {
            guard = state.cvar.wait(guard).unwrap();
        }
        let is_eof = is_eof_input(&request.input);
        guard.active_request = Some(ActiveRequest {
            reply: request.reply,
            plot_hashes: HashMap::new(),
        });
        if is_eof {
            guard.shutdown = true;
        } else {
            queue_input(&mut guard.input_queue, &request.input);
        }
        state.cvar.notify_all();
    }

    let mut guard = state.inner.lock().unwrap();
    guard.shutdown = true;
    state.cvar.notify_all();
}

fn initialize_r() -> Result<(), String> {
    let start = std::time::Instant::now();
    let r_home = r_home_setup().map_err(|err| format!("failed to set up R_HOME: {err}"))?;
    crate::diagnostics::startup_log(format!(
        "r-session: r_home_setup {} ms",
        crate::diagnostics::elapsed_ms(start.elapsed())
    ));
    configure_r_env_vars(&r_home);
    #[cfg(target_family = "unix")]
    configure_r_tempdir();

    let libs_start = std::time::Instant::now();
    let libraries = RLibraries::from_r_home_path(&r_home);
    libraries.initialize_pre_setup_r();
    crate::diagnostics::startup_log(format!(
        "r-session: libraries pre-setup {} ms",
        crate::diagnostics::elapsed_ms(libs_start.elapsed())
    ));

    // Mirror the default R startup as closely as possible: delegate to R's
    // own initialization logic (Rf_initialize_R + command-line parsing) and
    // avoid disabling user/site startup files.
    //
    // We keep the console quiet and interactive, and preserve the existing
    // behavior of not restoring/saving the workspace automatically.
    let args = vec![
        "--quiet".to_string(),
        "--interactive".to_string(),
        "--no-restore".to_string(),
        "--no-save".to_string(),
    ];
    let setup_start = std::time::Instant::now();
    setup_r(&args)?;
    crate::diagnostics::startup_log(format!(
        "r-session: setup_r {} ms",
        crate::diagnostics::elapsed_ms(setup_start.elapsed())
    ));

    let post_start = std::time::Instant::now();
    libraries.initialize_post_setup_r();
    crate::diagnostics::startup_log(format!(
        "r-session: libraries post-setup {} ms",
        crate::diagnostics::elapsed_ms(post_start.elapsed())
    ));

    unsafe {
        harp::CONSOLE_THREAD_ID = Some(thread::current().id());
        harp::routines::r_register_routines();
    }
    harp::initialize();
    let help_start = std::time::Instant::now();
    configure_r_help_output()?;
    crate::diagnostics::startup_log(format!(
        "r-session: help output setup {} ms",
        crate::diagnostics::elapsed_ms(help_start.elapsed())
    ));

    crate::diagnostics::startup_log(format!(
        "r-session: initialize_r total {} ms",
        crate::diagnostics::elapsed_ms(start.elapsed())
    ));
    Ok(())
}

fn configure_r_help_output() -> Result<(), String> {
    eval_in_global_env(MCP_REPL_R_SCRIPT)
}

fn eval_in_global_env(code: &str) -> Result<(), String> {
    // Parse from explicit lines so CRLF content from include_str! on Windows
    // cannot leak carriage returns into the parser input stream.
    let parse_lines: Vec<String> = code
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .split('\n')
        .map(str::to_string)
        .collect();
    let mut parse = RFunction::from("parse");
    parse.param("text", parse_lines);
    let exprs = parse
        .call()
        .map_err(|err| format!("failed to parse R startup code: {err}"))?;

    let mut globalenv_fn = RFunction::from("globalenv");
    let globalenv = globalenv_fn
        .call()
        .map_err(|err| format!("failed to resolve globalenv(): {err}"))?;

    let mut eval = RFunction::from("eval");
    eval.add(exprs);
    eval.param("envir", globalenv);
    eval.call()
        .map_err(|err| format!("failed to eval R startup code: {err}"))?;
    Ok(())
}

fn configure_r_env_vars(r_home: &Path) {
    unsafe {
        std::env::set_var("R_DISABLE_HTTPD", "1");
    }

    let share = r_home.join("share");
    let include = r_home.join("include");
    let doc = r_home.join("doc");

    let ok = share.try_exists().unwrap_or(false)
        && include.try_exists().unwrap_or(false)
        && doc.try_exists().unwrap_or(false);

    if ok {
        unsafe {
            std::env::set_var("R_SHARE_DIR", share);
            std::env::set_var("R_INCLUDE_DIR", include);
            std::env::set_var("R_DOC_DIR", doc);
        }
        return;
    }

    // Fallback for non-standard R layouts.
    let result = r_command(|command| {
        command
            .stdin(std::process::Stdio::null())
            .arg("--vanilla")
            .arg("--slave")
            .arg("-e")
            .arg(r#"cat(paste(R.home("share"), R.home("include"), R.home("doc"), sep=";"))"#);
    });

    if let Ok(output) = result
        && let Ok(vars) = String::from_utf8(output.stdout)
    {
        let vars: Vec<&str> = vars.trim().split(';').collect();
        if vars.len() == 3 {
            unsafe {
                std::env::set_var("R_SHARE_DIR", vars[0]);
                std::env::set_var("R_INCLUDE_DIR", vars[1]);
                std::env::set_var("R_DOC_DIR", vars[2]);
            }
        } else {
            eprintln!("Unexpected output for R env vars");
        }
    } else {
        eprintln!("Failed to discover R env vars");
    }
}

#[cfg(target_family = "unix")]
fn configure_r_tempdir() {
    let Some(tmpdir) = std::env::var_os(R_SESSION_TMPDIR_ENV) else {
        return;
    };
    if tmpdir.is_empty() {
        return;
    }
    let path = PathBuf::from(&tmpdir);
    if !path.is_absolute() {
        eprintln!(
            "Ignoring non-absolute R session temp dir: {}",
            path.to_string_lossy()
        );
        return;
    }
    if path.as_path() == std::path::Path::new("/") {
        eprintln!("Refusing to use '/' as R session temp dir");
        return;
    }

    unsafe {
        std::env::set_var("TMPDIR", &tmpdir);
    }
}

#[cfg(target_family = "unix")]
fn setup_r(args: &[String]) -> Result<(), String> {
    unsafe {
        let (owned_args, mut c_args) = build_c_args_owned(args);
        let _ = R_MAIN_ARGS.set(owned_args);
        libr::Rf_initialize_R(c_args.len() as i32, c_args.as_mut_ptr());

        libr::set(libr::R_Interactive, 1);
        libr::set(R_Consolefile, std::ptr::null_mut());
        libr::set(R_Outputfile, std::ptr::null_mut());

        libr::set(ptr_R_WriteConsole, None);
        libr::set(ptr_R_WriteConsoleEx, Some(r_write_console));
        libr::set(ptr_R_ReadConsole, Some(r_read_console));
        libr::set(ptr_R_ShowMessage, Some(r_show_message));
        libr::set(ptr_R_Busy, Some(r_busy));
        libr::set(ptr_R_Suicide, Some(r_suicide));

        libr::setup_Rmainloop();
    }

    Ok(())
}

#[cfg(target_family = "windows")]
fn setup_r(args: &[String]) -> Result<(), String> {
    unsafe {
        libr::set(libr::R_SignalHandlers, 1);

        let r_home = get_r_home();
        let r_home = CString::new(r_home).map_err(|err| err.to_string())?;
        let r_home = r_home.as_ptr() as *mut c_char;

        let user_home = get_user_home();
        let user_home = CString::new(user_home).map_err(|err| err.to_string())?;
        let user_home = user_home.as_ptr() as *mut c_char;

        let (_tmp_owned, mut c_args) = build_c_args_owned(&[]);
        cmdlineoptions(c_args.len() as i32, c_args.as_mut_ptr());

        let mut params_struct = MaybeUninit::uninit();
        let params: Rstart = params_struct.as_mut_ptr();

        R_DefParamsEx(params, 0);

        let (owned_args, mut c_args) = build_c_args_owned(args);
        let _ = R_MAIN_ARGS.set(owned_args);
        let mut c_args_len = c_args.len() as c_int;
        R_common_command_line(&mut c_args_len, c_args.as_mut_ptr(), params);

        (*params).R_Interactive = 1;
        (*params).CharacterMode = UImode_RTerm;
        // Keep startup behavior aligned with R defaults. R_common_command_line
        // already adjusts these based on the provided command-line arguments.
        (*params).set_NoRenviron(Rboolean_FALSE);

        (*params).WriteConsole = None;
        (*params).WriteConsoleEx = Some(r_write_console);
        (*params).ReadConsole = Some(r_read_console);
        (*params).ShowMessage = Some(r_show_message);
        (*params).YesNoCancel = Some(r_yes_no_cancel);
        (*params).Busy = Some(r_busy);
        (*params).Suicide = Some(r_suicide);
        (*params).CallBack = Some(r_callback);
        // Windows R embeds UTF-8 spans in console output using UTF8in/UTF8out markers.
        // Explicitly enable this (required when using Rstart version 0) so output
        // encoding is deterministic and can be decoded reliably.
        (*params).EmitEmbeddedUTF8 = Rboolean_TRUE;

        (*params).rhome = r_home;
        (*params).home = user_home;

        R_SetParams(params);
        libr::graphapp::GA_initapp(0, std::ptr::null_mut());
        readconsolecfg();
        libr::setup_Rmainloop();
    }

    Ok(())
}

fn build_c_args_owned(args: &[String]) -> (Vec<CString>, Vec<*mut c_char>) {
    let mut owned = Vec::with_capacity(args.len() + 1);
    owned.push(CString::new("mcp-repl").expect("argv[0] must not contain NUL"));
    for arg in args {
        owned.push(CString::new(arg.as_str()).expect("argv must not contain NUL"));
    }
    let ptrs = owned
        .iter()
        .map(|arg| arg.as_ptr() as *mut c_char)
        .collect();
    (owned, ptrs)
}

static SESSION_STATE: OnceLock<Arc<SessionState>> = OnceLock::new();
static SESSION: OnceLock<RSession> = OnceLock::new();
static R_MAIN_ARGS: OnceLock<Vec<CString>> = OnceLock::new();

fn session_state() -> &'static Arc<SessionState> {
    SESSION_STATE
        .get()
        .expect("R session state was not initialized")
}

fn queue_input(queue: &mut VecDeque<String>, input: &str) {
    if input.is_empty() {
        return;
    }
    let normalized = input.replace("\r\n", "\n");
    let mut lines: Vec<String> = normalized
        .split_inclusive('\n')
        .map(str::to_string)
        .collect();
    if !normalized.ends_with('\n') {
        if let Some(last) = lines.last_mut() {
            last.push('\n');
        } else {
            lines.push("\n".to_string());
        }
    }
    queue.extend(lines);
}

fn is_eof_input(input: &str) -> bool {
    let trimmed = input.trim_matches(|ch| ch == '\n' || ch == '\r');
    let mut saw_eot = false;
    for ch in trimmed.chars() {
        match ch {
            '\u{4}' => saw_eot = true,
            _ => return false,
        }
    }
    saw_eot
}

fn write_stdout_bytes(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    crate::output_stream::write_stdout_bytes(bytes);
}

fn write_stderr_bytes(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    crate::output_stream::write_stderr_bytes(bytes);
}

#[cfg(target_family = "windows")]
const UTF8_IN_MARKER: &[u8; 3] = b"\x02\xFF\xFE";
#[cfg(target_family = "windows")]
const UTF8_OUT_MARKER: &[u8; 3] = b"\x03\xFF\xFE";
#[cfg(target_family = "windows")]
static WINDOWS_CONSOLE_DECODE_STATE: OnceLock<Mutex<WindowsConsoleDecodeStates>> = OnceLock::new();

#[cfg(target_family = "windows")]
fn find_marker(bytes: &[u8], marker: &[u8; 3]) -> Option<usize> {
    bytes
        .windows(marker.len())
        .position(|window| window == marker)
}

#[cfg(target_family = "windows")]
fn decode_windows_code_page_bytes(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    if bytes.len() > i32::MAX as usize {
        return String::from_utf8_lossy(bytes).into_owned();
    }

    let code_page = unsafe { GetACP() };

    let input_len = bytes.len() as i32;
    let wide_len = unsafe {
        MultiByteToWideChar(
            code_page,
            0,
            bytes.as_ptr(),
            input_len,
            std::ptr::null_mut(),
            0,
        )
    };
    if wide_len <= 0 {
        return String::from_utf8_lossy(bytes).into_owned();
    }

    let mut wide = vec![0u16; wide_len as usize];
    let written = unsafe {
        MultiByteToWideChar(
            code_page,
            0,
            bytes.as_ptr(),
            input_len,
            wide.as_mut_ptr(),
            wide_len,
        )
    };
    if written <= 0 {
        return String::from_utf8_lossy(bytes).into_owned();
    }

    String::from_utf16_lossy(&wide[..written as usize])
}

#[cfg(target_family = "windows")]
fn decode_windows_embedded_segment(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => decode_windows_code_page_bytes(bytes),
    }
}

#[cfg(target_family = "windows")]
#[derive(Default)]
struct WindowsConsoleDecodeState {
    marker_tail: Vec<u8>,
    utf8_segment: Vec<u8>,
    in_utf8_segment: bool,
}

#[cfg(target_family = "windows")]
#[derive(Default)]
struct WindowsConsoleDecodeStates {
    stdout: WindowsConsoleDecodeState,
    stderr: WindowsConsoleDecodeState,
}

#[cfg(target_family = "windows")]
fn trailing_marker_prefix_len(bytes: &[u8], markers: &[&[u8; 3]]) -> usize {
    let mut keep = 0usize;
    for marker in markers {
        for prefix_len in (1..marker.len()).rev() {
            if bytes.ends_with(&marker[..prefix_len]) {
                keep = keep.max(prefix_len);
                break;
            }
        }
    }
    keep
}

#[cfg(target_family = "windows")]
fn decode_console_bytes_with_state(state: &mut WindowsConsoleDecodeState, bytes: &[u8]) -> Vec<u8> {
    if bytes.is_empty() {
        return Vec::new();
    }

    let mut input = Vec::with_capacity(state.marker_tail.len() + bytes.len());
    input.extend_from_slice(&state.marker_tail);
    state.marker_tail.clear();
    input.extend_from_slice(bytes);

    let mut out = String::new();
    let mut cursor = 0usize;

    while cursor < input.len() {
        let remaining = &input[cursor..];
        if !state.in_utf8_segment {
            if remaining.starts_with(UTF8_IN_MARKER) {
                state.in_utf8_segment = true;
                cursor += UTF8_IN_MARKER.len();
                continue;
            }
            if remaining.starts_with(UTF8_OUT_MARKER) {
                cursor += UTF8_OUT_MARKER.len();
                continue;
            }

            let next_in = find_marker(remaining, UTF8_IN_MARKER);
            let next_out = find_marker(remaining, UTF8_OUT_MARKER);
            let next = match (next_in, next_out) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            };
            if let Some(next) = next {
                if next > 0 {
                    out.push_str(&decode_windows_code_page_bytes(&remaining[..next]));
                    cursor += next;
                }
                continue;
            }

            let keep = trailing_marker_prefix_len(remaining, &[UTF8_IN_MARKER, UTF8_OUT_MARKER]);
            let split = remaining.len().saturating_sub(keep);
            if split > 0 {
                out.push_str(&decode_windows_code_page_bytes(&remaining[..split]));
            }
            if keep > 0 {
                state.marker_tail.extend_from_slice(&remaining[split..]);
            }
            break;
        }

        if let Some(end_rel) = find_marker(remaining, UTF8_OUT_MARKER) {
            let end = cursor + end_rel;
            state.utf8_segment.extend_from_slice(&input[cursor..end]);
            out.push_str(&decode_windows_embedded_segment(&state.utf8_segment));
            state.utf8_segment.clear();
            state.in_utf8_segment = false;
            cursor = end + UTF8_OUT_MARKER.len();
            continue;
        }

        let keep = trailing_marker_prefix_len(remaining, &[UTF8_OUT_MARKER]);
        let split = remaining.len().saturating_sub(keep);
        if split > 0 {
            state.utf8_segment.extend_from_slice(&remaining[..split]);
        }
        if keep > 0 {
            state.marker_tail.extend_from_slice(&remaining[split..]);
        }
        break;
    }

    out.into_bytes()
}

#[cfg(target_family = "windows")]
fn decode_console_bytes_for_channel(otype: c_int, bytes: &[u8]) -> Vec<u8> {
    let state = WINDOWS_CONSOLE_DECODE_STATE.get_or_init(|| Mutex::new(Default::default()));
    let mut guard = state.lock().unwrap();
    if otype == 0 {
        decode_console_bytes_with_state(&mut guard.stdout, bytes)
    } else {
        decode_console_bytes_with_state(&mut guard.stderr, bytes)
    }
}

#[cfg(all(test, target_family = "windows"))]
fn reset_console_decode_state_for_tests() {
    if let Some(state) = WINDOWS_CONSOLE_DECODE_STATE.get() {
        let mut guard = state.lock().unwrap();
        *guard = WindowsConsoleDecodeStates::default();
    }
}

#[cfg(not(target_family = "windows"))]
fn decode_console_bytes_for_channel(_otype: c_int, bytes: &[u8]) -> Vec<u8> {
    bytes.to_vec()
}

fn complete_active_request(
    state: &Arc<SessionState>,
    active: Option<ActiveRequest>,
    emit_session_end: bool,
) {
    if let Some(active) = active {
        // Keep the request boundary coupled to the same R-thread decision that
        // drained the final queued input line.
        ipc::emit_request_end();
        let _ = active.reply.send(RequestCompleted);
        state.cvar.notify_all();
    }
    if emit_session_end {
        ipc::emit_session_end();
    }
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn r_write_console(buf: *const c_char, buflen: c_int, otype: c_int) {
    if buf.is_null() || buflen <= 0 {
        return;
    }
    let bytes = unsafe { std::slice::from_raw_parts(buf as *const u8, buflen as usize) };
    let bytes = decode_console_bytes_for_channel(otype, bytes);
    if otype == 0 {
        write_stdout_bytes(&bytes);
    } else {
        write_stderr_bytes(&bytes);
    }
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn r_show_message(buf: *const c_char) {
    if buf.is_null() {
        return;
    }
    let message = unsafe { CStr::from_ptr(buf) }.to_string_lossy();
    write_stderr_bytes(message.as_bytes());
    write_stderr_bytes(b"\n");
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn r_busy(which: c_int) {
    #[cfg(target_family = "windows")]
    {
        if which == 0 {
            let _ = complete_active_request_if_idle();
        }
    }

    #[cfg(not(target_family = "windows"))]
    {
        let _ = which;
    }
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn r_suicide(buf: *const c_char) {
    let message = if buf.is_null() {
        "R requested shutdown."
    } else {
        unsafe { CStr::from_ptr(buf) }
            .to_str()
            .unwrap_or("R requested shutdown.")
    };
    let state = session_state();
    let mut guard = state.inner.lock().unwrap();
    let should_emit = !guard.session_end_emitted;
    guard.session_end_emitted = true;
    let active = guard.active_request.take();
    drop(guard);
    complete_active_request(state, active, should_emit);
    panic!("{message}");
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn r_read_console(
    prompt: *const c_char,
    buf: *mut c_uchar,
    buflen: c_int,
    _add_history: c_int,
) -> c_int {
    if buflen <= 0 {
        return 0;
    }
    let prompt_text = if prompt.is_null() {
        None
    } else {
        Some(
            unsafe { CStr::from_ptr(prompt) }
                .to_string_lossy()
                .to_string(),
        )
    };
    ipc::emit_readline_start(prompt_text.as_deref().unwrap_or(""));
    let is_save_prompt = prompt_text
        .as_deref()
        .map(|text| text.to_ascii_lowercase().contains("save workspace image"))
        .unwrap_or(false);
    let state = session_state();

    loop {
        let mut guard = state.inner.lock().unwrap();

        if is_save_prompt {
            let active = guard.active_request.take();
            let should_emit = guard.shutdown && !guard.session_end_emitted;
            if guard.shutdown {
                guard.session_end_emitted = true;
            }
            drop(guard);
            complete_active_request(state, active, should_emit);
            if !buf.is_null() {
                let response = b"n\n";
                let max = (buflen as usize).saturating_sub(1);
                if max > 0 {
                    let bytes = if response.len() > max {
                        &response[..max]
                    } else {
                        response
                    };
                    unsafe {
                        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf, bytes.len());
                        *buf.add(bytes.len()) = 0;
                    }
                    return 1;
                }
            }
            return 0;
        }

        if guard.shutdown {
            let should_emit = !guard.session_end_emitted;
            guard.session_end_emitted = true;
            let active = guard.active_request.take();
            drop(guard);
            complete_active_request(state, active, should_emit);
            if !buf.is_null() {
                unsafe { *buf = 0 };
            }
            return 0;
        }

        if let Some(line) = guard.input_queue.pop_front() {
            let mut bytes = line.into_bytes();
            if bytes.is_empty() || *bytes.last().unwrap() != b'\n' {
                bytes.push(b'\n');
            }

            let max = (buflen as usize).saturating_sub(1);
            let (head, tail) = if bytes.len() > max {
                bytes.split_at(max)
            } else {
                (bytes.as_slice(), &[][..])
            };

            if !tail.is_empty() {
                let remainder = String::from_utf8_lossy(tail).to_string();
                guard.input_queue.push_front(remainder);
            }
            drop(guard);

            let prompt = prompt_text.as_deref().unwrap_or("");
            let line_text = String::from_utf8_lossy(head).to_string();
            let mut echoed = String::with_capacity(prompt.len() + line_text.len());
            echoed.push_str(prompt);
            echoed.push_str(&line_text);
            ipc::emit_readline_result(prompt, &line_text);
            if !echoed.is_empty() {
                write_stdout_bytes(echoed.as_bytes());
            }

            if !buf.is_null() {
                unsafe {
                    std::ptr::copy_nonoverlapping(head.as_ptr(), buf, head.len());
                    *buf.add(head.len()) = 0;
                }
            }

            return 1;
        }

        if let Some(active) = guard.active_request.take() {
            drop(guard);
            complete_active_request(state, Some(active), false);
            continue;
        }

        guard = state.cvar.wait(guard).unwrap();
    }
}

pub(crate) fn push_plot_image(
    plot_id: String,
    bytes: Vec<u8>,
    mime_type: String,
    is_new: bool,
) -> Result<(), String> {
    let state = session_state();
    let mut guard = state
        .inner
        .lock()
        .map_err(|_| "session state lock poisoned".to_string())?;
    let Some(active) = guard.active_request.as_mut() else {
        return Ok(());
    };

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    let hash = hasher.finish();

    if active.plot_hashes.get(&plot_id) == Some(&hash) {
        return Ok(());
    }

    active.plot_hashes.insert(plot_id.clone(), hash);
    let mime_type = if mime_type.trim().is_empty() {
        "image/png".to_string()
    } else {
        mime_type
    };
    let data = STANDARD.encode(bytes);
    ipc::emit_plot_image(&plot_id, &mime_type, &data, is_new);

    Ok(())
}

#[cfg(target_family = "windows")]
#[unsafe(no_mangle)]
pub extern "C-unwind" fn r_yes_no_cancel(_question: *const c_char) -> c_int {
    // In embedded Windows sessions this callback can be reached during cleanup
    // when R asks whether to save the workspace image. Returning -1 requests
    // "no save", which keeps shutdown non-interactive.
    -1
}

#[cfg(target_family = "windows")]
#[unsafe(no_mangle)]
pub extern "C-unwind" fn r_callback() {}

#[cfg(target_family = "windows")]
fn get_r_home() -> String {
    let r_path = unsafe { get_R_HOME() };
    if r_path.is_null() {
        panic!("get_R_HOME failed to report an R home.");
    }
    unsafe { CStr::from_ptr(r_path) }
        .to_string_lossy()
        .to_string()
}

#[cfg(target_family = "windows")]
fn get_user_home() -> String {
    let r_path = unsafe { getRUser() };
    if r_path.is_null() {
        panic!("getRUser failed to report a user home directory.");
    }
    unsafe { CStr::from_ptr(r_path) }
        .to_string_lossy()
        .to_string()
}

#[cfg(all(test, target_family = "windows"))]
mod tests {
    use std::sync::{Mutex, OnceLock};

    use super::{
        UTF8_IN_MARKER, UTF8_OUT_MARKER, decode_console_bytes_for_channel,
        reset_console_decode_state_for_tests,
    };

    fn test_mutex() -> &'static Mutex<()> {
        static TEST_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
        TEST_MUTEX.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn decode_console_bytes_strips_embedded_utf8_markers() {
        let _guard = test_mutex().lock().expect("test mutex");
        reset_console_decode_state_for_tests();

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"[1] \"");
        bytes.extend_from_slice(UTF8_IN_MARKER);
        bytes.extend_from_slice(b"after interrupt");
        bytes.extend_from_slice(UTF8_OUT_MARKER);
        bytes.extend_from_slice(b"\"\n");

        let decoded = decode_console_bytes_for_channel(0, &bytes);
        let text = String::from_utf8(decoded).expect("decoder must produce UTF-8");

        assert_eq!(text, "[1] \"after interrupt\"\n");
    }

    #[test]
    fn decode_console_bytes_preserves_embedded_utf8_text() {
        let _guard = test_mutex().lock().expect("test mutex");
        reset_console_decode_state_for_tests();

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"R Help on ");
        bytes.extend_from_slice(UTF8_IN_MARKER);
        let quoted = format!("{}mean{}", '\u{2018}', '\u{2019}');
        bytes.extend_from_slice(quoted.as_bytes());
        bytes.extend_from_slice(UTF8_OUT_MARKER);
        bytes.extend_from_slice(b"\n");

        let decoded = decode_console_bytes_for_channel(0, &bytes);
        let text = String::from_utf8(decoded).expect("decoder must produce UTF-8");

        assert_eq!(text, format!("R Help on {quoted}\n"));
    }

    #[test]
    fn decode_console_bytes_handles_markers_split_across_callbacks() {
        let _guard = test_mutex().lock().expect("test mutex");
        reset_console_decode_state_for_tests();

        let chunk1 = b"[1] \"\x02\xff";
        let chunk2 = b"\xfeafter interrupt\x03\xff";
        let chunk3 = b"\xfe\"\n";

        let out1 = decode_console_bytes_for_channel(0, chunk1);
        let out2 = decode_console_bytes_for_channel(0, chunk2);
        let out3 = decode_console_bytes_for_channel(0, chunk3);

        let merged = [out1, out2, out3].concat();
        let text = String::from_utf8(merged).expect("decoder must produce UTF-8");
        assert_eq!(text, "[1] \"after interrupt\"\n");
    }

    #[test]
    fn decode_console_bytes_does_not_mix_stdout_stderr_marker_state() {
        let _guard = test_mutex().lock().expect("test mutex");
        reset_console_decode_state_for_tests();

        let out1 = decode_console_bytes_for_channel(0, b"\x02\xff\xfecaf");
        let out2 = decode_console_bytes_for_channel(1, b"ERR\n");
        let out3 = decode_console_bytes_for_channel(0, b"\x03\xff\xfe");

        assert!(
            out1.is_empty(),
            "stdout partial UTF-8 segment should be buffered"
        );
        let stderr = String::from_utf8(out2).expect("stderr output should remain UTF-8");
        assert_eq!(stderr, "ERR\n");
        let stdout_tail = String::from_utf8(out3).expect("stdout output should remain UTF-8");
        assert_eq!(stdout_tail, "caf");
    }
}

#[cfg(all(test, not(target_family = "windows")))]
mod non_windows_tests {
    use super::decode_console_bytes_for_channel;

    #[test]
    fn decode_console_bytes_passthrough_on_non_windows_stdout() {
        let input = b"plain output\n";
        assert_eq!(decode_console_bytes_for_channel(0, input), input);
    }

    #[test]
    fn decode_console_bytes_passthrough_on_non_windows_stderr() {
        let input = b"error output\n";
        assert_eq!(decode_console_bytes_for_channel(1, input), input);
    }
}
