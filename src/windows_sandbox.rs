#![allow(unsafe_op_in_unsafe_fn)]

#[cfg(test)]
use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::io::Read;
use std::io::Write;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsRawHandle;
use std::os::windows::io::FromRawHandle;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(test)]
use std::sync::Mutex;
#[cfg(test)]
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::sandbox::{R_SESSION_TMPDIR_ENV, SandboxPolicy};
use windows_sys::Win32::Foundation::CloseHandle;
#[cfg(test)]
use windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE;
#[cfg(test)]
use windows_sys::Win32::Foundation::ERROR_NO_DATA;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::HLOCAL;
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Foundation::LUID;
use windows_sys::Win32::Foundation::SetHandleInformation;
use windows_sys::Win32::Foundation::WAIT_ABANDONED;
use windows_sys::Win32::Foundation::WAIT_FAILED;
use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
use windows_sys::Win32::Security::ACCESS_ALLOWED_ACE;
use windows_sys::Win32::Security::ACE_HEADER;
use windows_sys::Win32::Security::ACL;
use windows_sys::Win32::Security::AdjustTokenPrivileges;
use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
use windows_sys::Win32::Security::Authorization::EXPLICIT_ACCESS_W;
use windows_sys::Win32::Security::Authorization::GRANT_ACCESS;
use windows_sys::Win32::Security::Authorization::GetNamedSecurityInfoW;
use windows_sys::Win32::Security::Authorization::SE_FILE_OBJECT;
use windows_sys::Win32::Security::Authorization::SetEntriesInAclW;
use windows_sys::Win32::Security::Authorization::SetNamedSecurityInfoW;
use windows_sys::Win32::Security::Authorization::TRUSTEE_IS_SID;
use windows_sys::Win32::Security::Authorization::TRUSTEE_IS_UNKNOWN;
use windows_sys::Win32::Security::Authorization::TRUSTEE_W;
use windows_sys::Win32::Security::CopySid;
use windows_sys::Win32::Security::CreateRestrictedToken;
use windows_sys::Win32::Security::CreateWellKnownSid;
use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
use windows_sys::Win32::Security::DeleteAce;
use windows_sys::Win32::Security::EqualSid;
use windows_sys::Win32::Security::GetAce;
use windows_sys::Win32::Security::GetAclInformation;
use windows_sys::Win32::Security::GetLengthSid;
use windows_sys::Win32::Security::GetTokenInformation;
use windows_sys::Win32::Security::IsTokenRestricted;
use windows_sys::Win32::Security::LookupPrivilegeValueW;
use windows_sys::Win32::Security::SID_AND_ATTRIBUTES;
use windows_sys::Win32::Security::SetTokenInformation;
use windows_sys::Win32::Security::TOKEN_ADJUST_DEFAULT;
use windows_sys::Win32::Security::TOKEN_ADJUST_PRIVILEGES;
use windows_sys::Win32::Security::TOKEN_ADJUST_SESSIONID;
use windows_sys::Win32::Security::TOKEN_ASSIGN_PRIMARY;
use windows_sys::Win32::Security::TOKEN_DUPLICATE;
use windows_sys::Win32::Security::TOKEN_PRIVILEGES;
use windows_sys::Win32::Security::TOKEN_QUERY;
use windows_sys::Win32::Security::TokenDefaultDacl;
use windows_sys::Win32::Security::TokenGroups;
use windows_sys::Win32::Security::{ACCESS_DENIED_ACE, ACL_SIZE_INFORMATION, AclSizeInformation};
use windows_sys::Win32::Storage::FileSystem::DELETE;
use windows_sys::Win32::Storage::FileSystem::FILE_APPEND_DATA;
use windows_sys::Win32::Storage::FileSystem::FILE_DELETE_CHILD;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_EXECUTE;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE;
use windows_sys::Win32::Storage::FileSystem::FILE_WRITE_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::FILE_WRITE_DATA;
use windows_sys::Win32::Storage::FileSystem::FILE_WRITE_EA;
use windows_sys::Win32::System::Console::GetStdHandle;
use windows_sys::Win32::System::Console::STD_ERROR_HANDLE;
use windows_sys::Win32::System::Console::STD_INPUT_HANDLE;
use windows_sys::Win32::System::Console::STD_OUTPUT_HANDLE;
use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
use windows_sys::Win32::System::JobObjects::CreateJobObjectW;
use windows_sys::Win32::System::JobObjects::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
use windows_sys::Win32::System::JobObjects::JOBOBJECT_EXTENDED_LIMIT_INFORMATION;
use windows_sys::Win32::System::JobObjects::JobObjectExtendedLimitInformation;
use windows_sys::Win32::System::JobObjects::SetInformationJobObject;
use windows_sys::Win32::System::Pipes::CreatePipe;
#[cfg(test)]
use windows_sys::Win32::System::Pipes::PeekNamedPipe;
use windows_sys::Win32::System::Threading::CREATE_UNICODE_ENVIRONMENT;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::System::Threading::CreateProcessAsUserW;
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::Win32::System::Threading::GetExitCodeProcess;
use windows_sys::Win32::System::Threading::INFINITE;
use windows_sys::Win32::System::Threading::OpenProcessToken;
use windows_sys::Win32::System::Threading::PROCESS_INFORMATION;
use windows_sys::Win32::System::Threading::ReleaseMutex;
use windows_sys::Win32::System::Threading::STARTF_USESTDHANDLES;
use windows_sys::Win32::System::Threading::STARTUPINFOW;
use windows_sys::Win32::System::Threading::WaitForSingleObject;

const DISABLE_MAX_PRIVILEGE: u32 = 0x01;
const LUA_TOKEN: u32 = 0x04;
const WRITE_RESTRICTED: u32 = 0x08;
const GENERIC_ALL: u32 = 0x1000_0000;
const WIN_WORLD_SID: i32 = 1;
const SE_GROUP_LOGON_ID: u32 = 0xC0000000;
const HANDLE_FLAG_INHERIT: u32 = 0x00000001;
const DENY_ACCESS: i32 = 3;
const REVOKE_ACCESS: i32 = 4;
const CONTAINER_INHERIT_ACE: u32 = 0x2;
const OBJECT_INHERIT_ACE: u32 = 0x1;
const INHERIT_ONLY_ACE: u8 = 0x08;
const WRAPPER_STDIO_DRAIN_IDLE_TIMEOUT: Duration = Duration::from_secs(2);
const WRAPPER_STDIO_DRAIN_MAX_WAIT: Duration = Duration::from_secs(15);
const WRAPPER_STDIO_DRAIN_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Default)]
struct AllowDenyPaths {
    allow: HashSet<PathBuf>,
    deny: HashSet<PathBuf>,
}

#[derive(Clone, Copy)]
enum CapabilityAclKind {
    Allow,
    Deny,
}

struct CapabilityAclGuard {
    path: PathBuf,
    sid: *mut c_void,
    kind: CapabilityAclKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedSandboxLaunch {
    policy: SandboxPolicy,
    sandbox_policy_cwd: PathBuf,
    session_temp_dir: PathBuf,
    capability_sid: String,
}

impl PreparedSandboxLaunch {
    pub fn capability_sid(&self) -> &str {
        &self.capability_sid
    }

    pub fn matches(
        &self,
        policy: &SandboxPolicy,
        sandbox_policy_cwd: &Path,
        session_temp_dir: &Path,
    ) -> bool {
        let canonical_cwd = canonicalize_or_identity(sandbox_policy_cwd);
        self.policy == *policy
            && self.sandbox_policy_cwd == canonical_cwd
            && self.session_temp_dir == canonicalize_or_identity(session_temp_dir)
            && self.capability_sid == stable_cap_sid_string(policy, sandbox_policy_cwd)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LaunchCapabilitySids {
    filesystem_sid: String,
    launch_sid: String,
}

struct PreparedLaunchAclLock {
    handle: HANDLE,
}

struct WrapperChildStdio {
    stdin_write: File,
    stdout_read: File,
    stderr_read: File,
    child_stdin: File,
    child_stdout: File,
    child_stderr: File,
}

struct WrapperStdioForwarders {
    stdin_forwarder: thread::JoinHandle<()>,
    stdout_forwarder: thread::JoinHandle<()>,
    stderr_forwarder: thread::JoinHandle<()>,
    stdout_state: Arc<WrapperForwarderState>,
    stderr_state: Arc<WrapperForwarderState>,
}

struct WrapperForwarderState {
    bytes_copied: AtomicU64,
    done: AtomicBool,
    write_in_progress: AtomicBool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WrapperStdioMode {
    Inherit,
    ForwardedPipes,
}

impl WrapperForwarderState {
    fn new() -> Self {
        Self {
            bytes_copied: AtomicU64::new(0),
            done: AtomicBool::new(false),
            write_in_progress: AtomicBool::new(false),
        }
    }

    fn begin_write(&self) -> WrapperWriteGuard<'_> {
        self.write_in_progress.store(true, Ordering::Release);
        WrapperWriteGuard {
            write_in_progress: &self.write_in_progress,
        }
    }
}

fn wrapper_stdio_mode(prepared_capability_sid: Option<&str>) -> WrapperStdioMode {
    if prepared_capability_sid.is_some() {
        WrapperStdioMode::ForwardedPipes
    } else {
        WrapperStdioMode::Inherit
    }
}

struct WrapperWriteGuard<'a> {
    write_in_progress: &'a AtomicBool,
}

impl Drop for WrapperWriteGuard<'_> {
    fn drop(&mut self) {
        self.write_in_progress.store(false, Ordering::Release);
    }
}

impl Drop for PreparedLaunchAclLock {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.handle);
            let _ = CloseHandle(self.handle);
        }
    }
}

#[cfg(test)]
pub(crate) fn prepare_sandbox_launch_test_mutex() -> &'static Mutex<()> {
    static TEST_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
    TEST_MUTEX.get_or_init(|| Mutex::new(()))
}

#[cfg(test)]
pub(crate) fn set_prepare_sandbox_launch_test_error(error: Option<String>) {
    PREPARE_SANDBOX_LAUNCH_TEST_ERROR.with(|slot| *slot.borrow_mut() = error);
}

#[cfg(test)]
pub(crate) fn set_apply_prepared_launch_acl_state_test_error(error: Option<String>) {
    APPLY_PREPARED_LAUNCH_ACL_STATE_TEST_ERROR.with(|slot| *slot.borrow_mut() = error);
}

#[cfg(test)]
pub(crate) fn set_add_deny_write_ace_test_error(error: Option<(usize, String)>) {
    ADD_DENY_WRITE_ACE_TEST_ERROR.with(|slot| *slot.borrow_mut() = error);
}

#[cfg(test)]
thread_local! {
    static PREPARE_SANDBOX_LAUNCH_TEST_ERROR: RefCell<Option<String>> = const { RefCell::new(None) };
    static APPLY_PREPARED_LAUNCH_ACL_STATE_TEST_ERROR: RefCell<Option<String>> = const { RefCell::new(None) };
    static ADD_DENY_WRITE_ACE_TEST_ERROR: RefCell<Option<(usize, String)>> = const { RefCell::new(None) };
}

fn should_apply_network_block(policy: &SandboxPolicy) -> bool {
    !policy.has_full_network_access()
}

fn upsert_env_case_insensitive(env_map: &mut HashMap<String, String>, key: &str, value: &str) {
    let removals: Vec<String> = env_map
        .keys()
        .filter(|existing| existing.eq_ignore_ascii_case(key) && existing.as_str() != key)
        .cloned()
        .collect();
    for existing in removals {
        env_map.remove(&existing);
    }
    env_map.insert(key.to_string(), value.to_string());
}

fn apply_no_network_to_env(env_map: &mut HashMap<String, String>) {
    upsert_env_case_insensitive(env_map, "HTTP_PROXY", "http://127.0.0.1:9");
    upsert_env_case_insensitive(env_map, "HTTPS_PROXY", "http://127.0.0.1:9");
    upsert_env_case_insensitive(env_map, "ALL_PROXY", "http://127.0.0.1:9");
    upsert_env_case_insensitive(env_map, "NO_PROXY", "localhost,127.0.0.1,::1");
    upsert_env_case_insensitive(env_map, "CARGO_NET_OFFLINE", "true");
    upsert_env_case_insensitive(env_map, "NPM_CONFIG_OFFLINE", "true");
    upsert_env_case_insensitive(env_map, "PIP_NO_INDEX", "1");
}

fn env_get_case_insensitive<'a>(
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

fn canonicalize_or_identity(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn stable_sid_seed_path(path: &Path) -> String {
    stable_sid_seed_path_buf(canonicalize_or_identity(path))
}

fn stable_sid_seed_path_buf(path: PathBuf) -> String {
    #[cfg(target_os = "windows")]
    {
        let path = path.to_string_lossy();
        if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
            let mut stable = format!(r"\\{rest}");
            stable.make_ascii_lowercase();
            return stable;
        }
        if let Some(rest) = path.strip_prefix(r"\\?\") {
            let mut stable = rest.to_string();
            stable.make_ascii_lowercase();
            return stable;
        }
        let mut stable = path.into_owned();
        stable.make_ascii_lowercase();
        stable
    }
    #[cfg(not(target_os = "windows"))]
    {
        path.to_string_lossy().into_owned()
    }
}

fn compute_allow_deny_paths(
    policy: &SandboxPolicy,
    policy_cwd: &Path,
    command_cwd: &Path,
    session_temp_dir: Option<&Path>,
    env_map: &HashMap<String, String>,
) -> AllowDenyPaths {
    let mut allow = HashSet::new();
    let mut deny = HashSet::new();

    let include_tmp_env_vars = matches!(
        policy,
        SandboxPolicy::WorkspaceWrite {
            exclude_tmpdir_env_var: false,
            ..
        }
    );

    if let SandboxPolicy::WorkspaceWrite { writable_roots, .. } = policy {
        let mut add_writable_root = |root: PathBuf| {
            let candidate = if root.is_absolute() {
                root
            } else {
                policy_cwd.join(root)
            };
            let canonical = canonicalize_or_identity(&candidate);
            allow.insert(canonical.clone());
            let git_entry = canonical.join(".git");
            if git_entry.exists() {
                deny.insert(git_entry);
            }
            let codex_entry = canonical.join(".codex");
            if codex_entry.is_dir() {
                deny.insert(codex_entry);
            }
            let agents_entry = canonical.join(".agents");
            if agents_entry.is_dir() {
                deny.insert(agents_entry);
            }
        };
        add_writable_root(command_cwd.to_path_buf());
        for root in writable_roots {
            add_writable_root(root.clone());
        }
    }

    if let Some(path) = session_temp_dir
        && path.exists()
    {
        allow.insert(canonicalize_or_identity(path));
    }

    if include_tmp_env_vars {
        for key in ["TEMP", "TMP"] {
            if let Some(value) = env_get_case_insensitive(env_map, key) {
                let path = PathBuf::from(value);
                if path.exists() {
                    allow.insert(canonicalize_or_identity(&path));
                }
            } else if let Ok(value) = std::env::var(key) {
                let path = PathBuf::from(value);
                if path.exists() {
                    allow.insert(canonicalize_or_identity(&path));
                }
            }
        }
    }

    AllowDenyPaths { allow, deny }
}

unsafe fn convert_string_sid_to_sid(value: &str) -> Option<*mut c_void> {
    let mut sid: *mut c_void = std::ptr::null_mut();
    let ok = ConvertStringSidToSidW(to_wide(value).as_ptr(), &mut sid);
    if ok != 0 { Some(sid) } else { None }
}

fn stable_cap_sid_string(policy: &SandboxPolicy, sandbox_policy_cwd: &Path) -> String {
    let canonical_cwd = canonicalize_or_identity(sandbox_policy_cwd);
    let stable_cwd = stable_sid_seed_path_buf(canonical_cwd.clone());
    let policy_seed = match policy {
        SandboxPolicy::ReadOnly => serde_json::json!({
            "mode": "read-only",
        }),
        SandboxPolicy::WorkspaceWrite {
            writable_roots,
            network_access,
            exclude_tmpdir_env_var,
            exclude_slash_tmp,
        } => {
            let mut canonical_roots = writable_roots
                .iter()
                .map(|root| stable_sid_seed_path(&canonical_cwd.join(root)))
                .collect::<Vec<_>>();
            canonical_roots.sort();
            canonical_roots.dedup();
            serde_json::json!({
                "mode": "workspace-write",
                "writable_roots": canonical_roots,
                "network_access": network_access,
                "exclude_tmpdir_env_var": exclude_tmpdir_env_var,
                "exclude_slash_tmp": exclude_slash_tmp,
            })
        }
        SandboxPolicy::DangerFullAccess | SandboxPolicy::ExternalSandbox { .. } => {
            serde_json::json!({
                "mode": "unsupported",
            })
        }
    };
    let seed = format!(
        "mcp-repl-windows-sandbox-v2\0{}\0{}",
        stable_cwd, policy_seed,
    );
    let a = stable_sid_word(seed.as_bytes(), 0x243f_6a88);
    let b = stable_sid_word(seed.as_bytes(), 0x85a3_08d3);
    let c = stable_sid_word(seed.as_bytes(), 0x1319_8a2e);
    let d = stable_sid_word(seed.as_bytes(), 0x0370_7344);
    format!("S-1-5-21-{a}-{b}-{c}-{d}")
}

