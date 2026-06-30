#![allow(unsafe_op_in_unsafe_fn)]

use std::collections::HashMap;
use std::ffi::{OsStr, OsString, c_void};
use std::fs::File;
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle};
use std::path::Path;
use std::thread;

use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
    WAIT_FAILED,
};
use windows_sys::Win32::Storage::FileSystem::GetFileType;
use windows_sys::Win32::System::Console::{
    COORD, ClosePseudoConsole, CreatePseudoConsole, ENABLE_PROCESSED_INPUT, GetConsoleMode,
    GetStdHandle, HPCON, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    SetConsoleCtrlHandler, SetConsoleMode,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_NEW_PROCESS_GROUP, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, CreateProcessW,
    DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE,
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION, STARTUPINFOEXW,
    UpdateProcThreadAttribute, WaitForSingleObject,
};

pub const WINDOWS_CONPTY_ARG: &str = "--windows-conpty";
pub const WINDOWS_CONPTY_REQUEST_ENV: &str = "MCP_REPL_WINDOWS_CONPTY";
const WINDOWS_CONPTY_ATTACHED_ENV: &str = "MCP_REPL_WINDOWS_CONPTY_ATTACHED";

const CONPTY_COLS: i16 = 80;
const CONPTY_ROWS: i16 = 24;

#[link(name = "ucrt")]
unsafe extern "C" {
    fn _isatty(fd: i32) -> i32;
    fn _get_osfhandle(fd: i32) -> isize;
}

pub fn invoked_as_windows_conpty() -> bool {
    std::env::args_os().nth(1).as_deref() == Some(OsStr::new(WINDOWS_CONPTY_ARG))
}

pub fn run_windows_conpty_main() -> ! {
    match windows_conpty_main_impl() {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(1);
        }
    }
}

pub fn emit_stdio_diagnostics_if_requested(label: &str) {
    if std::env::var_os("MCP_REPL_STDIO_DIAG").is_none() {
        return;
    }
    unsafe {
        for fd in 0..=2 {
            let handle = _get_osfhandle(fd) as HANDLE;
            let mut mode = 0u32;
            let console = GetConsoleMode(handle, &mut mode) != 0;
            eprintln!(
                "STDIO_DIAG {label} fd={fd} isatty={} handle={handle:p} file_type={} console={console} mode={mode}",
                _isatty(fd),
                GetFileType(handle)
            );
        }
        for (name, code) in [
            ("stdin", STD_INPUT_HANDLE),
            ("stdout", STD_OUTPUT_HANDLE),
            ("stderr", STD_ERROR_HANDLE),
        ] {
            let handle = GetStdHandle(code);
            let mut mode = 0u32;
            let console = GetConsoleMode(handle, &mut mode) != 0;
            eprintln!(
                "STDIO_DIAG {label} std={name} handle={handle:p} file_type={} console={console} mode={mode}",
                GetFileType(handle)
            );
        }
    }
}

pub fn attach_stdio_to_conpty_if_attached() -> Result<(), String> {
    if std::env::var_os(WINDOWS_CONPTY_ATTACHED_ENV).is_none() {
        return Ok(());
    }
    crate::diagnostics::startup_log("windows-conpty: attaching stdio");
    enable_ctrl_c_processing()?;
    rebind_crt_fd_to_conpty_device(0, "CONIN$", libc::O_RDONLY | libc::O_TEXT).map_err(|err| {
        crate::diagnostics::startup_log(format!("windows-conpty: attach stdin failed: {err}"));
        err
    })?;
    rebind_crt_fd_to_conpty_device(1, "CONOUT$", libc::O_WRONLY | libc::O_TEXT).map_err(|err| {
        crate::diagnostics::startup_log(format!("windows-conpty: attach stdout failed: {err}"));
        err
    })?;
    rebind_crt_fd_to_conpty_device(2, "CONOUT$", libc::O_WRONLY | libc::O_TEXT).map_err(|err| {
        crate::diagnostics::startup_log(format!("windows-conpty: attach stderr failed: {err}"));
        err
    })?;
    crate::diagnostics::startup_log("windows-conpty: attached stdio");
    Ok(())
}

