use std::ptr;
use std::sync::atomic::Ordering;

use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Storage::FileSystem::ReadFile;
use windows_sys::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE};
use windows_sys::Win32::System::Pipes::PeekNamedPipe;

use super::stdio::PYTHON_STDIN_FILE;

pub(super) fn discard_pending_stdin() {
    let stdin = PYTHON_STDIN_FILE.load(Ordering::SeqCst);
    if !stdin.is_null() {
        unsafe {
            libc::fflush(stdin);
        }
    }
    drain_stdin_pipe();
}

fn drain_stdin_pipe() {
    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return;
    }

    let mut buffer = [0u8; 8192];
    loop {
        let mut available = 0u32;
        let ok = unsafe {
            PeekNamedPipe(
                handle,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                &mut available,
                ptr::null_mut(),
            )
        };
        if ok == 0 || available == 0 {
            break;
        }

        let to_read = available.min(buffer.len() as u32);
        let mut read = 0u32;
        let ok = unsafe {
            ReadFile(
                handle,
                buffer.as_mut_ptr().cast(),
                to_read,
                &mut read,
                ptr::null_mut(),
            )
        };
        if ok == 0 || read == 0 {
            break;
        }
    }
}
