use std::process::Command;

use windows_sys::Win32::System::Console::{
    AttachConsole, CTRL_C_EVENT, FreeConsole, GenerateConsoleCtrlEvent, SetConsoleCtrlHandler,
};

const WINDOWS_SEND_CTRL_C_ARG: &str = "--windows-send-ctrl-c";

pub fn invoked_as_windows_ctrl_c_sender() -> bool {
    std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new(WINDOWS_SEND_CTRL_C_ARG))
}

pub fn run_windows_ctrl_c_sender_main() -> ! {
    match windows_ctrl_c_sender_main_impl() {
        Ok(()) => std::process::exit(0),
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(1);
        }
    }
}

fn windows_ctrl_c_sender_main_impl() -> Result<(), String> {
    let pid = std::env::args()
        .nth(2)
        .ok_or_else(|| format!("missing pid for {WINDOWS_SEND_CTRL_C_ARG}"))?
        .parse::<u32>()
        .map_err(|err| format!("invalid pid for {WINDOWS_SEND_CTRL_C_ARG}: {err}"))?;
    send_ctrl_c_to_process_console(pid)
}

pub fn spawn_ctrl_c_sender(pid: u32) -> Result<(), String> {
    let exe =
        std::env::current_exe().map_err(|err| format!("failed to resolve current exe: {err}"))?;
    let status = Command::new(exe)
        .arg(WINDOWS_SEND_CTRL_C_ARG)
        .arg(pid.to_string())
        .status()
        .map_err(|err| format!("failed to start Windows Ctrl-C sender: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("Windows Ctrl-C sender exited with status {status}"))
    }
}

pub fn send_ctrl_c_to_process_console(pid: u32) -> Result<(), String> {
    unsafe {
        let _ = FreeConsole();
        if AttachConsole(pid) == 0 {
            return Err(format!(
                "AttachConsole({pid}) failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        let _guard = AttachedConsoleGuard;
        if SetConsoleCtrlHandler(Some(ignore_ctrl_c_handler), 1) == 0 {
            return Err(format!(
                "SetConsoleCtrlHandler(ignore Ctrl-C) failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        let _ignore_guard = ConsoleCtrlIgnoreGuard;
        if GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0) == 0 {
            return Err(format!(
                "GenerateConsoleCtrlEvent(CTRL_C_EVENT) failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }
}

struct AttachedConsoleGuard;

impl Drop for AttachedConsoleGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = FreeConsole();
        }
    }
}

struct ConsoleCtrlIgnoreGuard;

impl Drop for ConsoleCtrlIgnoreGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = SetConsoleCtrlHandler(Some(ignore_ctrl_c_handler), 0);
        }
    }
}

unsafe extern "system" fn ignore_ctrl_c_handler(event: u32) -> i32 {
    if event == CTRL_C_EVENT { 1 } else { 0 }
}