fn make_random_sid_string() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();
    let a = (nanos as u32) ^ pid;
    let b = ((nanos >> 32) as u32).wrapping_add(pid.rotate_left(7));
    let c = ((nanos >> 64) as u32).wrapping_add(pid.rotate_left(13));
    let d = ((nanos >> 96) as u32).wrapping_add(pid.rotate_left(19));
    format!("S-1-5-21-{a}-{b}-{c}-{d}")
}

fn resolve_launch_capability_sids(
    policy: &SandboxPolicy,
    sandbox_policy_cwd: &Path,
    prepared_capability_sid: Option<&str>,
) -> Result<LaunchCapabilitySids, String> {
    match prepared_capability_sid {
        Some(value) => {
            let expected_filesystem_sid = stable_cap_sid_string(policy, sandbox_policy_cwd);
            if value != expected_filesystem_sid {
                return Err(
                    "prepared capability SID did not match expected workspace identity".to_string(),
                );
            }
            Ok(LaunchCapabilitySids {
                filesystem_sid: value.to_string(),
                launch_sid: make_random_sid_string(),
            })
        }
        None => {
            let sid = make_random_sid_string();
            Ok(LaunchCapabilitySids {
                filesystem_sid: sid.clone(),
                launch_sid: sid,
            })
        }
    }
}

fn default_dacl_capability_sid_strings(capability_sids: &LaunchCapabilitySids) -> Vec<&str> {
    if capability_sids.launch_sid == capability_sids.filesystem_sid {
        vec![capability_sids.filesystem_sid.as_str()]
    } else {
        vec![capability_sids.launch_sid.as_str()]
    }
}

fn prepared_launch_acl_lock_name(capability_sid: &str) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in capability_sid.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("Local\\mcp_repl_prepared_launch_acl_{hash:016x}")
}

fn acquire_prepared_launch_acl_lock(capability_sid: &str) -> Result<PreparedLaunchAclLock, String> {
    unsafe {
        let name = to_wide(prepared_launch_acl_lock_name(capability_sid));
        let handle = CreateMutexW(std::ptr::null_mut(), 0, name.as_ptr());
        if handle.is_null() {
            return Err(format!(
                "CreateMutexW failed for prepared launch ACL lock: {}",
                std::io::Error::last_os_error()
            ));
        }

        let wait = WaitForSingleObject(handle, INFINITE);
        if wait != WAIT_OBJECT_0 && wait != WAIT_ABANDONED {
            CloseHandle(handle);
            return Err(format!(
                "WaitForSingleObject failed for prepared launch ACL lock: {wait}"
            ));
        }

        Ok(PreparedLaunchAclLock { handle })
    }
}

fn build_prepared_sandbox_launch(
    policy: &SandboxPolicy,
    sandbox_policy_cwd: &Path,
    session_temp_dir: &Path,
    capability_sid: &str,
) -> PreparedSandboxLaunch {
    PreparedSandboxLaunch {
        policy: policy.clone(),
        sandbox_policy_cwd: canonicalize_or_identity(sandbox_policy_cwd),
        session_temp_dir: canonicalize_or_identity(session_temp_dir),
        capability_sid: capability_sid.to_string(),
    }
}

fn normalize_prepared_capability_sid(
    prepared_capability_sid: Option<&str>,
    base_token_restricted: bool,
) -> Option<&str> {
    if prepared_capability_sid.is_some() && base_token_restricted {
        None
    } else {
        prepared_capability_sid
    }
}

unsafe fn effective_prepared_capability_sid(
    base_token: HANDLE,
    prepared_capability_sid: Option<&str>,
) -> Option<&str> {
    let normalized = normalize_prepared_capability_sid(
        prepared_capability_sid,
        IsTokenRestricted(base_token) != 0,
    );
    if prepared_capability_sid.is_some() && normalized.is_none() {
        crate::diagnostics::startup_log(
            "windows-sandbox: prepared capability SID disabled because base token is already restricted",
        );
    }
    normalized
}

fn stable_sid_word(bytes: &[u8], seed: u32) -> u32 {
    let mut hash = 2_166_136_261u32 ^ seed;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(16_777_619);
    }
    hash.max(1)
}

fn validate_windows_policy(policy: &SandboxPolicy) -> Result<(), String> {
    match policy {
        SandboxPolicy::ReadOnly | SandboxPolicy::WorkspaceWrite { .. } => Ok(()),
        SandboxPolicy::DangerFullAccess | SandboxPolicy::ExternalSandbox { .. } => {
            Err("windows sandbox runner only supports read-only/workspace-write".to_string())
        }
    }
}

fn sandbox_acl_env_map(session_temp_dir: &Path) -> HashMap<String, String> {
    let temp_dir = session_temp_dir.to_string_lossy().to_string();
    HashMap::from([
        ("TEMP".to_string(), temp_dir.clone()),
        ("TMP".to_string(), temp_dir.clone()),
        (R_SESSION_TMPDIR_ENV.to_string(), temp_dir),
    ])
}

fn prepared_launch_acl_paths(launch: &PreparedSandboxLaunch) -> AllowDenyPaths {
    let env_map = sandbox_acl_env_map(&launch.session_temp_dir);
    let mut paths = compute_allow_deny_paths(
        &launch.policy,
        &launch.sandbox_policy_cwd,
        &launch.sandbox_policy_cwd,
        Some(&launch.session_temp_dir),
        &env_map,
    );
    paths.allow.remove(&launch.session_temp_dir);
    paths
}

unsafe fn apply_prepared_launch_acl_state(
    launch: &PreparedSandboxLaunch,
    sid: *mut c_void,
    action: &str,
) -> Result<(), String> {
    #[cfg(test)]
    if let Some(error) =
        APPLY_PREPARED_LAUNCH_ACL_STATE_TEST_ERROR.with(|slot| slot.borrow().clone())
    {
        return Err(error);
    }

    let paths = prepared_launch_acl_paths(launch);
    let mut acl_guards: Vec<CapabilityAclGuard> = Vec::new();
    let denied_roots = canonicalized_paths(&paths.deny);

    for path in &paths.allow {
        for target in prepared_launch_allow_targets(path, &denied_roots)? {
            match add_allow_ace(&target, sid) {
                Ok(true) => acl_guards.push(CapabilityAclGuard {
                    path: target,
                    sid,
                    kind: CapabilityAclKind::Allow,
                }),
                Ok(false) => {}
                Err(err) => {
                    cleanup_capability_acl_state(&acl_guards, sid, false);
                    return Err(format!(
                        "failed to {action} writable ACL on '{}': {err}",
                        target.display()
                    ));
                }
            }
        }
    }

    if matches!(launch.policy, SandboxPolicy::WorkspaceWrite { .. }) {
        for path in &paths.deny {
            match add_deny_write_ace(path, sid) {
                Ok(true) => acl_guards.push(CapabilityAclGuard {
                    path: path.clone(),
                    sid,
                    kind: CapabilityAclKind::Deny,
                }),
                Ok(false) => {}
                Err(err) => {
                    cleanup_capability_acl_state(&acl_guards, sid, false);
                    return Err(format!(
                        "failed to {action} deny ACL on '{}': {err}",
                        path.display()
                    ));
                }
            }
        }
    }

    Ok(())
}

fn canonicalized_paths(paths: &HashSet<PathBuf>) -> Vec<PathBuf> {
    let mut canonicalized = paths
        .iter()
        .map(|path| canonicalize_or_identity(path))
        .collect::<Vec<_>>();
    canonicalized.sort();
    canonicalized.dedup();
    canonicalized
}

fn path_is_within_any_root(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

fn prepared_launch_allow_targets(
    root: &Path,
    denied_roots: &[PathBuf],
) -> Result<Vec<PathBuf>, String> {
    let mut targets = Vec::new();
    let root_canonical = canonicalize_or_identity(root);
    let mut stack = vec![root.to_path_buf()];
    let mut visited = HashSet::new();

    while let Some(path) = stack.pop() {
        let canonical = canonicalize_or_identity(&path);
        if !canonical.starts_with(&root_canonical)
            || path_is_within_any_root(&canonical, denied_roots)
        {
            continue;
        }
        if !visited.insert(canonical) {
            continue;
        }

        targets.push(path.clone());

        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound && path == root => {
                // Let add_allow_ace() materialize a declared writable root that does not exist yet.
                continue;
            }
            Err(err) => {
                return Err(format!(
                    "symlink_metadata failed for '{}': {err}",
                    path.display()
                ));
            }
        };
        if !metadata.is_dir() {
            continue;
        }

        let entries = std::fs::read_dir(&path)
            .map_err(|err| format!("read_dir failed for '{}': {err}", path.display()))?;
        for entry in entries {
            let entry = entry
                .map_err(|err| format!("read_dir entry failed for '{}': {err}", path.display()))?;
            stack.push(entry.path());
        }
    }

    Ok(targets)
}

unsafe fn refresh_runtime_prepared_launch_acl_state(
    policy: &SandboxPolicy,
    sandbox_policy_cwd: &Path,
    session_temp_dir: &Path,
    prepared_capability_sid: &str,
    sid: *mut c_void,
) -> Result<PreparedSandboxLaunch, String> {
    let launch = build_prepared_sandbox_launch(
        policy,
        sandbox_policy_cwd,
        session_temp_dir,
        prepared_capability_sid,
    );
    let _acl_lock = acquire_prepared_launch_acl_lock(prepared_capability_sid)?;
    apply_prepared_launch_acl_state(&launch, sid, "refresh")?;
    Ok(launch)
}

pub fn prepare_sandbox_launch(
    policy: &SandboxPolicy,
    sandbox_policy_cwd: &Path,
    session_temp_dir: &Path,
) -> Result<PreparedSandboxLaunch, String> {
    #[cfg(test)]
    if let Some(error) = PREPARE_SANDBOX_LAUNCH_TEST_ERROR.with(|slot| slot.borrow().clone()) {
        return Err(error);
    }

    validate_windows_policy(policy)?;

    let cap_sid = stable_cap_sid_string(policy, sandbox_policy_cwd);
    let launch =
        build_prepared_sandbox_launch(policy, sandbox_policy_cwd, session_temp_dir, &cap_sid);
    let _acl_lock = acquire_prepared_launch_acl_lock(launch.capability_sid())?;

    unsafe {
        let psid_capability = convert_string_sid_to_sid(launch.capability_sid())
            .ok_or_else(|| "ConvertStringSidToSidW failed for capability SID".to_string())?;
        let apply_result = apply_prepared_launch_acl_state(&launch, psid_capability, "prepare");
        LocalFree(psid_capability as HLOCAL);
        apply_result?;
    }

    Ok(launch)
}

pub fn refresh_prepared_sandbox_launch_acl_state(
    launch: &PreparedSandboxLaunch,
) -> Result<(), String> {
    let _acl_lock = acquire_prepared_launch_acl_lock(launch.capability_sid())?;
    unsafe {
        let psid_capability = convert_string_sid_to_sid(launch.capability_sid())
            .ok_or_else(|| "ConvertStringSidToSidW failed for capability SID".to_string())?;
        let refresh_result = apply_prepared_launch_acl_state(launch, psid_capability, "refresh");
        LocalFree(psid_capability as HLOCAL);
        refresh_result
    }
}

#[repr(C)]
struct TokenDefaultDaclInfo {
    default_dacl: *mut ACL,
}

pub fn run_sandboxed_command(
    policy: &SandboxPolicy,
    sandbox_policy_cwd: &Path,
    command: &[String],
    prepared_capability_sid: Option<&str>,
) -> Result<i32, String> {
    run_sandboxed_command_with_env_map(
        policy,
        sandbox_policy_cwd,
        command,
        prepared_capability_sid,
        std::env::vars().collect(),
    )
}

