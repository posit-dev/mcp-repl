#[cfg(target_family = "unix")]
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::{Mutex, MutexGuard, OnceLock};

use crate::output_capture::{OutputTextSpan, output_ring_test_mutex};
use crate::worker_protocol::{ContentOrigin, WorkerContent};
use crate::worker_supervisor::WorkerProcess;

pub(super) fn cwd_test_mutex() -> &'static Mutex<()> {
    static TEST_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
    TEST_MUTEX.get_or_init(|| Mutex::new(()))
}

#[cfg(target_family = "unix")]
pub(super) fn env_test_mutex() -> &'static Mutex<()> {
    static TEST_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
    TEST_MUTEX.get_or_init(|| Mutex::new(()))
}

pub(super) fn output_ring_test_guard() -> MutexGuard<'static, ()> {
    output_ring_test_mutex()
        .lock()
        .unwrap_or_else(|err| err.into_inner())
}

#[cfg(target_family = "unix")]
pub(super) fn worker_process_test_temp_parent(label: &str) -> PathBuf {
    let root = std::env::temp_dir()
        .join("mcp-repl-test-scratch")
        .join(label);
    std::fs::create_dir_all(&root).expect("create worker process test temp parent");
    root
}

pub(super) fn contents_text(contents: &[WorkerContent]) -> String {
    contents
        .iter()
        .filter_map(|content| match content {
            WorkerContent::ContentText { text, .. } => Some(text.as_str()),
            WorkerContent::ContentImage { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

pub(super) fn pager_buffer_from_worker_text(text: &str) -> crate::pager::PagerBuffer {
    pager_buffer_from_worker_text_with_source_end(text, text.len() as u64)
}

pub(super) fn static_pager_buffer_from_worker_text(text: &str) -> crate::pager::PagerBuffer {
    pager_buffer_from_worker_text_with_source_end(text, u64::MAX)
}

pub(super) fn pager_buffer_from_worker_text_with_source_end(
    text: &str,
    source_end: u64,
) -> crate::pager::PagerBuffer {
    crate::pager::PagerBuffer::from_bytes_and_events(
        text.as_bytes().to_vec(),
        Vec::new(),
        vec![OutputTextSpan {
            start_byte: 0,
            end_byte: text.len(),
            is_stderr: false,
            origin: ContentOrigin::Worker,
            source: crate::output_capture::OutputTextSource::Raw,
        }],
        source_end,
    )
}

#[cfg(target_family = "unix")]
pub(super) fn sleeping_test_child() -> Child {
    Command::new("sh")
        .args(["-c", "sleep 30"])
        .spawn()
        .expect("spawn sleeping test child")
}

#[cfg(target_family = "unix")]
pub(super) fn successful_test_child() -> Child {
    Command::new("sh")
        .args(["-c", "exit 0"])
        .spawn()
        .expect("spawn exiting test child")
}

#[cfg(target_family = "windows")]
pub(super) fn successful_test_child() -> Child {
    Command::new("powershell.exe")
        .args(["-NoProfile", "-Command", "exit 0"])
        .spawn()
        .expect("spawn exiting test child")
}

#[cfg(target_family = "unix")]
pub(super) fn failing_test_status() -> std::process::ExitStatus {
    Command::new("sh")
        .args(["-c", "exit 7"])
        .status()
        .expect("collect failing exit status")
}

pub(super) fn test_worker_process(child: Child) -> WorkerProcess {
    WorkerProcess::new_for_test(child)
}
