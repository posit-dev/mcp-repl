#![cfg(target_os = "windows")]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use common::TestResult;

#[test]
fn suite_server_lock_allows_reentrant_acquire_within_process() -> TestResult<()> {
    let first = common::acquire_suite_server_lock_for_tests()?;
    let started = Arc::new(AtomicBool::new(false));
    let acquired = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();

    let thread_started = Arc::clone(&started);
    let thread_acquired = Arc::clone(&acquired);
    let waiter = thread::spawn(move || {
        thread_started.store(true, Ordering::SeqCst);
        let second = common::acquire_suite_server_lock_for_tests().expect("second lock");
        thread_acquired.store(true, Ordering::SeqCst);
        tx.send(()).expect("lock acquisition signal");
        drop(second);
    });

    while !started.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(5));
    }
    let reentrant = rx.recv_timeout(Duration::from_millis(200)).is_ok();

    drop(first);
    if !reentrant {
        rx.recv_timeout(Duration::from_secs(2))
            .expect("second lock should acquire after the first is released");
    }
    waiter.join().expect("waiter thread should join");
    assert!(
        acquired.load(Ordering::SeqCst) && reentrant,
        "same-process suite lock acquisition should not block behind an existing token"
    );
    Ok(())
}
