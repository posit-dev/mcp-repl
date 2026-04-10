#![cfg(target_os = "windows")]

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use common::TestResult;

#[test]
fn suite_server_lock_name_is_scoped_to_checkout() {
    let repo_a = PathBuf::from(r"C:\repo-a");
    let repo_b = PathBuf::from(r"D:\repo-b");

    let name_a = common::suite_server_lock_name_for_tests(&repo_a);
    let name_b = common::suite_server_lock_name_for_tests(&repo_b);

    assert_ne!(
        name_a, name_b,
        "distinct checkout roots should not share the same Windows suite lock name"
    );
}

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

#[test]
fn suite_server_lock_recovers_after_abandoned_owner() -> TestResult<()> {
    let (abandoned_tx, abandoned_rx) = mpsc::channel();
    let abandoner = thread::spawn(move || {
        let handle =
            common::acquire_suite_server_lock_handle_for_tests().expect("raw suite lock handle");
        abandoned_tx
            .send(handle)
            .expect("abandoned handle should be reported");
    });
    let abandoned_handle = abandoned_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("abandoner should acquire the suite lock");
    abandoner.join().expect("abandoner thread should join");

    let (acquired_tx, acquired_rx) = mpsc::channel();
    let waiter = thread::spawn(move || {
        let token = common::acquire_suite_server_lock_for_tests()
            .expect("suite lock should recover after an abandoned owner");
        acquired_tx
            .send(token)
            .expect("recovered token should be reported");
    });

    let token = match acquired_rx.recv_timeout(Duration::from_millis(250)) {
        Ok(token) => {
            common::close_suite_server_lock_handle_for_tests(abandoned_handle);
            token
        }
        Err(_) => {
            common::release_suite_server_lock_handle_for_tests(abandoned_handle);
            let token = acquired_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("cleanup should unblock the waiter");
            drop(token);
            waiter.join().expect("waiter thread should join");
            return Err("suite lock did not recover promptly after an abandoned owner".into());
        }
    };

    drop(token);
    waiter.join().expect("waiter thread should join");
    Ok(())
}