fn run_sandboxed_command_with_env_map(
    policy: &SandboxPolicy,
    sandbox_policy_cwd: &Path,
    command: &[String],
    prepared_capability_sid: Option<&str>,
    mut env_map: HashMap<String, String>,
) -> Result<i32, String> {
    if command.is_empty() {
        return Err("no command specified to execute".to_string());
    }

    validate_windows_policy(policy)?;

    unsafe {
        crate::diagnostics::startup_log("windows-sandbox: begin");
        if should_apply_network_block(policy) {
            apply_no_network_to_env(&mut env_map);
        }
        let session_temp_dir =
            env_get_case_insensitive(&env_map, R_SESSION_TMPDIR_ENV).map(PathBuf::from);

        let base_token = get_current_token_for_restriction()?;
        let mut prepared_capability_sid =
            effective_prepared_capability_sid(base_token, prepared_capability_sid);
        if prepared_capability_sid.is_some() && session_temp_dir.is_none() {
            crate::diagnostics::startup_log(
                "windows-sandbox: prepared capability SID disabled because session temp dir is missing",
            );
            prepared_capability_sid = None;
        }
        let capability_sids =
            resolve_launch_capability_sids(policy, sandbox_policy_cwd, prepared_capability_sid)?;
        let psid_capability = convert_string_sid_to_sid(&capability_sids.filesystem_sid)
            .ok_or_else(|| "ConvertStringSidToSidW failed for filesystem SID".to_string())?;
        let launch_sid_is_distinct = capability_sids.launch_sid != capability_sids.filesystem_sid;
        let psid_launch = if launch_sid_is_distinct {
            convert_string_sid_to_sid(&capability_sids.launch_sid)
                .ok_or_else(|| "ConvertStringSidToSidW failed for launch SID".to_string())?
        } else {
            psid_capability
        };
        let mut restricted_capability_sids = vec![psid_capability];
        if launch_sid_is_distinct {
            restricted_capability_sids.push(psid_launch);
        }
        let mut default_dacl_capability_sids: Vec<*mut c_void> = Vec::new();
        for sid in default_dacl_capability_sid_strings(&capability_sids) {
            if sid == capability_sids.filesystem_sid.as_str() {
                default_dacl_capability_sids.push(psid_capability);
                continue;
            }
            if sid == capability_sids.launch_sid.as_str() {
                default_dacl_capability_sids.push(psid_launch);
            }
        }
        let token_result = create_restricted_token_for_policy(
            base_token,
            &restricted_capability_sids,
            &default_dacl_capability_sids,
        );
        CloseHandle(base_token);
        let restricted_token = match token_result {
            Ok(token) => token,
            Err(err) => {
                if launch_sid_is_distinct {
                    LocalFree(psid_launch as HLOCAL);
                }
                LocalFree(psid_capability as HLOCAL);
                return Err(err);
            }
        };

        let null_device_ace_applied = allow_null_device(psid_launch);

        let mut acl_guards: Vec<CapabilityAclGuard> = Vec::new();
        if let Some(prepared_capability_sid) = prepared_capability_sid {
            let refresh_result = refresh_runtime_prepared_launch_acl_state(
                policy,
                sandbox_policy_cwd,
                session_temp_dir
                    .as_deref()
                    .expect("prepared capability SID requires session temp dir"),
                prepared_capability_sid,
                psid_capability,
            );
            if let Err(err) = refresh_result {
                cleanup_capability_acl_state(&acl_guards, psid_launch, null_device_ace_applied);
                CloseHandle(restricted_token);
                if launch_sid_is_distinct {
                    LocalFree(psid_launch as HLOCAL);
                }
                LocalFree(psid_capability as HLOCAL);
                return Err(err);
            }
            if let Some(session_temp_dir) = session_temp_dir.as_deref() {
                match add_allow_ace(session_temp_dir, psid_launch) {
                    Ok(true) => acl_guards.push(CapabilityAclGuard {
                        path: session_temp_dir.to_path_buf(),
                        sid: psid_launch,
                        kind: CapabilityAclKind::Allow,
                    }),
                    Ok(false) => {}
                    Err(err) => {
                        cleanup_capability_acl_state(
                            &acl_guards,
                            psid_launch,
                            null_device_ace_applied,
                        );
                        CloseHandle(restricted_token);
                        if launch_sid_is_distinct {
                            LocalFree(psid_launch as HLOCAL);
                        }
                        LocalFree(psid_capability as HLOCAL);
                        return Err(format!(
                            "failed to apply session temp dir ACL to '{}': {err}",
                            session_temp_dir.display()
                        ));
                    }
                }
            }
        } else {
            let paths = compute_allow_deny_paths(
                policy,
                sandbox_policy_cwd,
                sandbox_policy_cwd,
                session_temp_dir.as_deref(),
                &env_map,
            );
            crate::diagnostics::startup_log(format!(
                "windows-sandbox: acl plan allow={} deny={}",
                paths.allow.len(),
                paths.deny.len()
            ));
            for path in &paths.allow {
                match add_allow_ace(path, psid_capability) {
                    Ok(true) => acl_guards.push(CapabilityAclGuard {
                        path: path.clone(),
                        sid: psid_capability,
                        kind: CapabilityAclKind::Allow,
                    }),
                    Ok(false) => {}
                    Err(err) => {
                        cleanup_capability_acl_state(
                            &acl_guards,
                            psid_launch,
                            null_device_ace_applied,
                        );
                        CloseHandle(restricted_token);
                        if launch_sid_is_distinct {
                            LocalFree(psid_launch as HLOCAL);
                        }
                        LocalFree(psid_capability as HLOCAL);
                        return Err(format!(
                            "failed to apply writable ACL to '{}': {err}",
                            path.display()
                        ));
                    }
                }
            }
            crate::diagnostics::startup_log("windows-sandbox: allow ACLs applied");
            if matches!(policy, SandboxPolicy::WorkspaceWrite { .. }) {
                for path in &paths.deny {
                    match add_deny_write_ace(path, psid_capability) {
                        Ok(true) => acl_guards.push(CapabilityAclGuard {
                            path: path.clone(),
                            sid: psid_capability,
                            kind: CapabilityAclKind::Deny,
                        }),
                        Ok(false) => {}
                        Err(err) => {
                            cleanup_capability_acl_state(
                                &acl_guards,
                                psid_launch,
                                null_device_ace_applied,
                            );
                            CloseHandle(restricted_token);
                            if launch_sid_is_distinct {
                                LocalFree(psid_launch as HLOCAL);
                            }
                            LocalFree(psid_capability as HLOCAL);
                            return Err(format!(
                                "failed to apply deny ACL to '{}': {err}",
                                path.display()
                            ));
                        }
                    }
                }
                crate::diagnostics::startup_log("windows-sandbox: deny ACLs applied");
            }
        }

        let stdio_mode = wrapper_stdio_mode(prepared_capability_sid);
        let stdio_pipes = match stdio_mode {
            WrapperStdioMode::Inherit => None,
            WrapperStdioMode::ForwardedPipes => {
                let pipes = match create_wrapper_child_stdio() {
                    Ok(pipes) => pipes,
                    Err(err) => {
                        cleanup_capability_acl_state(
                            &acl_guards,
                            psid_launch,
                            null_device_ace_applied,
                        );
                        CloseHandle(restricted_token);
                        if launch_sid_is_distinct {
                            LocalFree(psid_launch as HLOCAL);
                        }
                        LocalFree(psid_capability as HLOCAL);
                        return Err(err);
                    }
                };
                crate::diagnostics::startup_log("windows-sandbox: stdio pipes created");
                Some(pipes)
            }
        };
        let spawn_result = create_process_as_user(
            restricted_token,
            command,
            sandbox_policy_cwd,
            &env_map,
            stdio_pipes.as_ref().map(|pipes| {
                (
                    pipes.child_stdin.as_raw_handle() as HANDLE,
                    pipes.child_stdout.as_raw_handle() as HANDLE,
                    pipes.child_stderr.as_raw_handle() as HANDLE,
                )
            }),
        );
        let (proc_info, _startup_info) = match spawn_result {
            Ok(value) => value,
            Err(err) => {
                drop(stdio_pipes);
                cleanup_capability_acl_state(&acl_guards, psid_launch, null_device_ace_applied);
                CloseHandle(restricted_token);
                if launch_sid_is_distinct {
                    LocalFree(psid_launch as HLOCAL);
                }
                LocalFree(psid_capability as HLOCAL);
                return Err(err);
            }
        };
        crate::diagnostics::startup_log("windows-sandbox: child spawned");
        let stdio_forwarders = stdio_pipes.map(spawn_wrapper_stdio_forwarders);

        let job_handle = create_job_kill_on_close().ok();
        if let Some(job) = job_handle {
            let _ = AssignProcessToJobObject(job, proc_info.hProcess);
        }

        let wait_status = WaitForSingleObject(proc_info.hProcess, INFINITE);
        if wait_status == WAIT_FAILED {
            if let Some(job) = job_handle {
                CloseHandle(job);
            }
            cleanup_capability_acl_state(&acl_guards, psid_launch, null_device_ace_applied);
            CloseHandle(proc_info.hThread);
            CloseHandle(proc_info.hProcess);
            CloseHandle(restricted_token);
            if launch_sid_is_distinct {
                LocalFree(psid_launch as HLOCAL);
            }
            LocalFree(psid_capability as HLOCAL);
            return Err(format!(
                "WaitForSingleObject failed: {}",
                std::io::Error::last_os_error()
            ));
        }

        let mut exit_code: u32 = 1;
        if GetExitCodeProcess(proc_info.hProcess, &mut exit_code) == 0 {
            if let Some(job) = job_handle {
                CloseHandle(job);
            }
            if let Some(WrapperStdioForwarders {
                stdin_forwarder,
                stdout_forwarder,
                stderr_forwarder,
                ..
            }) = stdio_forwarders
            {
                drop(stdin_forwarder);
                drop(stdout_forwarder);
                drop(stderr_forwarder);
            }
            cleanup_capability_acl_state(&acl_guards, psid_launch, null_device_ace_applied);
            CloseHandle(proc_info.hThread);
            CloseHandle(proc_info.hProcess);
            CloseHandle(restricted_token);
            if launch_sid_is_distinct {
                LocalFree(psid_launch as HLOCAL);
            }
            LocalFree(psid_capability as HLOCAL);
            return Err(format!(
                "GetExitCodeProcess failed: {}",
                std::io::Error::last_os_error()
            ));
        }

        if let Some(job) = job_handle {
            CloseHandle(job);
        }
        if let Some(WrapperStdioForwarders {
            stdin_forwarder,
            stdout_forwarder,
            stderr_forwarder,
            stdout_state,
            stderr_state,
        }) = stdio_forwarders
        {
            drop(stdin_forwarder);
            drain_wrapper_forwarder(stdout_forwarder, &stdout_state);
            drain_wrapper_forwarder(stderr_forwarder, &stderr_state);
        }
        cleanup_capability_acl_state(&acl_guards, psid_launch, null_device_ace_applied);
        CloseHandle(proc_info.hThread);
        CloseHandle(proc_info.hProcess);
        CloseHandle(restricted_token);
        if launch_sid_is_distinct {
            LocalFree(psid_launch as HLOCAL);
        }
        LocalFree(psid_capability as HLOCAL);

        Ok(exit_code as i32)
    }
}

unsafe fn cleanup_capability_acl_state(
    acl_guards: &[CapabilityAclGuard],
    capability_sid: *mut c_void,
    null_device_ace_applied: bool,
) {
    for guard in acl_guards {
        match guard.kind {
            CapabilityAclKind::Allow => revoke_ace(&guard.path, guard.sid),
            CapabilityAclKind::Deny => revoke_deny_write_ace(&guard.path, guard.sid),
        }
    }
    if null_device_ace_applied {
        revoke_null_device_ace(capability_sid);
    }
}

unsafe fn create_job_kill_on_close() -> Result<HANDLE, String> {
    let h = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
    if h.is_null() {
        return Err("CreateJobObjectW failed".to_string());
    }
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let ok = SetInformationJobObject(
        h,
        JobObjectExtendedLimitInformation,
        &mut limits as *mut _ as *mut _,
        std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
    );
    if ok == 0 {
        CloseHandle(h);
        return Err("SetInformationJobObject failed".to_string());
    }
    Ok(h)
}

unsafe fn create_restricted_token_for_policy(
    base_token: HANDLE,
    capability_sids: &[*mut c_void],
    default_dacl_capability_sids: &[*mut c_void],
) -> Result<HANDLE, String> {
    if capability_sids.is_empty() {
        return Err("no capability SIDs provided".to_string());
    }
    let mut logon_sid_bytes = get_logon_sid_bytes(base_token)?;
    let psid_logon = logon_sid_bytes.as_mut_ptr() as *mut c_void;
    let mut everyone = world_sid()?;
    let psid_everyone = everyone.as_mut_ptr() as *mut c_void;

    let mut restricted_sids: Vec<SID_AND_ATTRIBUTES> =
        Vec::with_capacity(capability_sids.len() + 2);
    for sid in capability_sids {
        restricted_sids.push(SID_AND_ATTRIBUTES {
            Sid: *sid,
            Attributes: 0,
        });
    }
    restricted_sids.push(SID_AND_ATTRIBUTES {
        Sid: psid_logon,
        Attributes: 0,
    });
    restricted_sids.push(SID_AND_ATTRIBUTES {
        Sid: psid_everyone,
        Attributes: 0,
    });

    let mut new_token: HANDLE = std::ptr::null_mut();
    let sid_count = restricted_sids.len() as u32;
    let sid_ptr = if restricted_sids.is_empty() {
        std::ptr::null_mut()
    } else {
        restricted_sids.as_mut_ptr()
    };
    let ok = CreateRestrictedToken(
        base_token,
        DISABLE_MAX_PRIVILEGE | LUA_TOKEN | WRITE_RESTRICTED,
        0,
        std::ptr::null(),
        0,
        std::ptr::null(),
        sid_count,
        sid_ptr,
        &mut new_token,
    );
    if ok == 0 {
        return Err(format!("CreateRestrictedToken failed: {}", GetLastError()));
    }

    let mut dacl_sids = Vec::with_capacity(default_dacl_capability_sids.len() + 2);
    dacl_sids.push(psid_logon);
    dacl_sids.push(psid_everyone);
    dacl_sids.extend_from_slice(default_dacl_capability_sids);
    set_default_dacl(new_token, &dacl_sids)?;
    enable_single_privilege(new_token, "SeChangeNotifyPrivilege")?;
    Ok(new_token)
}

