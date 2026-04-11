use std::cell::RefCell;
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

#[derive(Default)]
struct WindowsSandboxTestState {
    prepare_error: Option<String>,
    apply_acl_error: Option<String>,
    add_deny_write_ace_error: Option<(usize, String)>,
    allow_targets_read_dir_error: Option<(PathBuf, String)>,
    allow_targets_read_dir_calls: Vec<PathBuf>,
    deny_target_pre_apply_delete: Option<PathBuf>,
}

pub(crate) struct SandboxLaunchTestMutex {
    inner: Mutex<()>,
}

pub(crate) struct SandboxLaunchTestMutexGuard<'a> {
    _guard: std::sync::MutexGuard<'a, ()>,
}

fn canonicalize_or_identity(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn clear_windows_sandbox_test_state() {
    WINDOWS_SANDBOX_TEST_STATE.with(|slot| *slot.borrow_mut() = WindowsSandboxTestState::default());
}

impl SandboxLaunchTestMutex {
    fn new() -> Self {
        Self {
            inner: Mutex::new(()),
        }
    }

    pub(crate) fn lock(&self) -> Result<SandboxLaunchTestMutexGuard<'_>, Infallible> {
        let guard = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                self.inner.clear_poison();
                poisoned.into_inner()
            }
        };
        clear_windows_sandbox_test_state();
        Ok(SandboxLaunchTestMutexGuard { _guard: guard })
    }
}

impl Drop for SandboxLaunchTestMutexGuard<'_> {
    fn drop(&mut self) {
        clear_windows_sandbox_test_state();
    }
}

pub(crate) fn prepare_sandbox_launch_test_mutex() -> &'static SandboxLaunchTestMutex {
    static TEST_MUTEX: OnceLock<SandboxLaunchTestMutex> = OnceLock::new();
    TEST_MUTEX.get_or_init(SandboxLaunchTestMutex::new)
}

pub(crate) fn set_prepare_sandbox_launch_test_error(error: Option<String>) {
    WINDOWS_SANDBOX_TEST_STATE.with(|slot| slot.borrow_mut().prepare_error = error);
}

pub(crate) fn prepare_sandbox_launch_test_error() -> Option<String> {
    WINDOWS_SANDBOX_TEST_STATE.with(|slot| slot.borrow().prepare_error.clone())
}

pub(crate) fn set_apply_prepared_launch_acl_state_test_error(error: Option<String>) {
    WINDOWS_SANDBOX_TEST_STATE.with(|slot| slot.borrow_mut().apply_acl_error = error);
}

pub(crate) fn apply_prepared_launch_acl_state_test_error() -> Option<String> {
    WINDOWS_SANDBOX_TEST_STATE.with(|slot| slot.borrow().apply_acl_error.clone())
}

pub(crate) fn set_add_deny_write_ace_test_error(error: Option<(usize, String)>) {
    WINDOWS_SANDBOX_TEST_STATE.with(|slot| slot.borrow_mut().add_deny_write_ace_error = error);
}

pub(crate) fn next_add_deny_write_ace_test_result() -> Option<Result<(), String>> {
    WINDOWS_SANDBOX_TEST_STATE.with(|slot| {
        match slot.borrow_mut().add_deny_write_ace_error.as_mut() {
            Some((remaining_successes, error)) if *remaining_successes == 0 => {
                Some(Err(error.clone()))
            }
            Some((remaining_successes, _)) => {
                *remaining_successes -= 1;
                Some(Ok(()))
            }
            None => None,
        }
    })
}

pub(crate) fn set_prepared_launch_allow_targets_read_dir_test_error(
    error: Option<(PathBuf, String)>,
) {
    WINDOWS_SANDBOX_TEST_STATE.with(|slot| slot.borrow_mut().allow_targets_read_dir_error = error);
}

pub(crate) fn clear_prepared_launch_allow_targets_read_dir_calls() {
    WINDOWS_SANDBOX_TEST_STATE.with(|slot| slot.borrow_mut().allow_targets_read_dir_calls.clear());
}

pub(crate) fn take_prepared_launch_allow_targets_read_dir_calls() -> Vec<PathBuf> {
    WINDOWS_SANDBOX_TEST_STATE
        .with(|slot| std::mem::take(&mut slot.borrow_mut().allow_targets_read_dir_calls))
}

pub(crate) fn record_prepared_launch_allow_targets_read_dir_call(path: &Path) {
    WINDOWS_SANDBOX_TEST_STATE.with(|slot| {
        slot.borrow_mut()
            .allow_targets_read_dir_calls
            .push(canonicalize_or_identity(path));
    });
}

pub(crate) fn prepared_launch_allow_targets_read_dir_test_error(path: &Path) -> Option<String> {
    let path = canonicalize_or_identity(path);
    WINDOWS_SANDBOX_TEST_STATE.with(|slot| {
        let state = slot.borrow();
        state
            .allow_targets_read_dir_error
            .as_ref()
            .filter(|(expected_path, _)| canonicalize_or_identity(expected_path) == path)
            .map(|(_, error)| error.clone())
    })
}

pub(crate) fn set_prepared_launch_deny_target_pre_apply_delete(target: Option<PathBuf>) {
    WINDOWS_SANDBOX_TEST_STATE.with(|slot| slot.borrow_mut().deny_target_pre_apply_delete = target);
}

pub(crate) fn should_delete_prepared_launch_deny_target_before_apply(path: &Path) -> bool {
    let path = canonicalize_or_identity(path);
    WINDOWS_SANDBOX_TEST_STATE.with(|slot| {
        let mut state = slot.borrow_mut();
        if state
            .deny_target_pre_apply_delete
            .as_ref()
            .is_some_and(|expected| canonicalize_or_identity(expected) == path)
        {
            state.deny_target_pre_apply_delete = None;
            true
        } else {
            false
        }
    })
}

thread_local! {
    static WINDOWS_SANDBOX_TEST_STATE: RefCell<WindowsSandboxTestState> = const {
        RefCell::new(WindowsSandboxTestState {
            prepare_error: None,
            apply_acl_error: None,
            add_deny_write_ace_error: None,
            allow_targets_read_dir_error: None,
            allow_targets_read_dir_calls: Vec::new(),
            deny_target_pre_apply_delete: None,
        })
    };
}