fn enable_ctrl_c_processing() -> Result<(), String> {
    if unsafe { SetConsoleCtrlHandler(None, 0) } == 0 {
        return Err(format!(
            "failed to enable Windows Ctrl-C processing: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn enable_processed_input(handle: HANDLE) -> Result<(), String> {
    let mut mode = 0u32;
    if unsafe { GetConsoleMode(handle, &mut mode) } == 0 {
        return Err(format!(
            "failed to read Windows console input mode: {}",
            std::io::Error::last_os_error()
        ));
    }
    if mode & ENABLE_PROCESSED_INPUT == 0
        && unsafe { SetConsoleMode(handle, mode | ENABLE_PROCESSED_INPUT) } == 0
    {
        return Err(format!(
            "failed to enable Windows processed input: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn rebind_crt_fd_to_conpty_device(fd: i32, device: &str, flags: i32) -> Result<(), String> {
    let file = if flags & libc::O_WRONLY != 0 {
        OpenOptions::new()
            .write(true)
            .open(device)
            .map_err(|err| format!("failed to open {device} for fd {fd}: {err}"))?
    } else {
        OpenOptions::new()
            .read(true)
            .open(device)
            .map_err(|err| format!("failed to open {device} for fd {fd}: {err}"))?
    };
    if fd == 0 {
        enable_processed_input(file.as_raw_handle() as HANDLE)?;
    }
    let handle = file.into_raw_handle();
    let new_fd = unsafe { libc::open_osfhandle(handle as isize, flags) };
    if new_fd < 0 {
        unsafe {
            CloseHandle(handle as HANDLE);
        }
        return Err(format!(
            "failed to convert {device} handle into CRT fd {fd}: {}",
            std::io::Error::last_os_error()
        ));
    }
    if unsafe { libc::dup2(new_fd, fd) } != 0 {
        let err = std::io::Error::last_os_error();
        unsafe {
            libc::close(new_fd);
        }
        return Err(format!("failed to duplicate {device} onto fd {fd}: {err}"));
    }
    unsafe {
        libc::close(new_fd);
    }
    Ok(())
}

fn windows_conpty_main_impl() -> Result<i32, String> {
    crate::diagnostics::startup_log("windows-conpty: begin");
    let command = parse_windows_conpty_args(std::env::args_os().skip(1).collect())?;
    crate::diagnostics::startup_log("windows-conpty: parsed args");
    run_conpty_command_with_env_map(&command, std::env::vars().collect(), None)
}

fn parse_windows_conpty_args(raw_args: Vec<OsString>) -> Result<Vec<String>, String> {
    let mut command = Vec::new();
    let mut args = raw_args.into_iter();
    while let Some(arg) = args.next() {
        if arg == WINDOWS_CONPTY_ARG {
            continue;
        }
        if arg == "--" {
            command.extend(args.map(|value| value.to_string_lossy().to_string()));
            break;
        }
        return Err(format!("unknown argument: {}", arg.to_string_lossy()));
    }
    if command.is_empty() {
        return Err("no command specified to execute".to_string());
    }
    Ok(command)
}

pub fn request_env_enabled(env_map: &HashMap<String, String>) -> bool {
    env_get_case_insensitive(env_map, WINDOWS_CONPTY_REQUEST_ENV)
        .is_some_and(|value| matches!(value, "1" | "true" | "TRUE" | "yes" | "YES"))
}

pub fn run_conpty_command_with_env_map(
    command: &[String],
    mut env_map: HashMap<String, String>,
    cwd: Option<&Path>,
) -> Result<i32, String> {
    unsafe {
        crate::diagnostics::startup_log("windows-conpty: creating conpty");
        let mut conpty = Conpty::new()?;
        upsert_env_case_insensitive(&mut env_map, WINDOWS_CONPTY_REQUEST_ENV, "1");
        upsert_env_case_insensitive(&mut env_map, WINDOWS_CONPTY_ATTACHED_ENV, "1");
        crate::diagnostics::startup_log("windows-conpty: conpty created");
        let output_read = conpty.take_output_reader()?;
        let output_forwarder = spawn_conpty_output_forwarder(output_read);
        crate::diagnostics::startup_log("windows-conpty: spawning child");
        let proc_info = spawn_conpty_process(command, cwd, &env_map, conpty.hpc)?;
        conpty.close_child_side_handles();
        crate::diagnostics::startup_log("windows-conpty: child spawned");
        let _job_handle = JobHandle::kill_on_close()
            .ok()
            .and_then(|job| job.assign_process(proc_info.hProcess).ok().map(|()| job));

        let wait_status = WaitForSingleObject(proc_info.hProcess, INFINITE);
        if wait_status == WAIT_FAILED {
            CloseHandle(proc_info.hThread);
            CloseHandle(proc_info.hProcess);
            return Err(format!(
                "WaitForSingleObject failed: {}",
                std::io::Error::last_os_error()
            ));
        }

        let mut exit_code: u32 = 1;
        if GetExitCodeProcess(proc_info.hProcess, &mut exit_code) == 0 {
            CloseHandle(proc_info.hThread);
            CloseHandle(proc_info.hProcess);
            return Err(format!(
                "GetExitCodeProcess failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        CloseHandle(proc_info.hThread);
        CloseHandle(proc_info.hProcess);
        drop(conpty);
        let _ = output_forwarder.join();
        Ok(exit_code as i32)
    }
}

pub(crate) struct JobHandle(HANDLE);

impl JobHandle {
    pub(crate) unsafe fn kill_on_close() -> Result<Self, String> {
        let handle = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
        if handle.is_null() {
            return Err(format!(
                "CreateJobObjectW failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = SetInformationJobObject(
            handle,
            JobObjectExtendedLimitInformation,
            &mut limits as *mut _ as *mut _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if ok == 0 {
            let err = std::io::Error::last_os_error();
            CloseHandle(handle);
            return Err(format!("SetInformationJobObject failed: {err}"));
        }
        Ok(Self(handle))
    }

    pub(crate) fn raw(&self) -> HANDLE {
        self.0
    }

    pub(crate) unsafe fn assign_process(&self, process: HANDLE) -> Result<(), String> {
        if AssignProcessToJobObject(self.raw(), process) == 0 {
            return Err(format!(
                "AssignProcessToJobObject failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }
}

impl Drop for JobHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

pub unsafe fn spawn_conpty_process_as_user(
    token: HANDLE,
    command: &[String],
    cwd: &Path,
    env_map: &mut HashMap<String, String>,
) -> Result<(PROCESS_INFORMATION, Conpty, thread::JoinHandle<()>), String> {
    let mut conpty = Conpty::new()?;
    upsert_env_case_insensitive(env_map, WINDOWS_CONPTY_REQUEST_ENV, "1");
    upsert_env_case_insensitive(env_map, WINDOWS_CONPTY_ATTACHED_ENV, "1");
    let output_read = conpty.take_output_reader()?;
    let output_forwarder = spawn_conpty_output_forwarder(output_read);
    let proc_info = spawn_conpty_process_with_token(token, command, cwd, env_map, conpty.hpc)?;
    conpty.close_child_side_handles();
    Ok((proc_info, conpty, output_forwarder))
}

pub unsafe fn spawn_conpty_process_direct(
    command: &[String],
    cwd: Option<&Path>,
    env_map: &mut HashMap<String, String>,
) -> Result<(PROCESS_INFORMATION, Conpty), String> {
    let mut conpty = Conpty::new()?;
    upsert_env_case_insensitive(env_map, WINDOWS_CONPTY_REQUEST_ENV, "1");
    upsert_env_case_insensitive(env_map, WINDOWS_CONPTY_ATTACHED_ENV, "1");
    let proc_info = spawn_conpty_process(command, cwd, env_map, conpty.hpc)?;
    conpty.close_child_side_handles();
    Ok((proc_info, conpty))
}

pub struct Conpty {
    hpc: HPCON,
    input_read: Option<File>,
    input_write: Option<File>,
    output_read: Option<File>,
    output_write: Option<File>,
}

impl Conpty {
    unsafe fn new() -> Result<Self, String> {
        let mut input_read: HANDLE = std::ptr::null_mut();
        let mut input_write: HANDLE = std::ptr::null_mut();
        let mut output_read: HANDLE = std::ptr::null_mut();
        let mut output_write: HANDLE = std::ptr::null_mut();

        if CreatePipe(&mut input_read, &mut input_write, std::ptr::null_mut(), 0) == 0 {
            return Err(format!(
                "CreatePipe ConPTY input failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        if CreatePipe(&mut output_read, &mut output_write, std::ptr::null_mut(), 0) == 0 {
            CloseHandle(input_read);
            CloseHandle(input_write);
            return Err(format!(
                "CreatePipe ConPTY output failed: {}",
                std::io::Error::last_os_error()
            ));
        }

        let mut hpc: HPCON = INVALID_HANDLE_VALUE as HPCON;
        let hr = CreatePseudoConsole(
            COORD {
                X: CONPTY_COLS,
                Y: CONPTY_ROWS,
            },
            input_read,
            output_write,
            0,
            &mut hpc,
        );
        if hr < 0 {
            CloseHandle(input_read);
            CloseHandle(output_write);
            CloseHandle(input_write);
            CloseHandle(output_read);
            return Err(format!("CreatePseudoConsole failed: HRESULT {hr:#x}"));
        }
        if SetHandleInformation(input_write, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) == 0 {
            CloseHandle(input_write);
            CloseHandle(output_read);
            ClosePseudoConsole(hpc);
            return Err(format!(
                "SetHandleInformation failed for ConPTY input writer: {}",
                std::io::Error::last_os_error()
            ));
        }

        Ok(Self {
            hpc,
            input_read: Some(File::from_raw_handle(input_read as _)),
            input_write: Some(File::from_raw_handle(input_write as _)),
            output_read: Some(File::from_raw_handle(output_read as _)),
            output_write: Some(File::from_raw_handle(output_write as _)),
        })
    }

    pub fn take_input_writer(&mut self) -> Result<File, String> {
        self.input_write
            .take()
            .ok_or_else(|| "ConPTY input writer already taken".to_string())
    }

    pub fn take_output_reader(&mut self) -> Result<File, String> {
        self.output_read
            .take()
            .ok_or_else(|| "ConPTY output reader already taken".to_string())
    }

    fn close_child_side_handles(&mut self) {
        self.input_read.take();
        self.output_write.take();
    }
}

impl Drop for Conpty {
    fn drop(&mut self) {
        unsafe {
            ClosePseudoConsole(self.hpc);
        }
    }
}

unsafe fn spawn_conpty_process(
    command: &[String],
    cwd: Option<&Path>,
    env_map: &HashMap<String, String>,
    hpc: HPCON,
) -> Result<PROCESS_INFORMATION, String> {
    if command.is_empty() {
        return Err("no command specified to execute".to_string());
    }
    let mut cmdline = to_wide(
        command
            .iter()
            .map(|arg| quote_windows_arg(arg))
            .collect::<Vec<_>>()
            .join(" "),
    );
    let mut startup_info: STARTUPINFOEXW = std::mem::zeroed();
    startup_info.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    let mut attrs = ProcThreadAttributeList::new(1)?;
    attrs.set_conpty(hpc)?;
    startup_info.lpAttributeList = attrs.as_mut_ptr();

    let env_block = make_env_block(env_map);
    let cwd_wide = cwd.map(to_wide);
    let mut proc_info: PROCESS_INFORMATION = std::mem::zeroed();
    let ok = CreateProcessW(
        std::ptr::null(),
        cmdline.as_mut_ptr(),
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        0,
        EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_NEW_PROCESS_GROUP,
        env_block.as_ptr() as *const c_void,
        cwd_wide
            .as_ref()
            .map(|value| value.as_ptr())
            .unwrap_or(std::ptr::null()),
        &startup_info.StartupInfo,
        &mut proc_info,
    );
    if ok == 0 {
        return Err(format!(
            "CreateProcessW failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(proc_info)
}

unsafe fn spawn_conpty_process_with_token(
    token: HANDLE,
    command: &[String],
    cwd: &Path,
    env_map: &HashMap<String, String>,
    hpc: HPCON,
) -> Result<PROCESS_INFORMATION, String> {
    if command.is_empty() {
        return Err("no command specified to execute".to_string());
    }
    let mut cmdline = to_wide(
        command
            .iter()
            .map(|arg| quote_windows_arg(arg))
            .collect::<Vec<_>>()
            .join(" "),
    );
    let mut startup_info: STARTUPINFOEXW = std::mem::zeroed();
    startup_info.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    let mut attrs = ProcThreadAttributeList::new(1)?;
    attrs.set_conpty(hpc)?;
    startup_info.lpAttributeList = attrs.as_mut_ptr();

    let env_block = make_env_block(env_map);
    let cwd_wide = to_wide(cwd);
    let mut proc_info: PROCESS_INFORMATION = std::mem::zeroed();
    let ok = CreateProcessAsUserW(
        token,
        std::ptr::null(),
        cmdline.as_mut_ptr(),
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        0,
        EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_NEW_PROCESS_GROUP,
        env_block.as_ptr() as *const c_void,
        cwd_wide.as_ptr(),
        &startup_info.StartupInfo,
        &mut proc_info,
    );
    if ok == 0 {
        return Err(format!(
            "CreateProcessAsUserW failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(proc_info)
}

struct ProcThreadAttributeList {
    data: Vec<usize>,
}

impl ProcThreadAttributeList {
    unsafe fn new(count: u32) -> Result<Self, String> {
        let mut bytes_required = 0usize;
        let _ =
            InitializeProcThreadAttributeList(std::ptr::null_mut(), count, 0, &mut bytes_required);
        let word_count = bytes_required.div_ceil(std::mem::size_of::<usize>());
        let mut data = vec![0usize; word_count];
        if InitializeProcThreadAttributeList(
            data.as_mut_ptr().cast(),
            count,
            0,
            &mut bytes_required,
        ) == 0
        {
            return Err(format!(
                "InitializeProcThreadAttributeList failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Self { data })
    }

    fn as_mut_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.data.as_mut_ptr().cast()
    }

    unsafe fn set_conpty(&mut self, hpc: HPCON) -> Result<(), String> {
        let ok = UpdateProcThreadAttribute(
            self.as_mut_ptr(),
            0,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
            hpc as *const c_void,
            std::mem::size_of::<HPCON>(),
            std::ptr::null_mut(),
            std::ptr::null(),
        );
        if ok == 0 {
            return Err(format!(
                "UpdateProcThreadAttribute failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }
}

impl Drop for ProcThreadAttributeList {
    fn drop(&mut self) {
        unsafe {
            DeleteProcThreadAttributeList(self.as_mut_ptr());
        }
    }
}

fn spawn_conpty_output_forwarder(mut output: File) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut stdout = io::stdout();
        forward_conpty_output(&mut output, &mut stdout);
    })
}

fn forward_conpty_output<R, W>(output: &mut R, stdout: &mut W)
where
    R: Read,
    W: Write,
{
    let mut buffer = [0u8; 8192];
    loop {
        match output.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                if stdout
                    .write_all(&buffer[..count])
                    .and_then(|_| stdout.flush())
                    .is_err()
                {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

pub fn quote_windows_arg(arg: &str) -> String {
    let needs_quotes = arg.is_empty()
        || arg
            .chars()
            .any(|ch| matches!(ch, ' ' | '\t' | '\n' | '\r' | '"'));
    if !needs_quotes {
        return arg.to_string();
    }

    let mut quoted = String::with_capacity(arg.len() + 2);
    quoted.push('"');
    let mut backslashes = 0;
    for ch in arg.chars() {
        if ch == '\\' {
            backslashes += 1;
            continue;
        }
        if ch == '"' {
            quoted.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
            quoted.push('"');
            backslashes = 0;
            continue;
        }
        quoted.extend(std::iter::repeat_n('\\', backslashes));
        backslashes = 0;
        quoted.push(ch);
    }
    quoted.extend(std::iter::repeat_n('\\', backslashes * 2));
    quoted.push('"');
    quoted
}

pub fn make_env_block(env: &HashMap<String, String>) -> Vec<u16> {
    let mut items: Vec<(String, String)> = env
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    items.sort_by(|a, b| {
        a.0.to_uppercase()
            .cmp(&b.0.to_uppercase())
            .then(a.0.cmp(&b.0))
    });
    let mut wide = Vec::new();
    for (key, value) in items {
        let mut entry = to_wide(format!("{key}={value}"));
        entry.pop();
        wide.extend_from_slice(&entry);
        wide.push(0);
    }
    wide.push(0);
    wide
}

pub fn to_wide<S: AsRef<OsStr>>(value: S) -> Vec<u16> {
    let mut wide: Vec<u16> = value.as_ref().encode_wide().collect();
    wide.push(0);
    wide
}

pub fn env_get_case_insensitive<'a>(
    env_map: &'a HashMap<String, String>,
    key: &str,
) -> Option<&'a str> {
    env_map.get(key).map(String::as_str).or_else(|| {
        env_map.iter().find_map(|(candidate, value)| {
            if candidate.eq_ignore_ascii_case(key) {
                Some(value.as_str())
            } else {
                None
            }
        })
    })
}

pub fn upsert_env_case_insensitive(env_map: &mut HashMap<String, String>, key: &str, value: &str) {
    remove_env_case_insensitive(env_map, key);
    env_map.insert(key.to_string(), value.to_string());
}

pub fn remove_env_case_insensitive(env_map: &mut HashMap<String, String>, key: &str) {
    let removals: Vec<String> = env_map
        .keys()
        .filter(|existing| existing.eq_ignore_ascii_case(key))
        .cloned()
        .collect();
    for existing in removals {
        env_map.remove(&existing);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conpty_forwarder_preserves_escape_sequences_and_bel() {
        let input = b"plain\x1b[31mred\x1b[0m\x1b]0;title\x07bell\x07\n";
        let mut reader = std::io::Cursor::new(input);
        let mut output = Vec::new();

        forward_conpty_output(&mut reader, &mut output);

        assert_eq!(output, input);
    }
}