unsafe fn create_process_as_user(
    token: HANDLE,
    argv: &[String],
    cwd: &Path,
    env_map: &HashMap<String, String>,
    stdio: Option<(HANDLE, HANDLE, HANDLE)>,
) -> Result<(PROCESS_INFORMATION, STARTUPINFOW), String> {
    let cmdline_str = argv
        .iter()
        .map(|arg| quote_windows_arg(arg))
        .collect::<Vec<_>>()
        .join(" ");
    let mut cmdline = to_wide(&cmdline_str);

    let env_block = make_env_block(env_map);
    let mut startup_info: STARTUPINFOW = std::mem::zeroed();
    startup_info.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    let desktop = to_wide("Winsta0\\Default");
    startup_info.lpDesktop = desktop.as_ptr() as *mut u16;
    if let Some((stdin_handle, stdout_handle, stderr_handle)) = stdio {
        for handle in [stdin_handle, stdout_handle, stderr_handle] {
            if SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) == 0 {
                return Err(format!(
                    "SetHandleInformation failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
        }
        startup_info.dwFlags |= STARTF_USESTDHANDLES;
        startup_info.hStdInput = stdin_handle;
        startup_info.hStdOutput = stdout_handle;
        startup_info.hStdError = stderr_handle;
    } else {
        ensure_inheritable_stdio(&mut startup_info)?;
    }

    let mut proc_info: PROCESS_INFORMATION = std::mem::zeroed();
    let ok = CreateProcessAsUserW(
        token,
        std::ptr::null(),
        cmdline.as_mut_ptr(),
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        1,
        CREATE_UNICODE_ENVIRONMENT,
        env_block.as_ptr() as *mut c_void,
        to_wide(cwd).as_ptr(),
        &startup_info,
        &mut proc_info,
    );
    if ok == 0 {
        return Err(format!(
            "CreateProcessAsUserW failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok((proc_info, startup_info))
}

unsafe fn create_wrapper_child_stdio() -> Result<WrapperChildStdio, String> {
    let mut child_stdin: HANDLE = std::ptr::null_mut();
    let mut stdin_write: HANDLE = std::ptr::null_mut();
    let mut stdout_read: HANDLE = std::ptr::null_mut();
    let mut child_stdout: HANDLE = std::ptr::null_mut();
    let mut stderr_read: HANDLE = std::ptr::null_mut();
    let mut child_stderr: HANDLE = std::ptr::null_mut();

    if CreatePipe(&mut child_stdin, &mut stdin_write, std::ptr::null_mut(), 0) == 0 {
        return Err(format!(
            "CreatePipe stdin failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    if CreatePipe(&mut stdout_read, &mut child_stdout, std::ptr::null_mut(), 0) == 0 {
        CloseHandle(child_stdin);
        CloseHandle(stdin_write);
        return Err(format!(
            "CreatePipe stdout failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    if CreatePipe(&mut stderr_read, &mut child_stderr, std::ptr::null_mut(), 0) == 0 {
        CloseHandle(child_stdin);
        CloseHandle(stdin_write);
        CloseHandle(stdout_read);
        CloseHandle(child_stdout);
        return Err(format!(
            "CreatePipe stderr failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    for handle in [child_stdin, child_stdout, child_stderr] {
        if SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) == 0 {
            let err = format!(
                "SetHandleInformation failed for child stdio handle: {}",
                std::io::Error::last_os_error()
            );
            CloseHandle(child_stdin);
            CloseHandle(stdin_write);
            CloseHandle(stdout_read);
            CloseHandle(child_stdout);
            CloseHandle(stderr_read);
            CloseHandle(child_stderr);
            return Err(err);
        }
    }

    Ok(WrapperChildStdio {
        stdin_write: File::from_raw_handle(stdin_write as _),
        stdout_read: File::from_raw_handle(stdout_read as _),
        stderr_read: File::from_raw_handle(stderr_read as _),
        child_stdin: File::from_raw_handle(child_stdin as _),
        child_stdout: File::from_raw_handle(child_stdout as _),
        child_stderr: File::from_raw_handle(child_stderr as _),
    })
}

fn spawn_wrapper_stdio_forwarders(stdio: WrapperChildStdio) -> WrapperStdioForwarders {
    let WrapperChildStdio {
        stdin_write,
        stdout_read,
        stderr_read,
        child_stdin: _,
        child_stdout: _,
        child_stderr: _,
    } = stdio;

    let stdin_forwarder = thread::spawn(move || {
        let mut wrapper_stdin = io::stdin();
        let mut child_stdin = stdin_write;
        let _ = io::copy(&mut wrapper_stdin, &mut child_stdin);
        let _ = child_stdin.flush();
    });
    let stdout_state = Arc::new(WrapperForwarderState::new());
    let stdout_state_thread = Arc::clone(&stdout_state);
    let stdout_forwarder = thread::spawn(move || {
        copy_wrapper_output(stdout_read, io::stdout(), &stdout_state_thread);
    });
    let stderr_state = Arc::new(WrapperForwarderState::new());
    let stderr_state_thread = Arc::clone(&stderr_state);
    let stderr_forwarder = thread::spawn(move || {
        copy_wrapper_output(stderr_read, io::stderr(), &stderr_state_thread);
    });

    WrapperStdioForwarders {
        stdin_forwarder,
        stdout_forwarder,
        stderr_forwarder,
        stdout_state,
        stderr_state,
    }
}

fn copy_wrapper_output(
    mut child_output: File,
    mut wrapper_output: impl Write,
    state: &WrapperForwarderState,
) {
    let mut buffer = [0u8; 8192];
    loop {
        match child_output.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                let write_result = {
                    let _write_guard = state.begin_write();
                    let result = wrapper_output
                        .write_all(&buffer[..count])
                        .and_then(|_| wrapper_output.flush());
                    if result.is_ok() {
                        state
                            .bytes_copied
                            .fetch_add(count as u64, Ordering::Relaxed);
                    }
                    result
                };
                if write_result.is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    {
        let _write_guard = state.begin_write();
        let _ = wrapper_output.flush();
    }
    state.done.store(true, Ordering::Release);
}

fn drain_wrapper_forwarder(handle: thread::JoinHandle<()>, state: &WrapperForwarderState) {
    drain_wrapper_forwarder_with_timeouts(
        handle,
        state,
        WRAPPER_STDIO_DRAIN_IDLE_TIMEOUT,
        WRAPPER_STDIO_DRAIN_MAX_WAIT,
        WRAPPER_STDIO_DRAIN_POLL_INTERVAL,
    );
}

fn drain_wrapper_forwarder_with_timeouts(
    handle: thread::JoinHandle<()>,
    state: &WrapperForwarderState,
    idle_timeout: Duration,
    max_wait: Duration,
    poll_interval: Duration,
) {
    let start = Instant::now();
    let mut last_progress = start;
    let mut last_bytes = state.bytes_copied.load(Ordering::Relaxed);

    loop {
        if state.done.load(Ordering::Acquire) {
            let _ = handle.join();
            return;
        }

        let now = Instant::now();
        let bytes = state.bytes_copied.load(Ordering::Relaxed);
        if bytes != last_bytes {
            last_bytes = bytes;
            last_progress = now;
        }
        if state.write_in_progress.load(Ordering::Acquire) {
            last_progress = now;
        }

        if now.duration_since(last_progress) >= idle_timeout
            || now.duration_since(start) >= max_wait
        {
            drop(handle);
            return;
        }

        thread::sleep(poll_interval);
    }
}

unsafe fn ensure_inheritable_stdio(startup_info: &mut STARTUPINFOW) -> Result<(), String> {
    for std_handle_kind in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        let std_handle = GetStdHandle(std_handle_kind);
        if std_handle.is_null() || std_handle == INVALID_HANDLE_VALUE {
            return Err(format!(
                "GetStdHandle failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        if SetHandleInformation(std_handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) == 0 {
            return Err(format!(
                "SetHandleInformation failed: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    startup_info.dwFlags |= STARTF_USESTDHANDLES;
    startup_info.hStdInput = GetStdHandle(STD_INPUT_HANDLE);
    startup_info.hStdOutput = GetStdHandle(STD_OUTPUT_HANDLE);
    startup_info.hStdError = GetStdHandle(STD_ERROR_HANDLE);
    Ok(())
}

fn make_env_block(env: &HashMap<String, String>) -> Vec<u16> {
    let mut items: Vec<(String, String)> =
        env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    items.sort_by(|a, b| {
        a.0.to_uppercase()
            .cmp(&b.0.to_uppercase())
            .then(a.0.cmp(&b.0))
    });
    let mut wide_env: Vec<u16> = Vec::new();
    for (key, value) in items {
        let mut entry = to_wide(format!("{key}={value}"));
        entry.pop();
        wide_env.extend_from_slice(&entry);
        wide_env.push(0);
    }
    wide_env.push(0);
    wide_env
}

fn to_wide<S: AsRef<OsStr>>(value: S) -> Vec<u16> {
    let mut wide: Vec<u16> = value.as_ref().encode_wide().collect();
    wide.push(0);
    wide
}

fn quote_windows_arg(arg: &str) -> String {
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
        match ch {
            '\\' => {
                backslashes += 1;
            }
            '"' => {
                quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            _ => {
                if backslashes > 0 {
                    quoted.push_str(&"\\".repeat(backslashes));
                    backslashes = 0;
                }
                quoted.push(ch);
            }
        }
    }
    if backslashes > 0 {
        quoted.push_str(&"\\".repeat(backslashes * 2));
    }
    quoted.push('"');
    quoted
}

unsafe fn add_allow_ace(path: &Path, sid: *mut c_void) -> Result<bool, String> {
    if !path.exists() {
        std::fs::create_dir_all(path)
            .map_err(|err| format!("create_dir_all failed for '{}': {err}", path.display()))?;
    }

    let mut security_descriptor: *mut c_void = std::ptr::null_mut();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let code = GetNamedSecurityInfoW(
        to_wide(path).as_ptr(),
        SE_FILE_OBJECT,
        DACL_SECURITY_INFORMATION,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        &mut dacl,
        std::ptr::null_mut(),
        &mut security_descriptor,
    );
    if code != ERROR_SUCCESS {
        return Err(format!("GetNamedSecurityInfoW failed: {code}"));
    }
    if dacl_has_allow_for_sid(dacl, sid, path.is_dir()) {
        if !security_descriptor.is_null() {
            LocalFree(security_descriptor as HLOCAL);
        }
        return Ok(false);
    }

    let trustee = TRUSTEE_W {
        pMultipleTrustee: std::ptr::null_mut(),
        MultipleTrusteeOperation: 0,
        TrusteeForm: TRUSTEE_IS_SID,
        TrusteeType: TRUSTEE_IS_UNKNOWN,
        ptstrName: sid as *mut u16,
    };
    let mut explicit: EXPLICIT_ACCESS_W = std::mem::zeroed();
    explicit.grfAccessPermissions = FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE;
    explicit.grfAccessMode = GRANT_ACCESS;
    explicit.grfInheritance = CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE;
    explicit.Trustee = trustee;

    let mut new_dacl: *mut ACL = std::ptr::null_mut();
    let set_acl_code = SetEntriesInAclW(1, &explicit, dacl, &mut new_dacl);
    if set_acl_code != ERROR_SUCCESS {
        if !security_descriptor.is_null() {
            LocalFree(security_descriptor as HLOCAL);
        }
        return Err(format!("SetEntriesInAclW failed: {set_acl_code}"));
    }

    let set_security_code = SetNamedSecurityInfoW(
        to_wide(path).as_ptr() as *mut u16,
        SE_FILE_OBJECT,
        DACL_SECURITY_INFORMATION,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        new_dacl,
        std::ptr::null_mut(),
    );
    if !new_dacl.is_null() {
        LocalFree(new_dacl as HLOCAL);
    }
    if !security_descriptor.is_null() {
        LocalFree(security_descriptor as HLOCAL);
    }
    if set_security_code != ERROR_SUCCESS {
        return Err(format!("SetNamedSecurityInfoW failed: {set_security_code}"));
    }
    Ok(true)
}

unsafe fn add_deny_write_ace(path: &Path, sid: *mut c_void) -> Result<bool, String> {
    #[cfg(test)]
    {
        let injected_error =
            ADD_DENY_WRITE_ACE_TEST_ERROR.with(|slot| match slot.borrow_mut().as_mut() {
                Some((remaining_successes, error)) if *remaining_successes == 0 => {
                    Some(Err(error.clone()))
                }
                Some((remaining_successes, _)) => {
                    *remaining_successes -= 1;
                    Some(Ok(()))
                }
                None => None,
            });
        if let Some(Err(error)) = injected_error {
            return Err(error);
        }
    }

    let mut security_descriptor: *mut c_void = std::ptr::null_mut();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let code = GetNamedSecurityInfoW(
        to_wide(path).as_ptr(),
        SE_FILE_OBJECT,
        DACL_SECURITY_INFORMATION,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        &mut dacl,
        std::ptr::null_mut(),
        &mut security_descriptor,
    );
    if code != ERROR_SUCCESS {
        return Err(format!("GetNamedSecurityInfoW failed: {code}"));
    }
    if dacl_has_write_deny_for_sid(dacl, sid) {
        if !security_descriptor.is_null() {
            LocalFree(security_descriptor as HLOCAL);
        }
        return Ok(false);
    }

    let trustee = TRUSTEE_W {
        pMultipleTrustee: std::ptr::null_mut(),
        MultipleTrusteeOperation: 0,
        TrusteeForm: TRUSTEE_IS_SID,
        TrusteeType: TRUSTEE_IS_UNKNOWN,
        ptstrName: sid as *mut u16,
    };
    let mut explicit: EXPLICIT_ACCESS_W = std::mem::zeroed();
    explicit.grfAccessPermissions = FILE_GENERIC_WRITE
        | FILE_WRITE_DATA
        | FILE_APPEND_DATA
        | FILE_WRITE_EA
        | FILE_WRITE_ATTRIBUTES
        | DELETE
        | FILE_DELETE_CHILD;
    explicit.grfAccessMode = DENY_ACCESS;
    explicit.grfInheritance = CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE;
    explicit.Trustee = trustee;

    let mut new_dacl: *mut ACL = std::ptr::null_mut();
    let set_acl_code = SetEntriesInAclW(1, &explicit, dacl, &mut new_dacl);
    if set_acl_code != ERROR_SUCCESS {
        if !security_descriptor.is_null() {
            LocalFree(security_descriptor as HLOCAL);
        }
        return Err(format!("SetEntriesInAclW failed: {set_acl_code}"));
    }

    let set_security_code = SetNamedSecurityInfoW(
        to_wide(path).as_ptr() as *mut u16,
        SE_FILE_OBJECT,
        DACL_SECURITY_INFORMATION,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        new_dacl,
        std::ptr::null_mut(),
    );
    if !new_dacl.is_null() {
        LocalFree(new_dacl as HLOCAL);
    }
    if !security_descriptor.is_null() {
        LocalFree(security_descriptor as HLOCAL);
    }
    if set_security_code != ERROR_SUCCESS {
        return Err(format!("SetNamedSecurityInfoW failed: {set_security_code}"));
    }
    Ok(true)
}

fn deny_write_mask() -> u32 {
    FILE_GENERIC_WRITE
        | FILE_WRITE_DATA
        | FILE_APPEND_DATA
        | FILE_WRITE_EA
        | FILE_WRITE_ATTRIBUTES
        | DELETE
        | FILE_DELETE_CHILD
}

unsafe fn dacl_size_info(dacl: *mut ACL) -> Option<ACL_SIZE_INFORMATION> {
    if dacl.is_null() {
        return None;
    }
    let mut info: ACL_SIZE_INFORMATION = std::mem::zeroed();
    let ok = GetAclInformation(
        dacl as *const ACL,
        &mut info as *mut _ as *mut c_void,
        std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
        AclSizeInformation,
    );
    (ok != 0).then_some(info)
}

unsafe fn ace_sid_ptr(ace: *mut c_void) -> *mut c_void {
    let base = ace as usize;
    (base + std::mem::size_of::<ACE_HEADER>() + std::mem::size_of::<u32>()) as *mut c_void
}

fn allow_ace_satisfies_requirements(mask: u32, ace_flags: u8, require_inheritance: bool) -> bool {
    if (mask & (FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE))
        != (FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE)
    {
        return false;
    }
    if require_inheritance
        && ((ace_flags & (CONTAINER_INHERIT_ACE as u8)) == 0
            || (ace_flags & (OBJECT_INHERIT_ACE as u8)) == 0)
    {
        return false;
    }
    true
}

unsafe fn dacl_has_allow_for_sid(
    dacl: *mut ACL,
    sid: *mut c_void,
    require_inheritance: bool,
) -> bool {
    let Some(info) = dacl_size_info(dacl) else {
        return false;
    };
    for index in 0..info.AceCount {
        let mut ace: *mut c_void = std::ptr::null_mut();
        if GetAce(dacl as *const ACL, index, &mut ace) == 0 {
            continue;
        }
        let header = &*(ace as *const ACE_HEADER);
        if header.AceType != 0 || (header.AceFlags & INHERIT_ONLY_ACE) != 0 {
            continue;
        }
        let allowed = &*(ace as *const ACCESS_ALLOWED_ACE);
        if EqualSid(ace_sid_ptr(ace), sid) != 0
            && allow_ace_satisfies_requirements(allowed.Mask, header.AceFlags, require_inheritance)
        {
            return true;
        }
    }
    false
}

unsafe fn dacl_has_write_deny_for_sid(dacl: *mut ACL, sid: *mut c_void) -> bool {
    let Some(info) = dacl_size_info(dacl) else {
        return false;
    };
    let mut denied_mask = 0;
    for index in 0..info.AceCount {
        let mut ace: *mut c_void = std::ptr::null_mut();
        if GetAce(dacl as *const ACL, index, &mut ace) == 0 {
            continue;
        }
        let header = &*(ace as *const ACE_HEADER);
        if header.AceType != 1 || (header.AceFlags & INHERIT_ONLY_ACE) != 0 {
            continue;
        }
        let denied = &*(ace as *const ACCESS_DENIED_ACE);
        if EqualSid(ace_sid_ptr(ace), sid) != 0 {
            denied_mask |= denied.Mask;
        }
    }
    (denied_mask & deny_write_mask()) == deny_write_mask()
}

unsafe fn revoke_ace(path: &Path, sid: *mut c_void) {
    let mut security_descriptor: *mut c_void = std::ptr::null_mut();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let code = GetNamedSecurityInfoW(
        to_wide(path).as_ptr(),
        SE_FILE_OBJECT,
        DACL_SECURITY_INFORMATION,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        &mut dacl,
        std::ptr::null_mut(),
        &mut security_descriptor,
    );
    if code != ERROR_SUCCESS {
        if !security_descriptor.is_null() {
            LocalFree(security_descriptor as HLOCAL);
        }
        return;
    }

    let trustee = TRUSTEE_W {
        pMultipleTrustee: std::ptr::null_mut(),
        MultipleTrusteeOperation: 0,
        TrusteeForm: TRUSTEE_IS_SID,
        TrusteeType: TRUSTEE_IS_UNKNOWN,
        ptstrName: sid as *mut u16,
    };
    let mut explicit: EXPLICIT_ACCESS_W = std::mem::zeroed();
    explicit.grfAccessPermissions = 0;
    explicit.grfAccessMode = REVOKE_ACCESS;
    explicit.grfInheritance = CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE;
    explicit.Trustee = trustee;

    let mut new_dacl: *mut ACL = std::ptr::null_mut();
    let set_acl_code = SetEntriesInAclW(1, &explicit, dacl, &mut new_dacl);
    if set_acl_code == ERROR_SUCCESS {
        let _ = SetNamedSecurityInfoW(
            to_wide(path).as_ptr() as *mut u16,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            new_dacl,
            std::ptr::null_mut(),
        );
        if !new_dacl.is_null() {
            LocalFree(new_dacl as HLOCAL);
        }
    }
    if !security_descriptor.is_null() {
        LocalFree(security_descriptor as HLOCAL);
    }
}

unsafe fn revoke_deny_write_ace(path: &Path, sid: *mut c_void) {
    let mut security_descriptor: *mut c_void = std::ptr::null_mut();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let code = GetNamedSecurityInfoW(
        to_wide(path).as_ptr(),
        SE_FILE_OBJECT,
        DACL_SECURITY_INFORMATION,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        &mut dacl,
        std::ptr::null_mut(),
        &mut security_descriptor,
    );
    if code != ERROR_SUCCESS {
        if !security_descriptor.is_null() {
            LocalFree(security_descriptor as HLOCAL);
        }
        return;
    }

    let Some(info) = dacl_size_info(dacl) else {
        if !security_descriptor.is_null() {
            LocalFree(security_descriptor as HLOCAL);
        }
        return;
    };

    let mut changed = false;
    for index in (0..info.AceCount).rev() {
        let mut ace: *mut c_void = std::ptr::null_mut();
        if GetAce(dacl as *const ACL, index, &mut ace) == 0 {
            continue;
        }
        let header = &*(ace as *const ACE_HEADER);
        if header.AceType != 1 {
            continue;
        }
        if EqualSid(ace_sid_ptr(ace), sid) == 0 {
            continue;
        }
        if DeleteAce(dacl, index) != 0 {
            changed = true;
        }
    }

    if changed {
        let _ = SetNamedSecurityInfoW(
            to_wide(path).as_ptr() as *mut u16,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            dacl,
            std::ptr::null_mut(),
        );
    }
    if !security_descriptor.is_null() {
        LocalFree(security_descriptor as HLOCAL);
    }
}

unsafe fn allow_null_device(sid: *mut c_void) -> bool {
    let nul_path = to_wide(r"\\.\NUL");
    let mut security_descriptor: *mut c_void = std::ptr::null_mut();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let code = GetNamedSecurityInfoW(
        nul_path.as_ptr(),
        SE_FILE_OBJECT,
        DACL_SECURITY_INFORMATION,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        &mut dacl,
        std::ptr::null_mut(),
        &mut security_descriptor,
    );
    if code != ERROR_SUCCESS {
        return false;
    }

    let trustee = TRUSTEE_W {
        pMultipleTrustee: std::ptr::null_mut(),
        MultipleTrusteeOperation: 0,
        TrusteeForm: TRUSTEE_IS_SID,
        TrusteeType: TRUSTEE_IS_UNKNOWN,
        ptstrName: sid as *mut u16,
    };
    let mut explicit: EXPLICIT_ACCESS_W = std::mem::zeroed();
    explicit.grfAccessPermissions = FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE;
    explicit.grfAccessMode = GRANT_ACCESS;
    explicit.grfInheritance = 0;
    explicit.Trustee = trustee;

    let mut new_dacl: *mut ACL = std::ptr::null_mut();
    let set_acl_code = SetEntriesInAclW(1, &explicit, dacl, &mut new_dacl);
    let set_security_code = if set_acl_code == ERROR_SUCCESS {
        SetNamedSecurityInfoW(
            nul_path.as_ptr() as *mut u16,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            new_dacl,
            std::ptr::null_mut(),
        )
    } else {
        set_acl_code
    };
    if !new_dacl.is_null() {
        LocalFree(new_dacl as HLOCAL);
    }
    if !security_descriptor.is_null() {
        LocalFree(security_descriptor as HLOCAL);
    }

    set_security_code == ERROR_SUCCESS
}

unsafe fn revoke_null_device_ace(sid: *mut c_void) {
    let nul_path = to_wide(r"\\.\NUL");
    let mut security_descriptor: *mut c_void = std::ptr::null_mut();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let code = GetNamedSecurityInfoW(
        nul_path.as_ptr(),
        SE_FILE_OBJECT,
        DACL_SECURITY_INFORMATION,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        &mut dacl,
        std::ptr::null_mut(),
        &mut security_descriptor,
    );
    if code != ERROR_SUCCESS {
        if !security_descriptor.is_null() {
            LocalFree(security_descriptor as HLOCAL);
        }
        return;
    }

    let trustee = TRUSTEE_W {
        pMultipleTrustee: std::ptr::null_mut(),
        MultipleTrusteeOperation: 0,
        TrusteeForm: TRUSTEE_IS_SID,
        TrusteeType: TRUSTEE_IS_UNKNOWN,
        ptstrName: sid as *mut u16,
    };
    let mut explicit: EXPLICIT_ACCESS_W = std::mem::zeroed();
    explicit.grfAccessPermissions = 0;
    explicit.grfAccessMode = REVOKE_ACCESS;
    explicit.grfInheritance = 0;
    explicit.Trustee = trustee;

    let mut new_dacl: *mut ACL = std::ptr::null_mut();
    let set_acl_code = SetEntriesInAclW(1, &explicit, dacl, &mut new_dacl);
    if set_acl_code == ERROR_SUCCESS {
        let _ = SetNamedSecurityInfoW(
            nul_path.as_ptr() as *mut u16,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            new_dacl,
            std::ptr::null_mut(),
        );
        if !new_dacl.is_null() {
            LocalFree(new_dacl as HLOCAL);
        }
    }
    if !security_descriptor.is_null() {
        LocalFree(security_descriptor as HLOCAL);
    }
}

unsafe fn set_default_dacl(token: HANDLE, sids: &[*mut c_void]) -> Result<(), String> {
    if sids.is_empty() {
        return Ok(());
    }

    let entries: Vec<EXPLICIT_ACCESS_W> = sids
        .iter()
        .map(|sid| EXPLICIT_ACCESS_W {
            grfAccessPermissions: GENERIC_ALL,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: 0,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: std::ptr::null_mut(),
                MultipleTrusteeOperation: 0,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_UNKNOWN,
                ptstrName: *sid as *mut u16,
            },
        })
        .collect();

    let mut new_dacl: *mut ACL = std::ptr::null_mut();
    let status = SetEntriesInAclW(
        entries.len() as u32,
        entries.as_ptr(),
        std::ptr::null_mut(),
        &mut new_dacl,
    );
    if status != ERROR_SUCCESS {
        return Err(format!("SetEntriesInAclW failed: {status}"));
    }

    let mut info = TokenDefaultDaclInfo {
        default_dacl: new_dacl,
    };
    let ok = SetTokenInformation(
        token,
        TokenDefaultDacl,
        &mut info as *mut _ as *mut c_void,
        std::mem::size_of::<TokenDefaultDaclInfo>() as u32,
    );
    if ok == 0 {
        if !new_dacl.is_null() {
            LocalFree(new_dacl as HLOCAL);
        }
        return Err(format!(
            "SetTokenInformation(TokenDefaultDacl) failed: {}",
            GetLastError()
        ));
    }

    if !new_dacl.is_null() {
        LocalFree(new_dacl as HLOCAL);
    }
    Ok(())
}

unsafe fn enable_single_privilege(token: HANDLE, privilege_name: &str) -> Result<(), String> {
    let mut luid = LUID {
        LowPart: 0,
        HighPart: 0,
    };
    let ok = LookupPrivilegeValueW(
        std::ptr::null(),
        to_wide(privilege_name).as_ptr(),
        &mut luid,
    );
    if ok == 0 {
        return Err(format!("LookupPrivilegeValueW failed: {}", GetLastError()));
    }

    let mut privileges: TOKEN_PRIVILEGES = std::mem::zeroed();
    privileges.PrivilegeCount = 1;
    privileges.Privileges[0].Luid = luid;
    privileges.Privileges[0].Attributes = 0x00000002;
    let ok = AdjustTokenPrivileges(
        token,
        0,
        &privileges,
        0,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
    );
    if ok == 0 {
        return Err(format!("AdjustTokenPrivileges failed: {}", GetLastError()));
    }
    if GetLastError() != 0 {
        return Err(format!(
            "AdjustTokenPrivileges completed with error: {}",
            GetLastError()
        ));
    }
    Ok(())
}

unsafe fn get_current_token_for_restriction() -> Result<HANDLE, String> {
    let desired_access = TOKEN_DUPLICATE
        | TOKEN_QUERY
        | TOKEN_ASSIGN_PRIMARY
        | TOKEN_ADJUST_DEFAULT
        | TOKEN_ADJUST_SESSIONID
        | TOKEN_ADJUST_PRIVILEGES;
    let mut token: HANDLE = std::ptr::null_mut();
    let ok = OpenProcessToken(GetCurrentProcess(), desired_access, &mut token);
    if ok == 0 {
        return Err(format!("OpenProcessToken failed: {}", GetLastError()));
    }
    Ok(token)
}

unsafe fn world_sid() -> Result<Vec<u8>, String> {
    let mut sid_len: u32 = 0;
    CreateWellKnownSid(
        WIN_WORLD_SID,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        &mut sid_len,
    );
    let mut sid = vec![0_u8; sid_len as usize];
    let ok = CreateWellKnownSid(
        WIN_WORLD_SID,
        std::ptr::null_mut(),
        sid.as_mut_ptr() as *mut c_void,
        &mut sid_len,
    );
    if ok == 0 {
        return Err(format!("CreateWellKnownSid failed: {}", GetLastError()));
    }
    Ok(sid)
}

unsafe fn get_logon_sid_bytes(token: HANDLE) -> Result<Vec<u8>, String> {
    unsafe fn scan_token_groups_for_logon(token: HANDLE) -> Option<Vec<u8>> {
        let mut required: u32 = 0;
        GetTokenInformation(token, TokenGroups, std::ptr::null_mut(), 0, &mut required);
        if required == 0 {
            return None;
        }

        let mut groups_buf = vec![0_u8; required as usize];
        let ok = GetTokenInformation(
            token,
            TokenGroups,
            groups_buf.as_mut_ptr() as *mut c_void,
            required,
            &mut required,
        );
        if ok == 0 || (required as usize) < std::mem::size_of::<u32>() {
            return None;
        }

        let group_count = std::ptr::read_unaligned(groups_buf.as_ptr() as *const u32) as usize;
        let after_count = groups_buf.as_ptr().add(std::mem::size_of::<u32>()) as usize;
        let align = std::mem::align_of::<SID_AND_ATTRIBUTES>();
        let aligned = (after_count + (align - 1)) & !(align - 1);
        let groups_ptr = aligned as *const SID_AND_ATTRIBUTES;

        for index in 0..group_count {
            let entry = std::ptr::read_unaligned(groups_ptr.add(index));
            if (entry.Attributes & SE_GROUP_LOGON_ID) != SE_GROUP_LOGON_ID {
                continue;
            }
            let sid_len = GetLengthSid(entry.Sid);
            if sid_len == 0 {
                return None;
            }
            let mut sid = vec![0_u8; sid_len as usize];
            if CopySid(sid_len, sid.as_mut_ptr() as *mut c_void, entry.Sid) == 0 {
                return None;
            }
            return Some(sid);
        }

        None
    }

    if let Some(logon_sid) = scan_token_groups_for_logon(token) {
        return Ok(logon_sid);
    }

    #[repr(C)]
    struct TokenLinkedToken {
        linked_token: HANDLE,
    }
    const TOKEN_LINKED_TOKEN_CLASS: i32 = 19;

    let mut required: u32 = 0;
    GetTokenInformation(
        token,
        TOKEN_LINKED_TOKEN_CLASS,
        std::ptr::null_mut(),
        0,
        &mut required,
    );
    if required >= std::mem::size_of::<TokenLinkedToken>() as u32 {
        let mut linked_buf = vec![0_u8; required as usize];
        let ok = GetTokenInformation(
            token,
            TOKEN_LINKED_TOKEN_CLASS,
            linked_buf.as_mut_ptr() as *mut c_void,
            required,
            &mut required,
        );
        if ok != 0 {
            let linked = std::ptr::read_unaligned(linked_buf.as_ptr() as *const TokenLinkedToken);
            if !linked.linked_token.is_null() {
                let result = scan_token_groups_for_logon(linked.linked_token);
                CloseHandle(linked.linked_token);
                if let Some(logon_sid) = result {
                    return Ok(logon_sid);
                }
            }
        }
    }

    Err("logon SID not present on token".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    #[cfg(target_os = "windows")]
    use std::process::Command;
    use std::sync::Arc;
    use tempfile::tempdir;

    fn workspace_policy(
        writable_roots: Vec<PathBuf>,
        network_access: bool,
        exclude_tmpdir_env_var: bool,
    ) -> SandboxPolicy {
        SandboxPolicy::WorkspaceWrite {
            writable_roots,
            network_access,
            exclude_tmpdir_env_var,
            exclude_slash_tmp: false,
        }
    }

    fn prepared_launch_workspace_tempdir() -> tempfile::TempDir {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("windows-sandbox-tests");
        std::fs::create_dir_all(&root).expect("create prepared launch workspace test root");
        tempfile::Builder::new()
            .prefix("prepared-launch-")
            .tempdir_in(&root)
            .expect("prepared launch workspace tempdir")
    }

    #[cfg(target_os = "windows")]
    fn remove_junction(path: &Path) {
        if !path.exists() {
            return;
        }

        if std::fs::remove_dir(path).is_ok() {
            return;
        }

        let output = Command::new("cmd")
            .args(["/C", "rmdir", &path.to_string_lossy()])
            .output()
            .expect("spawn rmdir");
        assert!(
            output.status.success(),
            "failed to remove junction '{}': stdout={} stderr={}",
            path.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    #[cfg(target_os = "windows")]
    fn create_junction(path: &Path, target: &Path) {
        remove_junction(path);
        let output = Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                &path.to_string_lossy(),
                &target.to_string_lossy(),
            ])
            .output()
            .expect("spawn mklink");
        assert!(
            output.status.success(),
            "failed to create junction '{}' -> '{}': stdout={} stderr={}",
            path.display(),
            target.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    struct BlockingWriter {
        entered: Arc<AtomicBool>,
        release: Arc<(Mutex<bool>, std::sync::Condvar)>,
    }

    impl Write for BlockingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.entered.store(true, Ordering::Release);
            let (lock, cvar) = &*self.release;
            let mut released = lock.lock().expect("blocking writer release mutex");
            while !*released {
                released = cvar.wait(released).expect("blocking writer release mutex");
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct RecordingWriterState {
        bytes: Vec<u8>,
        flush_count: usize,
    }

    struct RecordingWriter {
        state: Arc<Mutex<RecordingWriterState>>,
    }

    impl Write for RecordingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.state
                .lock()
                .expect("recording writer state mutex")
                .bytes
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.state
                .lock()
                .expect("recording writer state mutex")
                .flush_count += 1;
            Ok(())
        }
    }

    #[test]
    fn drain_wrapper_forwarder_waits_while_progress_continues() {
        let state = Arc::new(WrapperForwarderState::new());
        let thread_state = Arc::clone(&state);
        let handle = thread::spawn(move || {
            for _ in 0..4 {
                thread::sleep(Duration::from_millis(20));
                thread_state.bytes_copied.fetch_add(1024, Ordering::Relaxed);
            }
            thread_state.done.store(true, Ordering::Release);
        });

        let start = Instant::now();
        drain_wrapper_forwarder_with_timeouts(
            handle,
            &state,
            Duration::from_millis(30),
            Duration::from_millis(500),
            Duration::from_millis(5),
        );

        assert!(start.elapsed() >= Duration::from_millis(60));
        assert!(state.done.load(Ordering::Acquire));
    }

    #[test]
    fn drain_wrapper_forwarder_stops_after_idle_timeout_without_progress() {
        let state = Arc::new(WrapperForwarderState::new());
        let thread_state = Arc::clone(&state);
        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            thread_state.done.store(true, Ordering::Release);
        });

        let start = Instant::now();
        drain_wrapper_forwarder_with_timeouts(
            handle,
            &state,
            Duration::from_millis(20),
            Duration::from_millis(200),
            Duration::from_millis(5),
        );

        assert!(start.elapsed() < Duration::from_millis(80));
        thread::sleep(Duration::from_millis(120));
    }

    #[test]
    fn drain_wrapper_forwarder_stops_after_max_wait_for_blocked_write() {
        let tmp = tempdir().expect("tempdir");
        let payload_path = tmp.path().join("payload.bin");
        std::fs::write(&payload_path, vec![b'x'; 8192]).expect("write payload");

        let state = Arc::new(WrapperForwarderState::new());
        let entered = Arc::new(AtomicBool::new(false));
        let release = Arc::new((Mutex::new(false), std::sync::Condvar::new()));

        let thread_state = Arc::clone(&state);
        let writer = BlockingWriter {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        };
        let handle = thread::spawn(move || {
            let input = File::open(&payload_path).expect("open payload");
            copy_wrapper_output(input, writer, &thread_state);
        });

        while !entered.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(5));
        }

        let release_gate = Arc::clone(&release);
        let releaser = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            let (lock, cvar) = &*release_gate;
            let mut released = lock.lock().expect("release mutex");
            *released = true;
            cvar.notify_all();
        });

        let start = Instant::now();
        drain_wrapper_forwarder_with_timeouts(
            handle,
            &state,
            Duration::from_millis(20),
            Duration::from_millis(40),
            Duration::from_millis(5),
        );

        let elapsed = start.elapsed();
        let finished_before_cleanup = state.done.load(Ordering::Acquire);
        releaser.join().expect("releaser thread should not panic");
        assert!(
            elapsed < Duration::from_millis(80),
            "drain should stop waiting once the blocked write exceeds the timeout"
        );
        assert!(
            !finished_before_cleanup,
            "drain should return before a blocked write finishes"
        );
    }

    #[test]
    fn drain_wrapper_forwarder_waits_for_inflight_write_before_idle_timeout() {
        let tmp = tempdir().expect("tempdir");
        let payload_path = tmp.path().join("payload.bin");
        std::fs::write(&payload_path, vec![b'x'; 8192]).expect("write payload");

        let state = Arc::new(WrapperForwarderState::new());
        let entered = Arc::new(AtomicBool::new(false));
        let release = Arc::new((Mutex::new(false), std::sync::Condvar::new()));

        let thread_state = Arc::clone(&state);
        let writer = BlockingWriter {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        };
        let handle = thread::spawn(move || {
            let input = File::open(&payload_path).expect("open payload");
            copy_wrapper_output(input, writer, &thread_state);
        });

        while !entered.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(5));
        }

        let release_gate = Arc::clone(&release);
        let releaser = thread::spawn(move || {
            thread::sleep(Duration::from_millis(80));
            let (lock, cvar) = &*release_gate;
            let mut released = lock.lock().expect("release mutex");
            *released = true;
            cvar.notify_all();
        });

        let start = Instant::now();
        drain_wrapper_forwarder_with_timeouts(
            handle,
            &state,
            Duration::from_millis(20),
            Duration::from_millis(200),
            Duration::from_millis(5),
        );

        let elapsed = start.elapsed();
        let finished_before_release = state.done.load(Ordering::Acquire);
        releaser.join().expect("releaser thread should not panic");
        assert!(
            finished_before_release,
            "drain should keep waiting while a write is in progress and finishes before max_wait"
        );
        assert!(
            elapsed >= Duration::from_millis(60),
            "drain should not treat an in-flight write as idle"
        );
    }

    #[test]
    fn copy_wrapper_output_flushes_after_each_chunk() {
        let tmp = tempdir().expect("tempdir");
        let payload_path = tmp.path().join("payload.bin");
        std::fs::write(&payload_path, b"prompt> ").expect("write payload");

        let state = WrapperForwarderState::new();
        let writer_state = Arc::new(Mutex::new(RecordingWriterState::default()));
        let writer = RecordingWriter {
            state: Arc::clone(&writer_state),
        };

        let input = File::open(&payload_path).expect("open payload");
        copy_wrapper_output(input, writer, &state);

        let recorded = writer_state.lock().expect("recording writer state mutex");
        assert_eq!(recorded.bytes, b"prompt> ");
        assert!(
            recorded.flush_count >= 2,
            "forwarded output should flush each chunk as well as the final stream flush"
        );
    }

    #[test]
    fn direct_wrapper_launch_uses_inherited_stdio() {
        assert_eq!(wrapper_stdio_mode(None), WrapperStdioMode::Inherit);
    }

    #[test]
    fn prepared_wrapper_launch_uses_forwarded_pipes() {
        assert_eq!(
            wrapper_stdio_mode(Some("S-1-5-21-1-2-3-4")),
            WrapperStdioMode::ForwardedPipes
        );
    }

    #[test]
    fn applies_network_block_when_access_is_disabled() {
        assert!(should_apply_network_block(&workspace_policy(
            Vec::new(),
            false,
            false,
        )));
    }

    #[test]
    fn skips_network_block_when_access_is_allowed() {
        assert!(!should_apply_network_block(&workspace_policy(
            Vec::new(),
            true,
            false,
        )));
    }

    #[test]
    fn applies_network_block_for_read_only() {
        assert!(should_apply_network_block(&SandboxPolicy::ReadOnly));
    }

    #[test]
    fn apply_no_network_to_env_overrides_existing_values() {
        let mut env_map = HashMap::new();
        env_map.insert(
            "HTTP_PROXY".to_string(),
            "http://proxy.example:8080".to_string(),
        );
        env_map.insert(
            "HTTPS_PROXY".to_string(),
            "http://proxy.example:8080".to_string(),
        );
        env_map.insert(
            "ALL_PROXY".to_string(),
            "http://proxy.example:8080".to_string(),
        );
        env_map.insert("NO_PROXY".to_string(), "example.com".to_string());
        env_map.insert("CARGO_NET_OFFLINE".to_string(), "false".to_string());
        env_map.insert("NPM_CONFIG_OFFLINE".to_string(), "false".to_string());
        env_map.insert("PIP_NO_INDEX".to_string(), "0".to_string());

        apply_no_network_to_env(&mut env_map);

        assert_eq!(
            env_map.get("HTTP_PROXY"),
            Some(&"http://127.0.0.1:9".to_string())
        );
        assert_eq!(
            env_map.get("HTTPS_PROXY"),
            Some(&"http://127.0.0.1:9".to_string())
        );
        assert_eq!(
            env_map.get("ALL_PROXY"),
            Some(&"http://127.0.0.1:9".to_string())
        );
        assert_eq!(
            env_map.get("NO_PROXY"),
            Some(&"localhost,127.0.0.1,::1".to_string())
        );
        assert_eq!(env_map.get("CARGO_NET_OFFLINE"), Some(&"true".to_string()));
        assert_eq!(env_map.get("NPM_CONFIG_OFFLINE"), Some(&"true".to_string()));
        assert_eq!(env_map.get("PIP_NO_INDEX"), Some(&"1".to_string()));
    }

    #[test]
    fn apply_no_network_to_env_removes_case_variant_proxy_keys() {
        let mut env_map = HashMap::new();
        env_map.insert(
            "http_proxy".to_string(),
            "http://proxy.example:8080".to_string(),
        );
        env_map.insert(
            "Https_Proxy".to_string(),
            "http://proxy.example:8080".to_string(),
        );
        env_map.insert(
            "all_proxy".to_string(),
            "http://proxy.example:8080".to_string(),
        );
        env_map.insert("no_proxy".to_string(), "example.com".to_string());
        env_map.insert("pip_no_index".to_string(), "0".to_string());

        apply_no_network_to_env(&mut env_map);

        assert!(!env_map.contains_key("http_proxy"));
        assert!(!env_map.contains_key("Https_Proxy"));
        assert!(!env_map.contains_key("all_proxy"));
        assert!(!env_map.contains_key("no_proxy"));
        assert!(!env_map.contains_key("pip_no_index"));
        assert_eq!(
            env_map.get("HTTP_PROXY"),
            Some(&"http://127.0.0.1:9".to_string())
        );
        assert_eq!(
            env_map.get("HTTPS_PROXY"),
            Some(&"http://127.0.0.1:9".to_string())
        );
        assert_eq!(
            env_map.get("ALL_PROXY"),
            Some(&"http://127.0.0.1:9".to_string())
        );
        assert_eq!(
            env_map.get("NO_PROXY"),
            Some(&"localhost,127.0.0.1,::1".to_string())
        );
        assert_eq!(env_map.get("PIP_NO_INDEX"), Some(&"1".to_string()));
    }

    #[test]
    fn env_get_case_insensitive_matches_mixed_case_keys() {
        let mut env_map = HashMap::new();
        env_map.insert("Temp".to_string(), r"C:\Temp\session".to_string());
        assert_eq!(
            env_get_case_insensitive(&env_map, "TEMP"),
            Some(r"C:\Temp\session")
        );
        assert_eq!(
            env_get_case_insensitive(&env_map, "temp"),
            Some(r"C:\Temp\session")
        );
    }

    #[test]
    fn rejects_danger_full_access_policy() {
        let err = validate_windows_policy(&SandboxPolicy::DangerFullAccess)
            .expect_err("danger-full-access should be rejected");
        assert!(err.contains("read-only/workspace-write"));
    }

    #[test]
    fn rejects_external_sandbox_policy() {
        let err = validate_windows_policy(&SandboxPolicy::ExternalSandbox {
            network_access: crate::sandbox::NetworkAccess::Enabled,
        })
        .expect_err("external-sandbox should be rejected");
        assert!(err.contains("read-only/workspace-write"));
    }

    #[test]
    fn compute_allow_paths_includes_additional_writable_roots() {
        let tmp = tempdir().expect("tempdir");
        let command_cwd = tmp.path().join("workspace");
        let extra_root = tmp.path().join("extra");
        std::fs::create_dir_all(&command_cwd).expect("workspace dir");
        std::fs::create_dir_all(&extra_root).expect("extra dir");

        let policy = workspace_policy(vec![extra_root.clone()], false, true);
        let paths =
            compute_allow_deny_paths(&policy, &command_cwd, &command_cwd, None, &HashMap::new());

        assert!(
            paths
                .allow
                .contains(&canonicalize_or_identity(&command_cwd))
        );
        assert!(paths.allow.contains(&canonicalize_or_identity(&extra_root)));
        assert!(paths.deny.is_empty());
    }

    #[test]
    fn compute_allow_paths_reads_temp_env_case_insensitively() {
        let tmp = tempdir().expect("tempdir");
        let command_cwd = tmp.path().join("workspace");
        let mixed_case_temp = tmp.path().join("mixed-temp");
        std::fs::create_dir_all(&command_cwd).expect("workspace dir");
        std::fs::create_dir_all(&mixed_case_temp).expect("mixed temp dir");

        let policy = workspace_policy(Vec::new(), false, false);
        let mut env_map = HashMap::new();
        env_map.insert(
            "Temp".to_string(),
            mixed_case_temp.to_string_lossy().to_string(),
        );
        let paths = compute_allow_deny_paths(&policy, &command_cwd, &command_cwd, None, &env_map);

        assert!(
            paths
                .allow
                .contains(&canonicalize_or_identity(&command_cwd))
        );
        assert!(
            paths
                .allow
                .contains(&canonicalize_or_identity(&mixed_case_temp)),
            "expected allow list to include Temp env path"
        );
    }

    #[test]
    fn compute_allow_paths_excludes_tmp_env_vars_when_requested() {
        let tmp = tempdir().expect("tempdir");
        let command_cwd = tmp.path().join("workspace");
        let temp_dir = tmp.path().join("temp");
        std::fs::create_dir_all(&command_cwd).expect("workspace dir");
        std::fs::create_dir_all(&temp_dir).expect("temp dir");

        let policy = workspace_policy(Vec::new(), false, true);
        let mut env_map = HashMap::new();
        env_map.insert("TEMP".to_string(), temp_dir.to_string_lossy().to_string());
        let paths = compute_allow_deny_paths(&policy, &command_cwd, &command_cwd, None, &env_map);

        assert!(
            paths
                .allow
                .contains(&canonicalize_or_identity(&command_cwd))
        );
        assert!(!paths.allow.contains(&canonicalize_or_identity(&temp_dir)));
        assert!(paths.deny.is_empty());
    }

    #[test]
    fn compute_allow_paths_includes_session_temp_dir_when_tmp_env_vars_excluded() {
        let tmp = tempdir().expect("tempdir");
        let command_cwd = tmp.path().join("workspace");
        let temp_dir = tmp.path().join("temp");
        let session_temp_dir = tmp.path().join("session-temp");
        std::fs::create_dir_all(&command_cwd).expect("workspace dir");
        std::fs::create_dir_all(&temp_dir).expect("temp dir");
        std::fs::create_dir_all(&session_temp_dir).expect("session temp dir");

        let policy = workspace_policy(Vec::new(), false, true);
        let mut env_map = HashMap::new();
        env_map.insert("TEMP".to_string(), temp_dir.to_string_lossy().to_string());
        let paths = compute_allow_deny_paths(
            &policy,
            &command_cwd,
            &command_cwd,
            Some(&session_temp_dir),
            &env_map,
        );

        assert!(
            paths
                .allow
                .contains(&canonicalize_or_identity(&command_cwd))
        );
        assert!(!paths.allow.contains(&canonicalize_or_identity(&temp_dir)));
        assert!(
            paths
                .allow
                .contains(&canonicalize_or_identity(&session_temp_dir))
        );
    }

    #[test]
    fn compute_allow_paths_for_read_only_includes_only_session_temp_dir() {
        let tmp = tempdir().expect("tempdir");
        let command_cwd = tmp.path().join("workspace");
        let temp_dir = tmp.path().join("temp");
        let session_temp_dir = tmp.path().join("session-temp");
        std::fs::create_dir_all(&command_cwd).expect("workspace dir");
        std::fs::create_dir_all(&temp_dir).expect("temp dir");
        std::fs::create_dir_all(&session_temp_dir).expect("session temp dir");

        let mut env_map = HashMap::new();
        env_map.insert("TEMP".to_string(), temp_dir.to_string_lossy().to_string());
        let paths = compute_allow_deny_paths(
            &SandboxPolicy::ReadOnly,
            &command_cwd,
            &command_cwd,
            Some(&session_temp_dir),
            &env_map,
        );

        assert_eq!(paths.allow.len(), 1);
        assert!(
            paths
                .allow
                .contains(&canonicalize_or_identity(&session_temp_dir))
        );
        assert!(paths.deny.is_empty());
    }

    #[test]
    fn stable_capability_sid_is_deterministic_for_workspace_and_policy() {
        let tmp = tempdir().expect("tempdir");
        let cwd = tmp.path().join("workspace");
        std::fs::create_dir_all(&cwd).expect("workspace dir");

        let first = stable_cap_sid_string(&workspace_policy(Vec::new(), false, false), &cwd);
        let second = stable_cap_sid_string(&workspace_policy(Vec::new(), false, false), &cwd);

        assert_eq!(first, second);
    }

    #[test]
    fn prepared_launch_uses_distinct_launch_sid() {
        let tmp = tempdir().expect("tempdir");
        let cwd = tmp.path().join("workspace");
        let session_temp_dir = tmp.path().join("session-temp");
        std::fs::create_dir_all(&cwd).expect("workspace dir");
        std::fs::create_dir_all(&session_temp_dir).expect("session temp dir");

        let expected = stable_cap_sid_string(&workspace_policy(Vec::new(), false, false), &cwd);
        let resolved = resolve_launch_capability_sids(
            &workspace_policy(Vec::new(), false, false),
            &cwd,
            Some(&expected),
        )
        .expect("prepared launch SIDs");

        assert_eq!(resolved.filesystem_sid, expected);
        assert_ne!(resolved.launch_sid, resolved.filesystem_sid);
    }

    #[test]
    fn prepared_launch_sid_does_not_require_session_temp_metadata() {
        let tmp = tempdir().expect("tempdir");
        let cwd = tmp.path().join("workspace");
        std::fs::create_dir_all(&cwd).expect("workspace dir");
        let expected = stable_cap_sid_string(&workspace_policy(Vec::new(), false, false), &cwd);

        let result = resolve_launch_capability_sids(
            &workspace_policy(Vec::new(), false, false),
            &cwd,
            Some(&expected),
        );

        assert!(
            result.is_ok(),
            "prepared capability SID should not depend on session temp metadata"
        );
    }

    #[test]
    fn stable_capability_sid_changes_with_policy_or_workspace() {
        let tmp = tempdir().expect("tempdir");
        let cwd_a = tmp.path().join("workspace-a");
        let cwd_b = tmp.path().join("workspace-b");
        std::fs::create_dir_all(&cwd_a).expect("workspace a dir");
        std::fs::create_dir_all(&cwd_b).expect("workspace b dir");

        let workspace_a =
            stable_cap_sid_string(&workspace_policy(Vec::new(), false, false), &cwd_a);
        let workspace_b =
            stable_cap_sid_string(&workspace_policy(Vec::new(), false, false), &cwd_b);
        let readonly_a = stable_cap_sid_string(&SandboxPolicy::ReadOnly, &cwd_a);

        assert_ne!(workspace_a, workspace_b);
        assert_ne!(workspace_a, readonly_a);
    }

    #[test]
    fn stable_capability_sid_ignores_session_temp_dir() {
        let workspace = prepared_launch_workspace_tempdir();
        let session_root = tempdir().expect("session temp root");
        let cwd = workspace.path().join("workspace");
        let session_temp_a = session_root.path().join("session-temp-a");
        let session_temp_b = session_root.path().join("session-temp-b");
        std::fs::create_dir_all(&cwd).expect("workspace dir");
        crate::sandbox::prepare_session_temp_dir(&session_temp_a).expect("session temp a dir");
        crate::sandbox::prepare_session_temp_dir(&session_temp_b).expect("session temp b dir");

        let first = prepare_sandbox_launch(
            &workspace_policy(Vec::new(), false, false),
            &cwd,
            &session_temp_a,
        )
        .expect("prepare launch a");
        let second = prepare_sandbox_launch(
            &workspace_policy(Vec::new(), false, false),
            &cwd,
            &session_temp_b,
        )
        .expect("prepare launch b");

        assert_eq!(
            first.capability_sid(),
            second.capability_sid(),
            "stable filesystem SID should be reused across per-session temp dirs"
        );
    }

    #[test]
    fn prepared_launch_workspace_tempdir_avoids_system_temp_root() {
        let workspace = prepared_launch_workspace_tempdir();
        assert!(
            !workspace.path().starts_with(std::env::temp_dir()),
            "prepared-launch workspace tests should avoid system temp roots so ACL setup does not depend on TEMP DACLs"
        );
    }

    #[test]
    fn prepared_launch_does_not_share_session_temp_dir_access() {
        let workspace = prepared_launch_workspace_tempdir();
        let session_root = tempdir().expect("session temp root");
        let cwd = workspace.path().join("workspace");
        let session_temp_a = session_root.path().join("session-temp-a");
        let session_temp_b = session_root.path().join("session-temp-b");
        std::fs::create_dir_all(&cwd).expect("workspace dir");
        crate::sandbox::prepare_session_temp_dir(&session_temp_a).expect("session temp a dir");
        crate::sandbox::prepare_session_temp_dir(&session_temp_b).expect("session temp b dir");

        let policy = workspace_policy(Vec::new(), false, false);
        let prepared_a =
            prepare_sandbox_launch(&policy, &cwd, &session_temp_a).expect("prepare launch a");
        let prepared_b =
            prepare_sandbox_launch(&policy, &cwd, &session_temp_b).expect("prepare launch b");

        unsafe {
            let sid_a = convert_string_sid_to_sid(prepared_a.capability_sid())
                .expect("capability SID should convert");
            assert!(
                !path_has_allow_ace(&session_temp_b, sid_a),
                "prepared capability SID should not grant access to a different session temp dir"
            );
            LocalFree(sid_a as HLOCAL);
            revoke_capability_sid_paths(
                prepared_b.capability_sid(),
                &[
                    cwd.as_path(),
                    session_temp_a.as_path(),
                    session_temp_b.as_path(),
                ],
            );
        }
    }

    #[test]
    fn stable_capability_sid_changes_when_workspace_write_policy_changes() {
        let tmp = tempdir().expect("tempdir");
        let cwd = tmp.path().join("workspace");
        let extra_root = tmp.path().join("extra");
        std::fs::create_dir_all(&cwd).expect("workspace dir");
        std::fs::create_dir_all(&extra_root).expect("extra dir");

        let base = stable_cap_sid_string(&workspace_policy(Vec::new(), false, false), &cwd);
        let with_root =
            stable_cap_sid_string(&workspace_policy(vec![extra_root], false, false), &cwd);
        let with_network = stable_cap_sid_string(&workspace_policy(Vec::new(), true, false), &cwd);
        let with_tmp_exclusion =
            stable_cap_sid_string(&workspace_policy(Vec::new(), false, true), &cwd);

        assert_ne!(base, with_root);
        assert_ne!(base, with_network);
        assert_ne!(base, with_tmp_exclusion);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn stable_capability_sid_changes_when_relative_writable_root_target_changes() {
        let tmp = tempdir().expect("tempdir");
        let cwd = tmp.path().join("workspace");
        let target_a = tmp.path().join("target-a");
        let target_b = tmp.path().join("target-b");
        let linked_root = cwd.join("linked-root");
        std::fs::create_dir_all(&cwd).expect("workspace dir");
        std::fs::create_dir_all(&target_a).expect("target a dir");
        std::fs::create_dir_all(&target_b).expect("target b dir");

        create_junction(&linked_root, &target_a);
        let policy = workspace_policy(vec![PathBuf::from("linked-root")], false, false);
        let first = stable_cap_sid_string(&policy, &cwd);

        remove_junction(&linked_root);
        create_junction(&linked_root, &target_b);
        let second = stable_cap_sid_string(&policy, &cwd);

        remove_junction(&linked_root);

        assert_ne!(
            first, second,
            "stable SID should change when a relative writable root resolves to a different target"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn prepared_launch_matches_rejects_relative_writable_root_target_changes() {
        let tmp = tempdir().expect("tempdir");
        let cwd = tmp.path().join("workspace");
        let session_temp_dir = tmp.path().join("session-temp");
        let target_a = tmp.path().join("target-a");
        let target_b = tmp.path().join("target-b");
        let linked_root = cwd.join("linked-root");
        std::fs::create_dir_all(&cwd).expect("workspace dir");
        std::fs::create_dir_all(&session_temp_dir).expect("session temp dir");
        std::fs::create_dir_all(&target_a).expect("target a dir");
        std::fs::create_dir_all(&target_b).expect("target b dir");

        create_junction(&linked_root, &target_a);
        let policy = workspace_policy(vec![PathBuf::from("linked-root")], false, false);
        let launch = PreparedSandboxLaunch {
            policy: policy.clone(),
            sandbox_policy_cwd: canonicalize_or_identity(&cwd),
            session_temp_dir: canonicalize_or_identity(&session_temp_dir),
            capability_sid: stable_cap_sid_string(&policy, &cwd),
        };
        assert!(
            launch.matches(&policy, &cwd, &session_temp_dir),
            "launch should match before the relative writable root target changes"
        );

        remove_junction(&linked_root);
        create_junction(&linked_root, &target_b);

        assert!(
            !launch.matches(&policy, &cwd, &session_temp_dir),
            "launch should stop matching once a relative writable root resolves elsewhere"
        );

        remove_junction(&linked_root);
    }

    #[test]
    fn prepared_capability_sid_survives_late_created_absolute_writable_root() {
        let tmp = tempdir().expect("tempdir");
        let cwd = tmp.path().join("workspace");
        let late_root = tmp.path().join("late-root");
        std::fs::create_dir_all(&cwd).expect("workspace dir");

        let policy = workspace_policy(vec![late_root.clone()], false, false);
        let prepared_sid = stable_cap_sid_string(&policy, &cwd);

        std::fs::create_dir_all(&late_root).expect("late root dir");

        let resolved = resolve_launch_capability_sids(&policy, &cwd, Some(&prepared_sid));
        assert!(
            resolved.is_ok(),
            "prepared SID should remain valid after a declared absolute writable root is created"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn prepared_capability_sid_survives_late_created_absolute_writable_root_with_case_change() {
        let tmp = tempdir().expect("tempdir");
        let cwd = tmp.path().join("workspace");
        std::fs::create_dir_all(&cwd).expect("workspace dir");

        let late_root = PathBuf::from(
            tmp.path()
                .join("late-root")
                .to_string_lossy()
                .to_ascii_lowercase(),
        );
        let policy = workspace_policy(vec![late_root.clone()], false, false);
        let prepared_sid = stable_cap_sid_string(&policy, &cwd);

        std::fs::create_dir_all(&late_root).expect("late root dir");

        let resolved = resolve_launch_capability_sids(&policy, &cwd, Some(&prepared_sid));
        assert!(
            resolved.is_ok(),
            "prepared SID should remain valid after case-only path normalization changes"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn dropping_wrapper_child_stdio_closes_all_pipe_handles() {
        fn write_end_reports_closed_peer(mut probe: File, label: &str) {
            let err = probe
                .write_all(b"x")
                .expect_err("write probe should fail once the pipe peer is dropped");
            let raw = err.raw_os_error().map(|code| code as u32);
            assert!(
                err.kind() == io::ErrorKind::BrokenPipe
                    || matches!(raw, Some(ERROR_BROKEN_PIPE | ERROR_NO_DATA)),
                "{label} should observe a broken pipe after dropping wrapper stdio, got: {err}"
            );
        }

        unsafe fn read_end_reports_closed_peer(probe: File, label: &str) {
            let ok = PeekNamedPipe(
                probe.as_raw_handle() as HANDLE,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            let err = GetLastError();
            assert!(
                ok == 0 && matches!(err, ERROR_BROKEN_PIPE | ERROR_NO_DATA),
                "{label} should observe a disconnected pipe after dropping wrapper stdio, got ok={ok} err={err}"
            );
        }

        {
            let stdio =
                unsafe { create_wrapper_child_stdio() }.expect("create wrapper child stdio");
            let stdin_write_probe = stdio.stdin_write.try_clone().expect("clone stdin write");
            drop(stdio);
            write_end_reports_closed_peer(stdin_write_probe, "stdin write handle");
        }
        {
            let stdio =
                unsafe { create_wrapper_child_stdio() }.expect("create wrapper child stdio");
            let child_stdin_probe = stdio.child_stdin.try_clone().expect("clone child stdin");
            drop(stdio);
            unsafe {
                read_end_reports_closed_peer(child_stdin_probe, "child stdin handle");
            }
        }
        {
            let stdio =
                unsafe { create_wrapper_child_stdio() }.expect("create wrapper child stdio");
            let stdout_read_probe = stdio.stdout_read.try_clone().expect("clone stdout read");
            drop(stdio);
            unsafe {
                read_end_reports_closed_peer(stdout_read_probe, "stdout read handle");
            }
        }
        {
            let stdio =
                unsafe { create_wrapper_child_stdio() }.expect("create wrapper child stdio");
            let child_stdout_probe = stdio.child_stdout.try_clone().expect("clone child stdout");
            drop(stdio);
            write_end_reports_closed_peer(child_stdout_probe, "child stdout handle");
        }
        {
            let stdio =
                unsafe { create_wrapper_child_stdio() }.expect("create wrapper child stdio");
            let stderr_read_probe = stdio.stderr_read.try_clone().expect("clone stderr read");
            drop(stdio);
            unsafe {
                read_end_reports_closed_peer(stderr_read_probe, "stderr read handle");
            }
        }
        {
            let stdio =
                unsafe { create_wrapper_child_stdio() }.expect("create wrapper child stdio");
            let child_stderr_probe = stdio.child_stderr.try_clone().expect("clone child stderr");
            drop(stdio);
            write_end_reports_closed_peer(child_stderr_probe, "child stderr handle");
        }
    }

    #[test]
    fn prepared_launch_tempdir_can_be_refreshed_after_reset() {
        let workspace = prepared_launch_workspace_tempdir();
        let session_root = tempdir().expect("session temp root");
        let cwd = workspace.path().join("workspace");
        let session_temp_dir = session_root.path().join("session-temp");
        std::fs::create_dir_all(&cwd).expect("workspace dir");
        crate::sandbox::prepare_session_temp_dir(&session_temp_dir)
            .expect("prepare session temp dir");

        let policy = workspace_policy(Vec::new(), false, false);
        let prepared =
            prepare_sandbox_launch(&policy, &cwd, &session_temp_dir).expect("prepare launch");
        crate::sandbox::prepare_session_temp_dir(&session_temp_dir)
            .expect("reset session temp dir");
        refresh_prepared_sandbox_launch_acl_state(&prepared)
            .expect("refresh prepared launch ACL state");

        let probe = session_temp_dir.join("probe.txt");
        let command = vec![
            "powershell.exe".to_string(),
            "-NoProfile".to_string(),
            "-Command".to_string(),
            format!("Set-Content -LiteralPath '{}' -Value 'OK'", probe.display()),
        ];

        let mut env_map = std::env::vars().collect::<HashMap<_, _>>();
        env_map.extend(sandbox_acl_env_map(&session_temp_dir));
        let run_result = run_sandboxed_command_with_env_map(
            &policy,
            &cwd,
            &command,
            Some(prepared.capability_sid()),
            env_map,
        );

        unsafe {
            let sid = convert_string_sid_to_sid(prepared.capability_sid())
                .expect("stable capability SID should convert");
            revoke_ace(&cwd, sid);
            revoke_ace(&session_temp_dir, sid);
            LocalFree(sid as HLOCAL);
        }

        let status = run_result.expect("sandboxed command should run");
        assert_eq!(status, 0);
        assert!(
            probe.is_file(),
            "expected sandboxed command to recreate probe file after tempdir reset"
        );
    }

    #[test]
    fn prepared_launch_default_dacl_uses_only_launch_sid() {
        let capability_sids = LaunchCapabilitySids {
            filesystem_sid: "S-1-5-21-1-2-3-4".to_string(),
            launch_sid: "S-1-5-21-5-6-7-8".to_string(),
        };

        assert_eq!(
            default_dacl_capability_sid_strings(&capability_sids),
            vec!["S-1-5-21-5-6-7-8"],
            "prepared launches should keep the shared filesystem SID out of the token default DACL",
        );
    }

    #[test]
    fn inline_launch_default_dacl_keeps_single_capability_sid() {
        let capability_sids = LaunchCapabilitySids {
            filesystem_sid: "S-1-5-21-1-2-3-4".to_string(),
            launch_sid: "S-1-5-21-1-2-3-4".to_string(),
        };

        assert_eq!(
            default_dacl_capability_sid_strings(&capability_sids),
            vec!["S-1-5-21-1-2-3-4"],
            "inline launches should keep using their single capability SID in the default DACL",
        );
    }

    #[test]
    fn prepared_launch_acl_lock_blocks_same_sid_until_release() {
        let first = acquire_prepared_launch_acl_lock("S-1-5-21-1-2-3-4")
            .expect("first prepared launch ACL lock");
        let (acquired_tx, acquired_rx) = std::sync::mpsc::channel();

        let waiter = thread::spawn(move || {
            let second = acquire_prepared_launch_acl_lock("S-1-5-21-1-2-3-4")
                .expect("second prepared launch ACL lock");
            acquired_tx
                .send(())
                .expect("lock acquisition should be reported");
            drop(second);
        });

        assert!(
            acquired_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "same-SID prepared launch ACL work should serialize behind the first lock holder"
        );

        drop(first);
        acquired_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("waiter should acquire once the first lock is released");
        waiter.join().expect("waiter should join");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn prepared_capability_sid_falls_back_for_restricted_base_token() {
        let result = normalize_prepared_capability_sid(Some("S-1-5-21-1-2-3-4"), true);
        assert!(
            result.is_none(),
            "expected restricted-token prepared launch reuse to fall back to inline ACL prep, got: {result:?}"
        );
    }

    #[test]
    fn compute_allow_paths_denies_git_dir_inside_writable_root() {
        let tmp = tempdir().expect("tempdir");
        let command_cwd = tmp.path().join("workspace");
        let extra_root = tmp.path().join("extra");
        let git_dir = extra_root.join(".git");
        std::fs::create_dir_all(&command_cwd).expect("workspace dir");
        std::fs::create_dir_all(&git_dir).expect("git dir");

        let policy = workspace_policy(vec![extra_root.clone()], false, true);
        let paths =
            compute_allow_deny_paths(&policy, &command_cwd, &command_cwd, None, &HashMap::new());

        assert!(
            paths
                .allow
                .contains(&canonicalize_or_identity(&command_cwd))
        );
        assert!(paths.allow.contains(&canonicalize_or_identity(&extra_root)));
        assert!(paths.deny.contains(&canonicalize_or_identity(&git_dir)));
    }

    #[test]
    fn compute_allow_paths_denies_codex_and_agents_dirs_inside_writable_root() {
        let tmp = tempdir().expect("tempdir");
        let command_cwd = tmp.path().join("workspace");
        let codex_dir = command_cwd.join(".codex");
        let agents_dir = command_cwd.join(".agents");
        std::fs::create_dir_all(&codex_dir).expect("codex dir");
        std::fs::create_dir_all(&agents_dir).expect("agents dir");

        let policy = workspace_policy(Vec::new(), false, true);
        let paths =
            compute_allow_deny_paths(&policy, &command_cwd, &command_cwd, None, &HashMap::new());

        assert_eq!(paths.allow.len(), 1);
        assert!(
            paths
                .allow
                .contains(&canonicalize_or_identity(&command_cwd))
        );
        assert!(
            paths.deny.contains(&canonicalize_or_identity(&codex_dir)),
            "expected deny list to include .codex"
        );
        assert!(
            paths.deny.contains(&canonicalize_or_identity(&agents_dir)),
            "expected deny list to include .agents"
        );
    }

    #[test]
    fn compute_allow_paths_denies_git_file_inside_writable_root() {
        let tmp = tempdir().expect("tempdir");
        let command_cwd = tmp.path().join("workspace");
        let extra_root = tmp.path().join("extra");
        let git_file = extra_root.join(".git");
        std::fs::create_dir_all(&command_cwd).expect("workspace dir");
        std::fs::create_dir_all(&extra_root).expect("extra dir");
        std::fs::write(&git_file, "gitdir: .git/worktrees/example").expect("git file");

        let policy = workspace_policy(vec![extra_root.clone()], false, true);
        let paths =
            compute_allow_deny_paths(&policy, &command_cwd, &command_cwd, None, &HashMap::new());

        assert!(
            paths
                .allow
                .contains(&canonicalize_or_identity(&command_cwd))
        );
        assert!(paths.allow.contains(&canonicalize_or_identity(&extra_root)));
        assert!(paths.deny.contains(&canonicalize_or_identity(&git_file)));
    }

    #[test]
    fn compute_allow_paths_skips_git_dir_when_missing() {
        let tmp = tempdir().expect("tempdir");
        let command_cwd = tmp.path().join("workspace");
        let extra_root = tmp.path().join("extra");
        std::fs::create_dir_all(&command_cwd).expect("workspace dir");
        std::fs::create_dir_all(&extra_root).expect("extra dir");

        let policy = workspace_policy(vec![extra_root.clone()], false, true);
        let paths =
            compute_allow_deny_paths(&policy, &command_cwd, &command_cwd, None, &HashMap::new());

        assert_eq!(paths.allow.len(), 2);
        assert!(
            paths
                .allow
                .contains(&canonicalize_or_identity(&command_cwd))
        );
        assert!(paths.allow.contains(&canonicalize_or_identity(&extra_root)));
        assert!(paths.deny.is_empty());
    }

    #[test]
    fn compute_allow_paths_includes_command_cwd_and_denies_its_git_dir() {
        let tmp = tempdir().expect("tempdir");
        let command_cwd = tmp.path().join("workspace");
        let git_dir = command_cwd.join(".git");
        std::fs::create_dir_all(&git_dir).expect("git dir");

        let policy = workspace_policy(Vec::new(), false, true);
        let paths =
            compute_allow_deny_paths(&policy, &command_cwd, &command_cwd, None, &HashMap::new());

        assert_eq!(paths.allow.len(), 1);
        assert!(
            paths
                .allow
                .contains(&canonicalize_or_identity(&command_cwd))
        );
        assert!(paths.deny.contains(&canonicalize_or_identity(&git_dir)));
    }

    #[test]
    fn compute_allow_paths_keeps_nonexistent_declared_writable_root() {
        let tmp = tempdir().expect("tempdir");
        let command_cwd = tmp.path().join("workspace");
        let missing_root = tmp.path().join("missing-root");
        std::fs::create_dir_all(&command_cwd).expect("workspace dir");

        let policy = workspace_policy(vec![missing_root.clone()], false, true);
        let paths =
            compute_allow_deny_paths(&policy, &command_cwd, &command_cwd, None, &HashMap::new());

        assert!(
            paths
                .allow
                .contains(&canonicalize_or_identity(&command_cwd))
        );
        assert!(
            paths
                .allow
                .contains(&canonicalize_or_identity(&missing_root)),
            "declared writable root should remain allowed even before it exists"
        );
    }

    unsafe fn path_has_allow_ace(path: &Path, sid: *mut c_void) -> bool {
        let mut security_descriptor: *mut c_void = std::ptr::null_mut();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let code = GetNamedSecurityInfoW(
            to_wide(path).as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut security_descriptor,
        );
        if code != ERROR_SUCCESS {
            return false;
        }
        let has_ace = dacl_has_allow_for_sid(dacl, sid, path.is_dir());
        if !security_descriptor.is_null() {
            LocalFree(security_descriptor as HLOCAL);
        }
        has_ace
    }

    unsafe fn path_has_inheritable_allow_ace(path: &Path, sid: *mut c_void) -> bool {
        let mut security_descriptor: *mut c_void = std::ptr::null_mut();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let code = GetNamedSecurityInfoW(
            to_wide(path).as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut security_descriptor,
        );
        if code != ERROR_SUCCESS {
            return false;
        }

        let mut has_ace = false;
        if let Some(info) = dacl_size_info(dacl) {
            for index in 0..info.AceCount {
                let mut ace: *mut c_void = std::ptr::null_mut();
                if GetAce(dacl as *const ACL, index, &mut ace) == 0 {
                    continue;
                }
                let header = &*(ace as *const ACE_HEADER);
                if header.AceType != 0 || (header.AceFlags & INHERIT_ONLY_ACE) != 0 {
                    continue;
                }
                let allowed = &*(ace as *const ACCESS_ALLOWED_ACE);
                if EqualSid(ace_sid_ptr(ace), sid) != 0
                    && allow_ace_satisfies_requirements(allowed.Mask, header.AceFlags, true)
                {
                    has_ace = true;
                    break;
                }
            }
        }

        if !security_descriptor.is_null() {
            LocalFree(security_descriptor as HLOCAL);
        }
        has_ace
    }

    unsafe fn path_has_write_deny_ace(path: &Path, sid: *mut c_void) -> bool {
        let mut security_descriptor: *mut c_void = std::ptr::null_mut();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let code = GetNamedSecurityInfoW(
            to_wide(path).as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut security_descriptor,
        );
        if code != ERROR_SUCCESS {
            return false;
        }
        let has_ace = dacl_has_write_deny_for_sid(dacl, sid);
        if !security_descriptor.is_null() {
            LocalFree(security_descriptor as HLOCAL);
        }
        has_ace
    }

    fn expected_write_deny_mask() -> u32 {
        FILE_GENERIC_WRITE
            | FILE_WRITE_DATA
            | FILE_APPEND_DATA
            | FILE_WRITE_EA
            | FILE_WRITE_ATTRIBUTES
            | DELETE
            | FILE_DELETE_CHILD
    }

    unsafe fn path_write_deny_mask(path: &Path, sid: *mut c_void) -> u32 {
        let mut security_descriptor: *mut c_void = std::ptr::null_mut();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let code = GetNamedSecurityInfoW(
            to_wide(path).as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut security_descriptor,
        );
        if code != ERROR_SUCCESS {
            return 0;
        }

        let mut mask = 0;
        if let Some(info) = dacl_size_info(dacl) {
            for index in 0..info.AceCount {
                let mut ace: *mut c_void = std::ptr::null_mut();
                if GetAce(dacl as *const ACL, index, &mut ace) == 0 {
                    continue;
                }
                let header = &*(ace as *const ACE_HEADER);
                if header.AceType != 1 || (header.AceFlags & INHERIT_ONLY_ACE) != 0 {
                    continue;
                }
                let denied = &*(ace as *const ACCESS_DENIED_ACE);
                if EqualSid(ace_sid_ptr(ace), sid) != 0 {
                    mask |= denied.Mask;
                }
            }
        }

        if !security_descriptor.is_null() {
            LocalFree(security_descriptor as HLOCAL);
        }
        mask
    }

    unsafe fn add_custom_allow_ace(
        path: &Path,
        sid: *mut c_void,
        mask: u32,
        inheritance: u32,
    ) -> Result<(), String> {
        let mut security_descriptor: *mut c_void = std::ptr::null_mut();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let code = GetNamedSecurityInfoW(
            to_wide(path).as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut security_descriptor,
        );
        if code != ERROR_SUCCESS {
            return Err(format!("GetNamedSecurityInfoW failed: {code}"));
        }

        let trustee = TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid as *mut u16,
        };
        let mut explicit: EXPLICIT_ACCESS_W = std::mem::zeroed();
        explicit.grfAccessPermissions = mask;
        explicit.grfAccessMode = GRANT_ACCESS;
        explicit.grfInheritance = inheritance;
        explicit.Trustee = trustee;

        let mut new_dacl: *mut ACL = std::ptr::null_mut();
        let set_acl_code = SetEntriesInAclW(1, &explicit, dacl, &mut new_dacl);
        if set_acl_code != ERROR_SUCCESS {
            if !security_descriptor.is_null() {
                LocalFree(security_descriptor as HLOCAL);
            }
            return Err(format!("SetEntriesInAclW failed: {set_acl_code}"));
        }

        let set_security_code = SetNamedSecurityInfoW(
            to_wide(path).as_ptr() as *mut u16,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            new_dacl,
            std::ptr::null_mut(),
        );
        if !new_dacl.is_null() {
            LocalFree(new_dacl as HLOCAL);
        }
        if !security_descriptor.is_null() {
            LocalFree(security_descriptor as HLOCAL);
        }
        if set_security_code != ERROR_SUCCESS {
            return Err(format!("SetNamedSecurityInfoW failed: {set_security_code}"));
        }
        Ok(())
    }

    unsafe fn add_custom_deny_ace(path: &Path, sid: *mut c_void, mask: u32) -> Result<(), String> {
        let mut security_descriptor: *mut c_void = std::ptr::null_mut();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let code = GetNamedSecurityInfoW(
            to_wide(path).as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut security_descriptor,
        );
        if code != ERROR_SUCCESS {
            return Err(format!("GetNamedSecurityInfoW failed: {code}"));
        }

        let trustee = TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid as *mut u16,
        };
        let mut explicit: EXPLICIT_ACCESS_W = std::mem::zeroed();
        explicit.grfAccessPermissions = mask;
        explicit.grfAccessMode = DENY_ACCESS;
        explicit.grfInheritance = CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE;
        explicit.Trustee = trustee;

        let mut new_dacl: *mut ACL = std::ptr::null_mut();
        let set_acl_code = SetEntriesInAclW(1, &explicit, dacl, &mut new_dacl);
        if set_acl_code != ERROR_SUCCESS {
            if !security_descriptor.is_null() {
                LocalFree(security_descriptor as HLOCAL);
            }
            return Err(format!("SetEntriesInAclW failed: {set_acl_code}"));
        }

        let set_security_code = SetNamedSecurityInfoW(
            to_wide(path).as_ptr() as *mut u16,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            new_dacl,
            std::ptr::null_mut(),
        );
        if !new_dacl.is_null() {
            LocalFree(new_dacl as HLOCAL);
        }
        if !security_descriptor.is_null() {
            LocalFree(security_descriptor as HLOCAL);
        }
        if set_security_code != ERROR_SUCCESS {
            return Err(format!("SetNamedSecurityInfoW failed: {set_security_code}"));
        }
        Ok(())
    }

    unsafe fn revoke_capability_sid_paths(capability_sid: &str, paths: &[&Path]) {
        let sid = convert_string_sid_to_sid(capability_sid).expect("capability SID should convert");
        for path in paths {
            revoke_ace(path, sid);
        }
        LocalFree(sid as HLOCAL);
    }

    #[test]
    fn prepared_launch_refresh_reapplies_allow_acl_to_recreated_writable_root() {
        let workspace = prepared_launch_workspace_tempdir();
        let session_root = tempdir().expect("session temp root");
        let cwd = workspace.path().join("workspace");
        let extra_root = workspace.path().join("extra");
        let session_temp_dir = session_root.path().join("session-temp");
        std::fs::create_dir_all(&cwd).expect("workspace dir");
        std::fs::create_dir_all(&extra_root).expect("extra dir");
        crate::sandbox::prepare_session_temp_dir(&session_temp_dir)
            .expect("prepare session temp dir");

        let policy = workspace_policy(vec![extra_root.clone()], false, false);
        let prepared =
            prepare_sandbox_launch(&policy, &cwd, &session_temp_dir).expect("prepare launch");

        std::fs::remove_dir_all(&extra_root).expect("remove extra root");
        std::fs::create_dir_all(&extra_root).expect("recreate extra root");

        refresh_prepared_sandbox_launch_acl_state(&prepared)
            .expect("refresh prepared launch ACL state");

        unsafe {
            let sid = convert_string_sid_to_sid(prepared.capability_sid())
                .expect("capability SID should convert");
            assert!(
                path_has_allow_ace(&extra_root, sid),
                "refresh should restore allow ACEs on recreated writable roots"
            );
            LocalFree(sid as HLOCAL);
            revoke_capability_sid_paths(
                prepared.capability_sid(),
                &[
                    cwd.as_path(),
                    extra_root.as_path(),
                    session_temp_dir.as_path(),
                ],
            );
        }
    }

    #[test]
    fn prepared_launch_prepare_applies_allow_acl_to_missing_writable_root() {
        let workspace = prepared_launch_workspace_tempdir();
        let session_root = tempdir().expect("session temp root");
        let cwd = workspace.path().join("workspace");
        let missing_root = workspace.path().join("missing-root");
        let session_temp_dir = session_root.path().join("session-temp");
        std::fs::create_dir_all(&cwd).expect("workspace dir");
        crate::sandbox::prepare_session_temp_dir(&session_temp_dir)
            .expect("prepare session temp dir");

        let policy = workspace_policy(vec![missing_root.clone()], false, false);
        let prepared =
            prepare_sandbox_launch(&policy, &cwd, &session_temp_dir).expect("prepare launch");

        unsafe {
            let sid = convert_string_sid_to_sid(prepared.capability_sid())
                .expect("capability SID should convert");
            assert!(
                missing_root.is_dir(),
                "prepare should create missing writable roots"
            );
            assert!(
                path_has_allow_ace(&missing_root, sid),
                "prepare should apply allow ACEs to writable roots declared before they exist"
            );
            LocalFree(sid as HLOCAL);
            revoke_capability_sid_paths(
                prepared.capability_sid(),
                &[
                    cwd.as_path(),
                    missing_root.as_path(),
                    session_temp_dir.as_path(),
                ],
            );
        }
    }

    #[test]
    fn prepared_launch_refresh_applies_deny_acl_to_late_created_protected_dir() {
        let workspace = prepared_launch_workspace_tempdir();
        let session_root = tempdir().expect("session temp root");
        let cwd = workspace.path().join("workspace");
        let git_dir = cwd.join(".git");
        let session_temp_dir = session_root.path().join("session-temp");
        std::fs::create_dir_all(&cwd).expect("workspace dir");
        crate::sandbox::prepare_session_temp_dir(&session_temp_dir)
            .expect("prepare session temp dir");

        let policy = workspace_policy(Vec::new(), false, false);
        let prepared =
            prepare_sandbox_launch(&policy, &cwd, &session_temp_dir).expect("prepare launch");

        std::fs::create_dir_all(&git_dir).expect("create git dir after prepare");

        refresh_prepared_sandbox_launch_acl_state(&prepared)
            .expect("refresh prepared launch ACL state");

        unsafe {
            let sid = convert_string_sid_to_sid(prepared.capability_sid())
                .expect("capability SID should convert");
            assert!(
                path_has_write_deny_ace(&git_dir, sid),
                "refresh should apply deny ACEs to protected directories created after prepare"
            );
            LocalFree(sid as HLOCAL);
            revoke_capability_sid_paths(
                prepared.capability_sid(),
                &[cwd.as_path(), git_dir.as_path(), session_temp_dir.as_path()],
            );
        }
    }

    #[test]
    fn add_deny_write_ace_upgrades_partial_deny_acl_for_same_sid() {
        let tmp = tempdir().expect("tempdir");
        let protected_dir = tmp.path().join("protected");
        std::fs::create_dir_all(&protected_dir).expect("protected dir");

        unsafe {
            let sid = convert_string_sid_to_sid("S-1-5-21-1-2-3-4")
                .expect("capability SID should convert");
            let partial_mask = FILE_WRITE_DATA | DELETE;
            add_custom_deny_ace(&protected_dir, sid, partial_mask)
                .expect("partial deny ACE should be installed");
            assert_eq!(
                path_write_deny_mask(&protected_dir, sid) & expected_write_deny_mask(),
                partial_mask,
                "test setup should start with only a partial deny mask"
            );

            let added =
                add_deny_write_ace(&protected_dir, sid).expect("full deny ACE should be applied");

            assert!(
                added,
                "partial deny ACEs should be treated as incomplete and upgraded"
            );
            assert_eq!(
                path_write_deny_mask(&protected_dir, sid) & expected_write_deny_mask(),
                expected_write_deny_mask(),
                "full sandbox write deny mask should be present after upgrade"
            );

            revoke_deny_write_ace(&protected_dir, sid);
            LocalFree(sid as HLOCAL);
        }
    }

    #[test]
    fn add_allow_ace_upgrades_non_inheriting_allow_acl_for_same_sid() {
        let tmp = tempdir().expect("tempdir");
        let writable_dir = tmp.path().join("writable");
        std::fs::create_dir_all(&writable_dir).expect("writable dir");

        unsafe {
            let sid = convert_string_sid_to_sid("S-1-5-21-1-2-3-4")
                .expect("capability SID should convert");
            add_custom_allow_ace(
                &writable_dir,
                sid,
                FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE,
                0,
            )
            .expect("non-inheriting allow ACE should be installed");
            assert!(
                !path_has_inheritable_allow_ace(&writable_dir, sid),
                "test setup should start with a same-SID allow ACE that does not inherit"
            );

            let added =
                add_allow_ace(&writable_dir, sid).expect("inheriting allow ACE should be applied");

            assert!(
                added,
                "non-inheriting allow ACEs should be treated as incomplete and upgraded"
            );
            assert!(
                path_has_inheritable_allow_ace(&writable_dir, sid),
                "refreshed writable roots should keep inheriting the capability SID"
            );

            revoke_ace(&writable_dir, sid);
            LocalFree(sid as HLOCAL);
        }
    }

    #[test]
    fn prepared_launch_refresh_reapplies_allow_acl_to_existing_file_under_writable_root() {
        let workspace = prepared_launch_workspace_tempdir();
        let session_root = tempdir().expect("session temp root");
        let cwd = workspace.path().join("workspace");
        let nested = cwd.join("nested");
        let session_temp_dir = session_root.path().join("session-temp");
        let source_file = session_temp_dir.join("artifact.txt");
        let moved_file = nested.join("artifact.txt");
        std::fs::create_dir_all(&nested).expect("nested dir");
        crate::sandbox::prepare_session_temp_dir(&session_temp_dir)
            .expect("prepare session temp dir");

        let policy = workspace_policy(Vec::new(), false, false);
        let prepared =
            prepare_sandbox_launch(&policy, &cwd, &session_temp_dir).expect("prepare launch");

        unsafe {
            let filesystem_sid = convert_string_sid_to_sid(prepared.capability_sid())
                .expect("filesystem capability SID should convert");
            let launch_sid_string = make_random_sid_string();
            let launch_sid = convert_string_sid_to_sid(&launch_sid_string)
                .expect("launch capability SID should convert");
            std::fs::write(&source_file, b"artifact").expect("temp artifact file");
            add_allow_ace(&source_file, launch_sid).expect("launch-scoped allow ACE");
            std::fs::rename(&source_file, &moved_file).expect("move artifact into writable root");
            assert!(
                !path_has_allow_ace(&moved_file, filesystem_sid),
                "test setup should keep the temp-file ACL after the move into the writable root"
            );

            refresh_prepared_sandbox_launch_acl_state(&prepared)
                .expect("refresh prepared launch ACL state");

            assert!(
                path_has_allow_ace(&moved_file, filesystem_sid),
                "refresh should restore the stable filesystem ACE on files already under writable roots"
            );

            revoke_ace(&moved_file, launch_sid);
            LocalFree(launch_sid as HLOCAL);
            LocalFree(filesystem_sid as HLOCAL);
            revoke_capability_sid_paths(
                prepared.capability_sid(),
                &[
                    cwd.as_path(),
                    nested.as_path(),
                    moved_file.as_path(),
                    session_temp_dir.as_path(),
                ],
            );
        }
    }

    #[test]
    fn prepared_launch_refresh_rolls_back_added_deny_acls_after_failure() {
        let _guard = prepare_sandbox_launch_test_mutex()
            .lock()
            .expect("windows sandbox test mutex");
        let workspace = prepared_launch_workspace_tempdir();
        let session_root = tempdir().expect("session temp root");
        let cwd = workspace.path().join("workspace");
        let git_dir = cwd.join(".git");
        let codex_dir = cwd.join(".codex");
        let session_temp_dir = session_root.path().join("session-temp");
        std::fs::create_dir_all(&cwd).expect("workspace dir");
        crate::sandbox::prepare_session_temp_dir(&session_temp_dir)
            .expect("prepare session temp dir");

        let policy = workspace_policy(Vec::new(), false, false);
        let prepared =
            prepare_sandbox_launch(&policy, &cwd, &session_temp_dir).expect("prepare launch");

        std::fs::create_dir_all(&git_dir).expect("create git dir after prepare");
        std::fs::create_dir_all(&codex_dir).expect("create codex dir after prepare");
        set_add_deny_write_ace_test_error(Some((1, "injected deny ACL failure".to_string())));

        let result = refresh_prepared_sandbox_launch_acl_state(&prepared);

        set_add_deny_write_ace_test_error(None);

        assert!(
            matches!(result, Err(ref err) if err.contains("injected deny ACL failure")),
            "expected injected deny ACL failure, got: {result:?}"
        );

        unsafe {
            let sid = convert_string_sid_to_sid(prepared.capability_sid())
                .expect("capability SID should convert");
            assert!(
                !path_has_write_deny_ace(&git_dir, sid),
                "failed refresh should roll back deny ACEs added earlier in the same pass"
            );
            assert!(
                !path_has_write_deny_ace(&codex_dir, sid),
                "failed refresh should not leave a deny ACE on the failing path"
            );
            LocalFree(sid as HLOCAL);
        }
    }

    #[test]
    fn prepared_launch_runtime_refreshes_acl_state_before_spawn() {
        let _guard = prepare_sandbox_launch_test_mutex()
            .lock()
            .expect("windows sandbox test mutex");
        let tmp = tempdir().expect("tempdir");
        let cwd = tmp.path().join("workspace");
        let session_temp_dir = tmp.path().join("session-temp");
        std::fs::create_dir_all(&cwd).expect("workspace dir");
        crate::sandbox::prepare_session_temp_dir(&session_temp_dir)
            .expect("prepare session temp dir");

        let policy = workspace_policy(Vec::new(), false, false);
        let prepared_sid = stable_cap_sid_string(&policy, &cwd);
        set_apply_prepared_launch_acl_state_test_error(Some(
            "prepared launch runtime refresh invoked".to_string(),
        ));

        let result = unsafe {
            let sid = convert_string_sid_to_sid(&prepared_sid)
                .expect("prepared capability SID should convert");
            let result = refresh_runtime_prepared_launch_acl_state(
                &policy,
                &cwd,
                &session_temp_dir,
                &prepared_sid,
                sid,
            );
            LocalFree(sid as HLOCAL);
            result
        };

        set_apply_prepared_launch_acl_state_test_error(None);

        assert!(
            matches!(
                result,
                Err(ref err) if err.contains("prepared launch runtime refresh invoked")
            ),
            "prepared runtime should refresh stable ACL state before spawning the child, got: {result:?}"
        );
    }
}
