use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::ffi::CString;
#[cfg(target_os = "windows")]
use std::ffi::OsStr;
#[cfg(target_os = "windows")]
use std::ffi::OsString;
use std::io::Write;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::process;
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicI32, Ordering};
use url::Url;

use serde::{Deserialize, Serialize};
use tempfile::Builder;

pub const SANDBOX_STATE_META_CAPABILITY: &str = "codex/sandbox-state-meta";
pub const MANAGED_ALLOWED_DOMAINS_ENV_KEY: &str = "MCP_REPL_ALLOWED_DOMAINS";
pub const MANAGED_DENIED_DOMAINS_ENV_KEY: &str = "MCP_REPL_DENIED_DOMAINS";
#[cfg(target_os = "macos")]
pub const CODEX_SANDBOX_ENV_VAR: &str = "CODEX_SANDBOX";
pub const CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR: &str = "CODEX_SANDBOX_NETWORK_DISABLED";
pub const R_SESSION_TMPDIR_ENV: &str = "MCP_REPL_R_SESSION_TMPDIR";
#[cfg(target_os = "macos")]
pub const SANDBOX_LOG_DENIALS_ENV: &str = "MCP_REPL_SANDBOX_LOG_DENIALS";
const PROTECTED_METADATA_SUBPATHS: [&str; 3] = [".git", ".agents", ".codex"];
#[cfg(target_os = "linux")]
pub const LINUX_BWRAP_ENABLED_ENV: &str = "MCP_REPL_USE_LINUX_BWRAP";
#[cfg(target_os = "linux")]
pub const LINUX_BWRAP_NO_PROC_ENV: &str = "MCP_REPL_LINUX_BWRAP_NO_PROC";
#[cfg(target_os = "linux")]
static LINUX_BWRAP_CHILD_PID: AtomicI32 = AtomicI32::new(0);
#[cfg(target_os = "linux")]
static LINUX_BWRAP_PENDING_FORWARDED_SIGNAL: AtomicI32 = AtomicI32::new(0);
#[cfg(target_os = "linux")]
const LINUX_BWRAP_FORWARDED_SIGNALS: &[libc::c_int] =
    &[libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM];

#[derive(Debug, Clone)]
pub enum SandboxError {
    SessionTempDir(String),
    #[cfg(target_os = "macos")]
    SeatbeltMissing,
    #[cfg(target_os = "linux")]
    LinuxSandbox(String),
    #[cfg(target_os = "windows")]
    WindowsSandbox(String),
}

impl std::fmt::Display for SandboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SandboxError::SessionTempDir(message) => {
                write!(f, "failed to create session temp dir: {message}")
            }
            #[cfg(target_os = "macos")]
            SandboxError::SeatbeltMissing => {
                write!(f, "seatbelt sandbox executable not found")
            }
            #[cfg(target_os = "linux")]
            SandboxError::LinuxSandbox(message) => {
                write!(f, "linux sandbox error: {message}")
            }
            #[cfg(target_os = "windows")]
            SandboxError::WindowsSandbox(message) => {
                write!(f, "windows sandbox error: {message}")
            }
        }
    }
}

impl std::error::Error for SandboxError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum NetworkAccess {
    #[default]
    Restricted,
    Enabled,
}

impl NetworkAccess {
    pub fn is_enabled(self) -> bool {
        matches!(self, NetworkAccess::Enabled)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ManagedNetworkPolicy {
    pub allowed_domains: Vec<String>,
    pub denied_domains: Vec<String>,
    pub allow_local_binding: bool,
}

impl ManagedNetworkPolicy {
    pub fn has_domain_restrictions(&self) -> bool {
        !self.allowed_domains.is_empty() || !self.denied_domains.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum SandboxPolicy {
    #[serde(rename = "danger-full-access")]
    DangerFullAccess,
    #[serde(rename = "read-only")]
    ReadOnly {
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        network_access: bool,
    },
    #[serde(rename = "external-sandbox")]
    ExternalSandbox {
        #[serde(default)]
        network_access: NetworkAccess,
    },
    #[serde(rename = "workspace-write")]
    WorkspaceWrite {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        writable_roots: Vec<PathBuf>,
        #[serde(default)]
        network_access: bool,
        #[serde(default)]
        exclude_tmpdir_env_var: bool,
        #[serde(default)]
        exclude_slash_tmp: bool,
    },
    #[serde(rename = "managed")]
    Managed {
        file_system: FileSystemSandboxPolicy,
        #[serde(default)]
        network_access: NetworkAccess,
    },
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WritableRoot {
    pub root: PathBuf,
    pub read_only_subpaths: Vec<PathBuf>,
    #[cfg(target_os = "linux")]
    pub protected_metadata_names: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileSystemAccessMode {
    Read,
    Write,
    #[serde(alias = "none")]
    Deny,
}

impl FileSystemAccessMode {
    pub(crate) fn can_read(self) -> bool {
        !matches!(self, FileSystemAccessMode::Deny)
    }

    pub(crate) fn can_write(self) -> bool {
        matches!(self, FileSystemAccessMode::Write)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FileSystemSpecialPath {
    Root,
    Minimal,
    #[serde(alias = "current_working_directory")]
    ProjectRoots {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subpath: Option<PathBuf>,
    },
    Tmpdir,
    SlashTmp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FileSystemPath {
    Path { path: PathBuf },
    GlobPattern { pattern: String },
    Special { value: FileSystemSpecialPath },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSystemSandboxEntry {
    pub path: FileSystemPath,
    pub access: FileSystemAccessMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum FileSystemSandboxKind {
    #[default]
    Restricted,
    Unrestricted,
    ExternalSandbox,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSystemSandboxPolicy {
    pub kind: FileSystemSandboxKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glob_scan_max_depth: Option<usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<FileSystemSandboxEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct ResolvedFileSystemEntry {
    path: PathBuf,
    access: FileSystemAccessMode,
}

impl Default for FileSystemSandboxPolicy {
    fn default() -> Self {
        Self::read_only()
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl FileSystemSandboxPolicy {
    fn read_only() -> Self {
        Self::restricted(vec![FileSystemSandboxEntry {
            path: FileSystemPath::Special {
                value: FileSystemSpecialPath::Root,
            },
            access: FileSystemAccessMode::Read,
        }])
    }

    fn unrestricted() -> Self {
        Self {
            kind: FileSystemSandboxKind::Unrestricted,
            glob_scan_max_depth: None,
            entries: Vec::new(),
        }
    }

    fn external_sandbox() -> Self {
        Self {
            kind: FileSystemSandboxKind::ExternalSandbox,
            glob_scan_max_depth: None,
            entries: Vec::new(),
        }
    }

    fn restricted(entries: Vec<FileSystemSandboxEntry>) -> Self {
        Self {
            kind: FileSystemSandboxKind::Restricted,
            glob_scan_max_depth: None,
            entries,
        }
    }

    fn workspace_write(
        writable_roots: &[PathBuf],
        exclude_tmpdir_env_var: bool,
        exclude_slash_tmp: bool,
    ) -> Self {
        let mut entries = vec![
            FileSystemSandboxEntry {
                path: FileSystemPath::Special {
                    value: FileSystemSpecialPath::Root,
                },
                access: FileSystemAccessMode::Read,
            },
            FileSystemSandboxEntry {
                path: FileSystemPath::Special {
                    value: FileSystemSpecialPath::ProjectRoots { subpath: None },
                },
                access: FileSystemAccessMode::Write,
            },
        ];
        if !exclude_slash_tmp {
            entries.push(FileSystemSandboxEntry {
                path: FileSystemPath::Special {
                    value: FileSystemSpecialPath::SlashTmp,
                },
                access: FileSystemAccessMode::Write,
            });
        }
        if !exclude_tmpdir_env_var {
            entries.push(FileSystemSandboxEntry {
                path: FileSystemPath::Special {
                    value: FileSystemSpecialPath::Tmpdir,
                },
                access: FileSystemAccessMode::Write,
            });
        }
        entries.extend(
            writable_roots
                .iter()
                .cloned()
                .map(|path| FileSystemSandboxEntry {
                    path: FileSystemPath::Path { path },
                    access: FileSystemAccessMode::Write,
                }),
        );
        for subpath in PROTECTED_METADATA_SUBPATHS {
            entries.push(FileSystemSandboxEntry {
                path: FileSystemPath::Special {
                    value: FileSystemSpecialPath::ProjectRoots {
                        subpath: Some(PathBuf::from(subpath)),
                    },
                },
                access: FileSystemAccessMode::Read,
            });
        }
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            for root in writable_roots {
                for subpath in compute_read_only_subpaths(root) {
                    entries.push(FileSystemSandboxEntry {
                        path: FileSystemPath::Path { path: subpath },
                        access: FileSystemAccessMode::Read,
                    });
                }
            }
        }
        Self::restricted(entries)
    }

    fn has_root_access(&self, predicate: impl Fn(FileSystemAccessMode) -> bool) -> bool {
        matches!(self.kind, FileSystemSandboxKind::Restricted)
            && self.entries.iter().any(|entry| {
                matches!(
                    &entry.path,
                    FileSystemPath::Special {
                        value: FileSystemSpecialPath::Root,
                    } if predicate(entry.access)
                )
            })
    }

    fn has_denied_read_restrictions(&self) -> bool {
        matches!(self.kind, FileSystemSandboxKind::Restricted)
            && self
                .entries
                .iter()
                .any(|entry| entry.access == FileSystemAccessMode::Deny)
    }

    fn has_write_narrowing_entries(&self) -> bool {
        matches!(self.kind, FileSystemSandboxKind::Restricted)
            && self.entries.iter().any(|entry| {
                if entry.access.can_write() {
                    return false;
                }
                !matches!(
                    &entry.path,
                    FileSystemPath::Special {
                        value: FileSystemSpecialPath::Root,
                    } if entry.access == FileSystemAccessMode::Read
                )
            })
    }

    pub(crate) fn has_full_disk_read_access(&self) -> bool {
        match self.kind {
            FileSystemSandboxKind::Unrestricted | FileSystemSandboxKind::ExternalSandbox => true,
            FileSystemSandboxKind::Restricted => {
                self.has_root_access(FileSystemAccessMode::can_read)
                    && !self.has_denied_read_restrictions()
            }
        }
    }

    pub(crate) fn has_full_disk_write_access(&self) -> bool {
        match self.kind {
            FileSystemSandboxKind::Unrestricted | FileSystemSandboxKind::ExternalSandbox => true,
            FileSystemSandboxKind::Restricted => {
                self.has_root_access(FileSystemAccessMode::can_write)
                    && !self.has_write_narrowing_entries()
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn include_platform_defaults(&self) -> bool {
        !self.has_full_disk_read_access()
    }

    #[cfg(target_os = "linux")]
    fn include_platform_defaults(&self) -> bool {
        !self.has_full_disk_read_access()
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn get_readable_roots_with_cwd(
        &self,
        cwd: &Path,
        session_temp_dir: Option<&Path>,
    ) -> Vec<PathBuf> {
        if self.has_full_disk_read_access() {
            return Vec::new();
        }
        let roots = self
            .resolved_entries_with_cwd(cwd, session_temp_dir)
            .into_iter()
            .filter(|entry| entry.access.can_read())
            .filter(|entry| self.can_read_path_with_cwd(&entry.path, cwd, session_temp_dir))
            .map(|entry| entry.path)
            .collect();
        dedup_paths(roots)
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn get_writable_roots_with_cwd(
        &self,
        cwd: &Path,
        session_temp_dir: Option<&Path>,
    ) -> Vec<WritableRoot> {
        if self.has_full_disk_write_access() {
            return Vec::new();
        }
        let resolved_entries = self.resolved_entries_with_cwd(cwd, session_temp_dir);
        let writable_entries = resolved_entries
            .iter()
            .filter(|entry| entry.access.can_write())
            .filter(|entry| self.can_write_path_with_cwd(&entry.path, cwd, session_temp_dir))
            .map(|entry| entry.path.clone())
            .collect::<Vec<_>>();
        dedup_paths(writable_entries)
            .into_iter()
            .map(|root| {
                #[cfg(target_os = "linux")]
                let protect_metadata =
                    !is_linux_ambient_temp_writable_root(&root, session_temp_dir);
                #[cfg(not(target_os = "linux"))]
                let protect_metadata = true;

                let mut read_only_subpaths = if protect_metadata {
                    compute_writable_root_exclusions(&root, cwd)
                } else {
                    Vec::new()
                };
                read_only_subpaths.extend(
                    resolved_entries
                        .iter()
                        .filter(|entry| !entry.access.can_write())
                        .filter(|entry| {
                            !self.can_write_path_with_cwd(&entry.path, cwd, session_temp_dir)
                        })
                        .filter_map(|entry| {
                            if path_is_descendant_of_root(&entry.path, &root) {
                                Some(entry.path.clone())
                            } else {
                                None
                            }
                        }),
                );
                #[cfg(target_os = "linux")]
                let protected_metadata_names = if protect_metadata {
                    self.protected_metadata_names_for_writable_root(
                        &root,
                        &resolved_entries,
                        cwd,
                        session_temp_dir,
                    )
                } else {
                    Vec::new()
                };
                WritableRoot {
                    root,
                    read_only_subpaths: dedup_paths(read_only_subpaths),
                    #[cfg(target_os = "linux")]
                    protected_metadata_names,
                }
            })
            .collect()
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn get_unreadable_roots_with_cwd(
        &self,
        cwd: &Path,
        session_temp_dir: Option<&Path>,
    ) -> Vec<PathBuf> {
        if !matches!(self.kind, FileSystemSandboxKind::Restricted) {
            return Vec::new();
        }
        let root = filesystem_root_for_cwd(cwd);
        dedup_paths(
            self.resolved_entries_with_cwd(cwd, session_temp_dir)
                .into_iter()
                .filter(|entry| entry.access == FileSystemAccessMode::Deny)
                .filter(|entry| !self.can_read_path_with_cwd(&entry.path, cwd, session_temp_dir))
                .filter(|entry| Some(entry.path.as_path()) != root.as_deref())
                .map(|entry| entry.path)
                .collect(),
        )
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn get_unreadable_globs_with_cwd(&self, cwd: &Path) -> Vec<String> {
        if !matches!(self.kind, FileSystemSandboxKind::Restricted) {
            return Vec::new();
        }
        let mut patterns = self
            .entries
            .iter()
            .filter(|entry| entry.access == FileSystemAccessMode::Deny)
            .filter_map(|entry| match &entry.path {
                FileSystemPath::GlobPattern { pattern } => {
                    Some(resolve_glob_pattern_against_cwd(pattern, cwd))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        patterns.sort();
        patterns.dedup();
        patterns
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn can_read_path_with_cwd(
        &self,
        path: &Path,
        cwd: &Path,
        session_temp_dir: Option<&Path>,
    ) -> bool {
        self.resolve_access_with_cwd(path, cwd, session_temp_dir)
            .can_read()
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn can_write_path_with_cwd(
        &self,
        path: &Path,
        cwd: &Path,
        session_temp_dir: Option<&Path>,
    ) -> bool {
        if !self
            .resolve_access_with_cwd(path, cwd, session_temp_dir)
            .can_write()
        {
            return false;
        }
        if self.has_full_disk_write_access() {
            return true;
        }
        !self.is_metadata_write_denied(path, cwd, session_temp_dir)
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn resolve_access_with_cwd(
        &self,
        path: &Path,
        cwd: &Path,
        session_temp_dir: Option<&Path>,
    ) -> FileSystemAccessMode {
        match self.kind {
            FileSystemSandboxKind::Unrestricted | FileSystemSandboxKind::ExternalSandbox => {
                return FileSystemAccessMode::Write;
            }
            FileSystemSandboxKind::Restricted => {}
        }
        let Some(path) = resolve_candidate_path(path, cwd) else {
            return FileSystemAccessMode::Deny;
        };
        self.resolved_entries_with_cwd(cwd, session_temp_dir)
            .into_iter()
            .filter(|entry| path_is_at_or_under_root(&path, &entry.path))
            .max_by_key(|entry| (sandbox_path_specificity(&entry.path), entry.access))
            .map(|entry| entry.access)
            .unwrap_or(FileSystemAccessMode::Deny)
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn resolved_entries_with_cwd(
        &self,
        cwd: &Path,
        session_temp_dir: Option<&Path>,
    ) -> Vec<ResolvedFileSystemEntry> {
        self.entries
            .iter()
            .filter_map(|entry| {
                resolve_entry_path(&entry.path, cwd, session_temp_dir).map(|path| {
                    ResolvedFileSystemEntry {
                        path,
                        access: entry.access,
                    }
                })
            })
            .collect()
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn is_metadata_write_denied(
        &self,
        path: &Path,
        cwd: &Path,
        session_temp_dir: Option<&Path>,
    ) -> bool {
        if !matches!(self.kind, FileSystemSandboxKind::Restricted) {
            return false;
        }
        let Some(target) = resolve_candidate_path(path, cwd) else {
            return true;
        };
        let Some(protected_metadata_path) =
            self.metadata_child_of_writable_root(target.as_path(), cwd, session_temp_dir)
        else {
            return false;
        };
        !self.has_explicit_write_entry_for_metadata_path(
            &protected_metadata_path,
            target.as_path(),
            cwd,
            session_temp_dir,
        )
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn metadata_child_of_writable_root(
        &self,
        target: &Path,
        cwd: &Path,
        session_temp_dir: Option<&Path>,
    ) -> Option<PathBuf> {
        self.resolved_entries_with_cwd(cwd, session_temp_dir)
            .iter()
            .filter(|entry| entry.access.can_write())
            .filter_map(|entry| {
                let relative_path = target.strip_prefix(&entry.path).ok()?;
                let first_component = relative_path.components().next()?;
                let metadata_name = first_component.as_os_str().to_str()?;
                PROTECTED_METADATA_SUBPATHS
                    .contains(&metadata_name)
                    .then(|| entry.path.join(metadata_name))
            })
            .next()
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn has_explicit_write_entry_for_metadata_path(
        &self,
        protected_metadata_path: &Path,
        target: &Path,
        cwd: &Path,
        session_temp_dir: Option<&Path>,
    ) -> bool {
        self.resolved_entries_with_cwd(cwd, session_temp_dir)
            .iter()
            .any(|entry| {
                entry.access.can_write()
                    && target.starts_with(&entry.path)
                    && entry.path.starts_with(protected_metadata_path)
            })
    }

    #[cfg(target_os = "linux")]
    fn protected_metadata_names_for_writable_root(
        &self,
        root: &Path,
        resolved_entries: &[ResolvedFileSystemEntry],
        cwd: &Path,
        session_temp_dir: Option<&Path>,
    ) -> Vec<String> {
        let raw_writable_roots = resolved_entries
            .iter()
            .filter(|entry| entry.access.can_write())
            .filter(|entry| self.can_write_path_with_cwd(&entry.path, cwd, session_temp_dir))
            .filter(|entry| sandbox_path_relation(&entry.path, root).is_some())
            .map(|entry| entry.path.as_path())
            .collect::<Vec<_>>();
        PROTECTED_METADATA_SUBPATHS
            .iter()
            .filter_map(|metadata_name| {
                let mut metadata_paths = vec![root.join(metadata_name)];
                metadata_paths.extend(
                    raw_writable_roots
                        .iter()
                        .map(|raw_root| raw_root.join(metadata_name)),
                );
                metadata_paths
                    .iter()
                    .all(|metadata_path| {
                        !self.can_write_path_with_cwd(metadata_path, cwd, session_temp_dir)
                    })
                    .then(|| (*metadata_name).to_string())
            })
            .collect()
    }
}

impl SandboxPolicy {
    #[allow(dead_code)]
    pub fn has_full_disk_write_access(&self) -> bool {
        match self {
            SandboxPolicy::DangerFullAccess => true,
            SandboxPolicy::ExternalSandbox { .. } => true,
            SandboxPolicy::ReadOnly { .. } => false,
            SandboxPolicy::WorkspaceWrite { .. } => false,
            SandboxPolicy::Managed { file_system, .. } => file_system.has_full_disk_write_access(),
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[allow(dead_code)]
    pub fn has_full_disk_read_access(&self) -> bool {
        match self {
            SandboxPolicy::DangerFullAccess => true,
            SandboxPolicy::ExternalSandbox { .. } => true,
            SandboxPolicy::ReadOnly { .. } => true,
            SandboxPolicy::WorkspaceWrite { .. } => true,
            SandboxPolicy::Managed { file_system, .. } => file_system.has_full_disk_read_access(),
        }
    }

    pub fn has_full_network_access(&self) -> bool {
        match self {
            SandboxPolicy::DangerFullAccess => true,
            SandboxPolicy::ExternalSandbox { network_access } => network_access.is_enabled(),
            SandboxPolicy::ReadOnly { network_access } => *network_access,
            SandboxPolicy::WorkspaceWrite { network_access, .. } => *network_access,
            SandboxPolicy::Managed { network_access, .. } => network_access.is_enabled(),
        }
    }

    pub fn requires_sandbox(&self) -> bool {
        match self {
            SandboxPolicy::DangerFullAccess | SandboxPolicy::ExternalSandbox { .. } => false,
            SandboxPolicy::Managed {
                file_system,
                network_access,
            } => !file_system.has_full_disk_write_access() || !network_access.is_enabled(),
            SandboxPolicy::ReadOnly { .. } | SandboxPolicy::WorkspaceWrite { .. } => true,
        }
    }

    #[cfg(target_os = "macos")]
    #[allow(dead_code)]
    pub fn get_writable_roots_with_cwd(
        &self,
        cwd: &Path,
        session_temp_dir: Option<&Path>,
    ) -> Vec<WritableRoot> {
        match self {
            SandboxPolicy::ReadOnly { .. } => {
                let roots = temp_writable_roots(false, false, session_temp_dir);
                roots
                    .into_iter()
                    .map(|root| WritableRoot {
                        read_only_subpaths: compute_macos_writable_root_exclusions(&root),
                        root,
                    })
                    .collect()
            }
            SandboxPolicy::WorkspaceWrite {
                writable_roots,
                exclude_tmpdir_env_var,
                exclude_slash_tmp,
                network_access: _,
            } => {
                let mut roots = Vec::new();

                for root in writable_roots {
                    if let Some(path) = ensure_absolute(root.clone()) {
                        roots.push(path);
                    }
                }

                if let Some(path) = ensure_absolute(cwd.to_path_buf()) {
                    roots.push(path);
                }

                roots.extend(temp_writable_roots(
                    *exclude_tmpdir_env_var,
                    *exclude_slash_tmp,
                    session_temp_dir,
                ));

                roots.sort();
                roots.dedup();

                roots
                    .into_iter()
                    .map(|root| WritableRoot {
                        read_only_subpaths: compute_macos_writable_root_exclusions(&root),
                        root,
                    })
                    .collect()
            }
            SandboxPolicy::Managed { file_system, .. } => {
                file_system.get_writable_roots_with_cwd(cwd, session_temp_dir)
            }
            _ => Vec::new(),
        }
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn file_system_policy_from_legacy(policy: &SandboxPolicy) -> FileSystemSandboxPolicy {
    match policy {
        SandboxPolicy::DangerFullAccess => FileSystemSandboxPolicy::unrestricted(),
        SandboxPolicy::ExternalSandbox { .. } => FileSystemSandboxPolicy::external_sandbox(),
        SandboxPolicy::ReadOnly { .. } => FileSystemSandboxPolicy::read_only(),
        SandboxPolicy::WorkspaceWrite {
            writable_roots,
            exclude_tmpdir_env_var,
            exclude_slash_tmp,
            ..
        } => FileSystemSandboxPolicy::workspace_write(
            writable_roots,
            *exclude_tmpdir_env_var,
            *exclude_slash_tmp,
        ),
        SandboxPolicy::Managed { file_system, .. } => file_system.clone(),
    }
}

#[cfg_attr(target_os = "windows", allow(dead_code))]
fn ensure_absolute(path: PathBuf) -> Option<PathBuf> {
    if path.is_absolute() { Some(path) } else { None }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn dedup_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut deduped = Vec::with_capacity(paths.len());
    let mut seen = std::collections::HashSet::new();
    for path in paths {
        if seen.insert(path.clone()) {
            deduped.push(path);
        }
    }
    deduped
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn filesystem_root_for_cwd(cwd: &Path) -> Option<PathBuf> {
    let cwd = if cwd.is_absolute() {
        cwd.to_path_buf()
    } else {
        return None;
    };
    cwd.ancestors().last().map(Path::to_path_buf)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn resolve_candidate_path(path: &Path, cwd: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        Some(path.to_path_buf())
    } else if cwd.is_absolute() {
        Some(cwd.join(path))
    } else {
        None
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn resolve_entry_path(
    path: &FileSystemPath,
    cwd: &Path,
    session_temp_dir: Option<&Path>,
) -> Option<PathBuf> {
    match path {
        FileSystemPath::Path { path } => Some(path.clone()),
        FileSystemPath::GlobPattern { .. } => None,
        FileSystemPath::Special { value } => {
            resolve_file_system_special_path(value, cwd, session_temp_dir)
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn resolve_file_system_special_path(
    value: &FileSystemSpecialPath,
    cwd: &Path,
    session_temp_dir: Option<&Path>,
) -> Option<PathBuf> {
    match value {
        FileSystemSpecialPath::Root => filesystem_root_for_cwd(cwd),
        FileSystemSpecialPath::Minimal => None,
        FileSystemSpecialPath::ProjectRoots { subpath } => {
            let cwd = ensure_absolute(cwd.to_path_buf())?;
            match subpath {
                Some(subpath) if subpath.is_absolute() => Some(subpath.clone()),
                Some(subpath) => Some(cwd.join(subpath)),
                None => Some(cwd),
            }
        }
        FileSystemSpecialPath::Tmpdir => session_temp_dir
            .and_then(|path| ensure_absolute(path.to_path_buf()))
            .or_else(|| {
                let tmpdir = std::env::var_os("TMPDIR")?;
                if tmpdir.is_empty() {
                    None
                } else {
                    ensure_absolute(PathBuf::from(tmpdir))
                }
            }),
        FileSystemSpecialPath::SlashTmp => {
            let slash_tmp = PathBuf::from("/tmp");
            slash_tmp.is_dir().then_some(slash_tmp)
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn resolve_glob_pattern_against_cwd(pattern: &str, cwd: &Path) -> String {
    let path = Path::new(pattern);
    if path.is_absolute() {
        pattern.to_string()
    } else {
        cwd.join(path).to_string_lossy().into_owned()
    }
}

fn env_var_truthy(key: &str) -> bool {
    std::env::var(key).ok().is_some_and(|value| {
        let trimmed = value.trim();
        trimmed == "1" || trimmed.eq_ignore_ascii_case("true")
    })
}

#[cfg(target_os = "linux")]
fn env_var_truthy_if_set(key: &str) -> Option<bool> {
    std::env::var(key).ok().map(|value| {
        let trimmed = value.trim();
        trimmed == "1" || trimmed.eq_ignore_ascii_case("true")
    })
}

#[allow(dead_code)]
fn temp_roots_from_system(exclude_tmpdir_env_var: bool, exclude_slash_tmp: bool) -> Vec<PathBuf> {
    let mut roots = Vec::new();

    if cfg!(unix) && !exclude_slash_tmp {
        let slash_tmp = PathBuf::from("/tmp");
        if slash_tmp.is_dir() {
            roots.push(slash_tmp);
        }
    }

    if !exclude_tmpdir_env_var
        && let Some(tmpdir) = std::env::var_os("TMPDIR")
        && !tmpdir.is_empty()
        && let Some(path) = ensure_absolute(PathBuf::from(tmpdir))
    {
        roots.push(path);
    }

    roots
}

#[cfg(target_os = "linux")]
fn is_linux_ambient_temp_writable_root(root: &Path, session_temp_dir: Option<&Path>) -> bool {
    let Some(root) = ensure_absolute(root.to_path_buf()) else {
        return false;
    };
    if session_temp_dir
        .and_then(|path| ensure_absolute(path.to_path_buf()))
        .is_some_and(|session_temp_dir| session_temp_dir == root)
    {
        return false;
    }
    temp_roots_from_system(false, false)
        .into_iter()
        .any(|temp_root| temp_root == root)
}

#[cfg(target_os = "linux")]
pub fn invoked_as_codex_linux_sandbox() -> bool {
    std::env::args_os()
        .next()
        .and_then(|arg0| {
            PathBuf::from(arg0)
                .file_name()
                .map(|name| name.to_os_string())
        })
        .as_deref()
        == Some(std::ffi::OsStr::new("codex-linux-sandbox"))
}

#[cfg(target_os = "macos")]
#[allow(dead_code)]
fn temp_writable_roots(
    exclude_tmpdir_env_var: bool,
    exclude_slash_tmp: bool,
    session_temp_dir: Option<&Path>,
) -> Vec<PathBuf> {
    // Match Codex behavior: keep the session temp dir writable, but also allow
    // system temp roots like /tmp and TMPDIR so native libraries can use them.
    let mut roots = temp_roots_from_system(exclude_tmpdir_env_var, exclude_slash_tmp);
    if let Some(session_temp_dir) = session_temp_dir
        && let Some(path) = ensure_absolute(session_temp_dir.to_path_buf())
    {
        roots.push(path);
    }
    roots
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn compute_read_only_subpaths(root: &Path) -> Vec<PathBuf> {
    let mut subpaths = Vec::new();

    let dot_git = root.join(".git");
    if dot_git.is_dir() || dot_git.is_file() {
        if dot_git.is_file()
            && let Some(gitdir) = resolve_gitdir_from_file(&dot_git)
            && !subpaths.iter().any(|path| path == &gitdir)
        {
            subpaths.push(gitdir);
        }
        subpaths.push(dot_git);
    }

    let dot_codex = root.join(".codex");
    if dot_codex.is_dir() {
        subpaths.push(dot_codex);
    }

    let dot_agents = root.join(".agents");
    if dot_agents.is_dir() {
        subpaths.push(dot_agents);
    }

    subpaths
}

#[cfg(target_os = "macos")]
fn compute_macos_writable_root_exclusions(root: &Path) -> Vec<PathBuf> {
    let mut subpaths = compute_read_only_subpaths(root);
    for subpath in PROTECTED_METADATA_SUBPATHS {
        let protected_path = root.join(subpath);
        if !subpaths.iter().any(|path| path == &protected_path) {
            subpaths.push(protected_path);
        }
    }
    subpaths
}

#[cfg(target_os = "linux")]
fn compute_linux_writable_root_exclusions(root: &Path, cwd: &Path) -> Vec<PathBuf> {
    let mut subpaths = compute_read_only_subpaths(root);
    let codex_path = root.join(".codex");
    if path_is_at_or_under_root(cwd, root) && !subpaths.iter().any(|path| path == &codex_path) {
        subpaths.push(codex_path);
    }
    subpaths
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn compute_writable_root_exclusions(root: &Path, cwd: &Path) -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let _ = cwd;
        compute_macos_writable_root_exclusions(root)
    }
    #[cfg(target_os = "linux")]
    {
        compute_linux_writable_root_exclusions(root, cwd)
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn resolve_gitdir_from_file(dot_git: &Path) -> Option<PathBuf> {
    let contents = std::fs::read_to_string(dot_git).ok()?;
    let trimmed = contents.trim();
    let (_, gitdir_raw) = trimmed.split_once(':')?;
    let gitdir_raw = gitdir_raw.trim();
    if gitdir_raw.is_empty() {
        return None;
    }
    let base = dot_git.parent()?;
    let gitdir_path = if Path::new(gitdir_raw).is_absolute() {
        PathBuf::from(gitdir_raw)
    } else {
        base.join(gitdir_raw)
    };
    if gitdir_path.exists() {
        Some(gitdir_path)
    } else {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxState {
    pub sandbox_policy: SandboxPolicy,
    pub sandbox_cwd: PathBuf,
    pub use_linux_sandbox_bwrap: bool,
    pub managed_network_policy: ManagedNetworkPolicy,
    pub session_temp_dir: PathBuf,
}

fn append_sandbox_state_log_line(payload: &serde_json::Value) {
    let Some(path) = crate::debug_logs::log_path("sandbox-state.jsonl") else {
        return;
    };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{payload}");
    }
}

pub fn log_initial_sandbox_policy(policy: &SandboxPolicy) {
    crate::event_log::log(
        "sandbox_policy_initial",
        serde_json::json!({
            "policy": policy,
        }),
    );
    append_sandbox_state_log_line(&serde_json::json!({
        "kind": "initial-policy",
        "policy": policy,
    }));
}

pub fn log_sandbox_policy_update(policy: &SandboxPolicy) {
    crate::event_log::log(
        "sandbox_policy_update_received",
        serde_json::json!({
            "policy": policy,
        }),
    );
    append_sandbox_state_log_line(&serde_json::to_value(policy).unwrap_or_else(|_| {
        serde_json::json!({
            "debug": format!("{policy:?}"),
        })
    }));
}

pub fn log_sandbox_state_meta(meta: &serde_json::Value) {
    crate::event_log::log(
        "sandbox_state_meta_received",
        serde_json::json!({
            "meta": meta,
        }),
    );
    append_sandbox_state_log_line(&serde_json::json!({
        "kind": "tool-call-meta",
        "meta": meta,
    }));
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxStateUpdate {
    pub sandbox_policy: SandboxPolicy,
    #[serde(default)]
    pub sandbox_cwd: Option<PathBuf>,
    #[serde(default)]
    pub use_linux_sandbox_bwrap: Option<bool>,
    #[serde(default)]
    pub use_legacy_landlock: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodexSandboxStateMeta {
    #[serde(default)]
    sandbox_policy: Option<SandboxPolicy>,
    #[serde(default)]
    permission_profile: Option<CodexPermissionProfile>,
    #[serde(default)]
    codex_linux_sandbox_exe: Option<serde_json::Value>,
    sandbox_cwd: String,
    #[serde(default)]
    use_legacy_landlock: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CodexPermissionProfile {
    Managed {
        file_system: CodexManagedFileSystemPermissions,
        network: NetworkAccess,
    },
    Disabled,
    External {
        network: NetworkAccess,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CodexManagedFileSystemPermissions {
    Restricted {
        #[serde(default)]
        entries: Vec<CodexFileSystemSandboxEntry>,
        #[serde(default)]
        glob_scan_max_depth: Option<usize>,
    },
    Unrestricted,
}

#[derive(Debug, Clone, Deserialize)]
struct CodexFileSystemSandboxEntry {
    path: CodexFileSystemPath,
    access: CodexFileSystemAccessMode,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum CodexFileSystemAccessMode {
    Read,
    Write,
    #[serde(alias = "none")]
    Deny,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CodexFileSystemPath {
    Path { path: String },
    GlobPattern { pattern: String },
    Special { value: CodexFileSystemSpecialPath },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum CodexFileSystemSpecialPath {
    Root,
    Minimal,
    ProjectRoots {
        #[serde(default)]
        subpath: Option<PathBuf>,
    },
    Tmpdir,
    SlashTmp,
    Unknown {
        #[serde(rename = "path")]
        _path: String,
        #[serde(default, rename = "subpath")]
        _subpath: Option<PathBuf>,
    },
}

const CODEX_FULL_WRITE_RESTRICTED_NETWORK_ERROR: &str =
    "Codex permissionProfile full write access with restricted network access is not supported";

pub fn sandbox_state_update_from_codex_meta(
    meta: &serde_json::Value,
) -> Result<SandboxStateUpdate, String> {
    let parsed = serde_json::from_value::<CodexSandboxStateMeta>(meta.clone())
        .map_err(|err| format!("failed to parse Codex sandbox state metadata: {err}"))?;
    let sandbox_cwd = parse_codex_path_uri(&parsed.sandbox_cwd, "sandboxCwd")?;

    let sandbox_policy = match (parsed.sandbox_policy, parsed.permission_profile) {
        (Some(policy), _) => validate_codex_sandbox_policy(policy)?,
        (None, Some(permission_profile)) => {
            sandbox_policy_from_codex_permission_profile(permission_profile, &sandbox_cwd)?
        }
        (None, None) => {
            return Err("failed to parse Codex sandbox state metadata: missing field `sandboxPolicy` or `permissionProfile`"
                .to_string());
        }
    };
    let _ = parsed.codex_linux_sandbox_exe;
    #[cfg(not(target_os = "linux"))]
    let _ = parsed.use_legacy_landlock;
    #[cfg(target_os = "linux")]
    let use_legacy_landlock = parsed.use_legacy_landlock;

    Ok(SandboxStateUpdate {
        sandbox_policy,
        sandbox_cwd: Some(sandbox_cwd),
        #[cfg(target_os = "linux")]
        use_linux_sandbox_bwrap: use_legacy_landlock.then_some(false),
        #[cfg(not(target_os = "linux"))]
        use_linux_sandbox_bwrap: None,
        #[cfg(target_os = "linux")]
        use_legacy_landlock: Some(use_legacy_landlock),
        #[cfg(not(target_os = "linux"))]
        use_legacy_landlock: None,
    })
}

fn parse_codex_path_uri(value: &str, field: &str) -> Result<PathBuf, String> {
    let path = if value.starts_with("file:") {
        let url = Url::parse(value)
            .map_err(|err| format!("Codex sandbox metadata has invalid {field}: {err}"))?;
        if url.scheme() != "file" {
            return Err(format!(
                "Codex sandbox metadata requires {field} to be a file URI, got: {value}"
            ));
        }
        if !url.username().is_empty()
            || url.password().is_some()
            || url.port().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(format!(
                "Codex sandbox metadata {field} file URI has unsupported metadata: {value}"
            ));
        }
        url.to_file_path().map_err(|_| {
            format!("Codex sandbox metadata requires local file URI {field}, got: {value}")
        })?
    } else {
        PathBuf::from(value)
    };

    if !path.is_absolute() {
        return Err(format!(
            "Codex sandbox metadata requires an absolute {field}, got: {}",
            path.display()
        ));
    }
    Ok(path)
}

fn validate_codex_sandbox_policy(policy: SandboxPolicy) -> Result<SandboxPolicy, String> {
    if let SandboxPolicy::WorkspaceWrite { writable_roots, .. } = &policy
        && let Some(root) = writable_roots.iter().find(|root| !root.is_absolute())
    {
        return Err(format!(
            "Codex sandbox metadata requires absolute sandboxPolicy.writable_roots entries, got: {}",
            root.display()
        ));
    }
    Ok(policy)
}

fn sandbox_policy_from_codex_permission_profile(
    permission_profile: CodexPermissionProfile,
    sandbox_cwd: &Path,
) -> Result<SandboxPolicy, String> {
    match permission_profile {
        CodexPermissionProfile::Disabled => Ok(SandboxPolicy::DangerFullAccess),
        CodexPermissionProfile::External { network } => Ok(SandboxPolicy::ExternalSandbox {
            network_access: network,
        }),
        CodexPermissionProfile::Managed {
            file_system,
            network,
        } => sandbox_policy_from_codex_managed_profile(file_system, network, sandbox_cwd),
    }
}

fn sandbox_policy_from_codex_managed_profile(
    file_system: CodexManagedFileSystemPermissions,
    network: NetworkAccess,
    sandbox_cwd: &Path,
) -> Result<SandboxPolicy, String> {
    let network_access = network.is_enabled();
    match file_system {
        CodexManagedFileSystemPermissions::Unrestricted => {
            if network_access {
                Ok(SandboxPolicy::DangerFullAccess)
            } else {
                Ok(SandboxPolicy::Managed {
                    file_system: FileSystemSandboxPolicy::unrestricted(),
                    network_access: network,
                })
            }
        }
        CodexManagedFileSystemPermissions::Restricted {
            entries,
            glob_scan_max_depth,
        } => sandbox_policy_from_codex_restricted_entries(
            entries,
            glob_scan_max_depth,
            network,
            network_access,
            sandbox_cwd,
        ),
    }
}

#[derive(Debug, Default)]
struct RestrictedProfileProjection {
    root_read: bool,
    root_write: bool,
    workspace_root_writable: bool,
    writable_roots: Vec<PathBuf>,
    tmpdir_writable: bool,
    slash_tmp_writable: bool,
    read_entries: Vec<CodexFileSystemPath>,
}

fn sandbox_policy_from_codex_restricted_entries(
    entries: Vec<CodexFileSystemSandboxEntry>,
    glob_scan_max_depth: Option<usize>,
    network: NetworkAccess,
    network_access: bool,
    sandbox_cwd: &Path,
) -> Result<SandboxPolicy, String> {
    let file_system =
        file_system_policy_from_codex_restricted_entries(&entries, glob_scan_max_depth)?;
    if let Ok(policy) =
        legacy_sandbox_policy_from_codex_restricted_entries(entries, network_access, sandbox_cwd)
    {
        if cfg!(target_os = "macos") && matches!(policy, SandboxPolicy::ReadOnly { .. }) {
            return Ok(SandboxPolicy::Managed {
                file_system,
                network_access: network,
            });
        }
        return Ok(policy);
    }
    Ok(SandboxPolicy::Managed {
        file_system,
        network_access: network,
    })
}

fn file_system_policy_from_codex_restricted_entries(
    entries: &[CodexFileSystemSandboxEntry],
    glob_scan_max_depth: Option<usize>,
) -> Result<FileSystemSandboxPolicy, String> {
    let mut runtime_entries = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(path) = file_system_path_from_codex(&entry.path)? else {
            continue;
        };
        match (&path, &entry.access) {
            (FileSystemPath::GlobPattern { .. }, CodexFileSystemAccessMode::Deny) => {}
            (FileSystemPath::GlobPattern { .. }, _) => {
                return Err(
                    "Codex permissionProfile.file_system glob pattern entries only support deny access"
                        .to_string(),
                );
            }
            (
                FileSystemPath::Special {
                    value: FileSystemSpecialPath::Minimal,
                },
                CodexFileSystemAccessMode::Write,
            ) => {
                return Err(
                    "Codex permissionProfile.file_system minimal write access is not supported"
                        .to_string(),
                );
            }
            _ => {}
        }
        runtime_entries.push(FileSystemSandboxEntry {
            path,
            access: match entry.access {
                CodexFileSystemAccessMode::Read => FileSystemAccessMode::Read,
                CodexFileSystemAccessMode::Write => FileSystemAccessMode::Write,
                CodexFileSystemAccessMode::Deny => FileSystemAccessMode::Deny,
            },
        });
    }

    if !runtime_entries.iter().any(|entry| entry.access.can_read()) {
        return Err(
            "Codex permissionProfile.file_system restricted policy requires at least one readable entry"
                .to_string(),
        );
    }

    Ok(FileSystemSandboxPolicy {
        kind: FileSystemSandboxKind::Restricted,
        glob_scan_max_depth,
        entries: runtime_entries,
    })
}

fn file_system_path_from_codex(
    path: &CodexFileSystemPath,
) -> Result<Option<FileSystemPath>, String> {
    match path {
        CodexFileSystemPath::Path { path } => Ok(Some(FileSystemPath::Path {
            path: parse_codex_path_uri(path, "permissionProfile.file_system.entries.path")?,
        })),
        CodexFileSystemPath::GlobPattern { pattern } => Ok(Some(FileSystemPath::GlobPattern {
            pattern: pattern.clone(),
        })),
        CodexFileSystemPath::Special { value } => match value {
            CodexFileSystemSpecialPath::Root => Ok(Some(FileSystemPath::Special {
                value: FileSystemSpecialPath::Root,
            })),
            CodexFileSystemSpecialPath::Minimal => Ok(Some(FileSystemPath::Special {
                value: FileSystemSpecialPath::Minimal,
            })),
            CodexFileSystemSpecialPath::ProjectRoots { subpath } => {
                Ok(Some(FileSystemPath::Special {
                    value: FileSystemSpecialPath::ProjectRoots {
                        subpath: subpath.clone(),
                    },
                }))
            }
            CodexFileSystemSpecialPath::Tmpdir => Ok(Some(FileSystemPath::Special {
                value: FileSystemSpecialPath::Tmpdir,
            })),
            CodexFileSystemSpecialPath::SlashTmp => Ok(Some(FileSystemPath::Special {
                value: FileSystemSpecialPath::SlashTmp,
            })),
            CodexFileSystemSpecialPath::Unknown { .. } => Ok(None),
        },
    }
}

fn legacy_sandbox_policy_from_codex_restricted_entries(
    entries: Vec<CodexFileSystemSandboxEntry>,
    network_access: bool,
    sandbox_cwd: &Path,
) -> Result<SandboxPolicy, String> {
    let mut projection = RestrictedProfileProjection::default();

    for entry in entries {
        if codex_path_is_unknown_special(&entry.path) {
            continue;
        }
        match entry.access {
            CodexFileSystemAccessMode::Deny => {
                return Err(
                    "Codex permissionProfile.file_system deny entries are not supported"
                        .to_string(),
                );
            }
            CodexFileSystemAccessMode::Read => {
                if matches!(
                    &entry.path,
                    CodexFileSystemPath::Special {
                        value: CodexFileSystemSpecialPath::Root
                    }
                ) {
                    projection.root_read = true;
                }
                projection.read_entries.push(entry.path);
            }
            CodexFileSystemAccessMode::Write => {
                project_codex_write_entry(entry.path, sandbox_cwd, &mut projection)?
            }
        }
    }

    if projection.root_write {
        validate_root_write_projection(&projection)?;
        if !network_access {
            return Err(CODEX_FULL_WRITE_RESTRICTED_NETWORK_ERROR.to_string());
        }
        return Ok(SandboxPolicy::DangerFullAccess);
    }

    projection.writable_roots.sort();
    projection.writable_roots.dedup();

    if projection.workspace_root_writable {
        validate_workspace_write_read_entries(&projection, sandbox_cwd)?;
        return Ok(SandboxPolicy::WorkspaceWrite {
            writable_roots: projection.writable_roots,
            network_access,
            exclude_tmpdir_env_var: !projection.tmpdir_writable,
            exclude_slash_tmp: !projection.slash_tmp_writable,
        });
    }

    if !projection.writable_roots.is_empty()
        || projection.tmpdir_writable
        || projection.slash_tmp_writable
    {
        return Err(
            "Codex permissionProfile requests writes outside the workspace root, which mcp-repl cannot represent"
                .to_string(),
        );
    }

    if !projection.root_read {
        return Err(
            "Codex permissionProfile read-only policy without root read access is not supported"
                .to_string(),
        );
    }

    Ok(SandboxPolicy::ReadOnly { network_access })
}

fn codex_path_is_unknown_special(path: &CodexFileSystemPath) -> bool {
    matches!(
        path,
        CodexFileSystemPath::Special {
            value: CodexFileSystemSpecialPath::Unknown { .. }
        }
    )
}

fn project_codex_write_entry(
    path: CodexFileSystemPath,
    sandbox_cwd: &Path,
    projection: &mut RestrictedProfileProjection,
) -> Result<(), String> {
    match path {
        CodexFileSystemPath::Path { path } => {
            let path = parse_codex_path_uri(&path, "permissionProfile.file_system.entries.path")?;
            if path == sandbox_cwd {
                projection.workspace_root_writable = true;
            } else {
                projection.writable_roots.push(path);
            }
        }
        CodexFileSystemPath::Special { value } => match value {
            CodexFileSystemSpecialPath::Root => {
                projection.root_write = true;
            }
            CodexFileSystemSpecialPath::ProjectRoots { subpath: None } => {
                projection.workspace_root_writable = true;
            }
            CodexFileSystemSpecialPath::ProjectRoots {
                subpath: Some(subpath),
            } => {
                projection
                    .writable_roots
                    .push(resolve_codex_project_root_subpath(sandbox_cwd, &subpath));
            }
            CodexFileSystemSpecialPath::Tmpdir => {
                projection.tmpdir_writable = true;
            }
            CodexFileSystemSpecialPath::SlashTmp => {
                projection.slash_tmp_writable = true;
            }
            CodexFileSystemSpecialPath::Minimal => {
                return Err(
                    "Codex permissionProfile.file_system minimal write access is not supported"
                        .to_string(),
                );
            }
            CodexFileSystemSpecialPath::Unknown { .. } => {}
        },
        CodexFileSystemPath::GlobPattern { pattern } => {
            let _ = pattern;
            return Err(
                "Codex permissionProfile.file_system glob pattern writes are not supported"
                    .to_string(),
            );
        }
    }
    Ok(())
}

fn resolve_codex_project_root_subpath(sandbox_cwd: &Path, subpath: &Path) -> PathBuf {
    if subpath.is_absolute() {
        subpath.to_path_buf()
    } else {
        sandbox_cwd.join(subpath)
    }
}

fn validate_root_write_projection(projection: &RestrictedProfileProjection) -> Result<(), String> {
    for read_entry in &projection.read_entries {
        if !matches!(
            read_entry,
            CodexFileSystemPath::Special {
                value: CodexFileSystemSpecialPath::Root
            }
        ) {
            return Err(
                "Codex permissionProfile root write policy with read carveouts is not supported"
                    .to_string(),
            );
        }
    }
    Ok(())
}

fn validate_workspace_write_read_entries(
    projection: &RestrictedProfileProjection,
    sandbox_cwd: &Path,
) -> Result<(), String> {
    for read_entry in &projection.read_entries {
        if workspace_write_read_entry_is_representable(
            read_entry,
            sandbox_cwd,
            &projection.writable_roots,
            projection.root_read,
        )? {
            continue;
        }
        return Err(
            "Codex permissionProfile.file_system read entry cannot be represented by mcp-repl workspace-write"
                .to_string(),
        );
    }
    Ok(())
}

fn workspace_write_read_entry_is_representable(
    read_entry: &CodexFileSystemPath,
    sandbox_cwd: &Path,
    writable_roots: &[PathBuf],
    root_read: bool,
) -> Result<bool, String> {
    match read_entry {
        CodexFileSystemPath::Special {
            value: CodexFileSystemSpecialPath::Root,
        } => Ok(true),
        CodexFileSystemPath::Special {
            value: CodexFileSystemSpecialPath::ProjectRoots { subpath },
        } => Ok(subpath
            .as_ref()
            .is_some_and(|subpath| is_protected_metadata_subpath(subpath))),
        CodexFileSystemPath::Special {
            value:
                CodexFileSystemSpecialPath::Tmpdir
                | CodexFileSystemSpecialPath::SlashTmp
                | CodexFileSystemSpecialPath::Minimal,
        } => Ok(root_read),
        CodexFileSystemPath::Special {
            value: CodexFileSystemSpecialPath::Unknown { .. },
        } => Ok(true),
        CodexFileSystemPath::Path { path } => {
            let path = parse_codex_path_uri(path, "permissionProfile.file_system.entries.path")?;
            if is_protected_metadata_path_under_roots(&path, sandbox_cwd, writable_roots) {
                return Ok(true);
            }
            Ok(root_read && !is_under_any_root(&path, sandbox_cwd, writable_roots))
        }
        CodexFileSystemPath::GlobPattern { pattern } => {
            let _ = pattern;
            Ok(false)
        }
    }
}

fn is_protected_metadata_path_under_roots(
    path: &Path,
    sandbox_cwd: &Path,
    writable_roots: &[PathBuf],
) -> bool {
    is_protected_metadata_path_under_root(path, sandbox_cwd)
        || writable_roots
            .iter()
            .any(|root| is_protected_metadata_path_under_root(path, root))
}

fn is_protected_metadata_path_under_root(path: &Path, root: &Path) -> bool {
    let Ok(suffix) = path.strip_prefix(root) else {
        return false;
    };
    is_protected_metadata_subpath(suffix)
}

fn is_under_any_root(path: &Path, sandbox_cwd: &Path, writable_roots: &[PathBuf]) -> bool {
    path.starts_with(sandbox_cwd) || writable_roots.iter().any(|root| path.starts_with(root))
}

fn is_protected_metadata_subpath(path: &Path) -> bool {
    let mut components = path.components();
    let Some(first) = components.next() else {
        return false;
    };
    matches!(
        first.as_os_str().to_str(),
        Some(name) if PROTECTED_METADATA_SUBPATHS.contains(&name)
    )
}

impl SandboxState {
    pub fn apply_update(&mut self, update: SandboxStateUpdate) -> bool {
        let mut next = self.clone();
        next.sandbox_policy = update.sandbox_policy;
        if let Some(cwd) = update.sandbox_cwd {
            next.sandbox_cwd = cwd;
        }
        if let Some(use_bwrap) = update.use_linux_sandbox_bwrap {
            next.use_linux_sandbox_bwrap = use_bwrap;
        }
        let changed = next != *self;
        *self = next;
        changed
    }
}

impl Default for SandboxState {
    fn default() -> Self {
        let sandbox_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let session_temp_dir = build_session_temp_dir_path();
        Self {
            sandbox_policy: SandboxPolicy::WorkspaceWrite {
                writable_roots: Vec::new(),
                network_access: false,
                exclude_tmpdir_env_var: false,
                exclude_slash_tmp: false,
            },
            sandbox_cwd,
            use_linux_sandbox_bwrap: cfg!(target_os = "linux"),
            managed_network_policy: ManagedNetworkPolicy::default(),
            session_temp_dir,
        }
    }
}

#[cfg_attr(target_os = "windows", allow(dead_code))]
pub struct PreparedCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub arg0: Option<String>,
    #[cfg(target_os = "macos")]
    pub denial_logger: Option<DenialLogger>,
}

#[cfg(target_family = "unix")]
fn configure_embedded_r_runtime_env(env: &mut HashMap<String, String>) {
    let Some(r_home) = embedded_r_home() else {
        return;
    };

    env.entry("R_HOME".to_string())
        .or_insert_with(|| r_home.to_string_lossy().to_string());

    let lib_dir = r_home.join("lib");
    if !lib_dir.try_exists().unwrap_or(false) {
        return;
    }

    #[cfg(target_os = "linux")]
    prepend_env_path(env, "LD_LIBRARY_PATH", &lib_dir);
    #[cfg(target_os = "macos")]
    prepend_env_path(env, "DYLD_FALLBACK_LIBRARY_PATH", &lib_dir);
}

#[cfg(target_family = "unix")]
fn embedded_r_home() -> Option<&'static PathBuf> {
    static R_HOME: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    R_HOME
        .get_or_init(|| harp::command::r_home_setup().ok())
        .as_ref()
}

#[cfg(target_family = "unix")]
fn prepend_env_path(env: &mut HashMap<String, String>, key: &str, prefix: &Path) {
    let mut paths = vec![prefix.to_path_buf()];
    let existing = env
        .get(key)
        .map(std::ffi::OsString::from)
        .or_else(|| std::env::var_os(key));

    if let Some(existing) = existing {
        for path in std::env::split_paths(&existing) {
            if !paths.iter().any(|candidate| candidate == &path) {
                paths.push(path);
            }
        }
    }

    let value = match std::env::join_paths(paths) {
        Ok(joined) => joined.to_string_lossy().to_string(),
        Err(_) => prefix.to_string_lossy().to_string(),
    };
    env.insert(key.to_string(), value);
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn prepare_worker_command(
    program: &Path,
    args: Vec<String>,
    state: &SandboxState,
) -> Result<PreparedCommand, SandboxError> {
    prepare_worker_command_with_managed_network(program, args, state, None)
}

pub fn prepare_worker_command_with_managed_network(
    program: &Path,
    args: Vec<String>,
    state: &SandboxState,
    managed_network_proxy: Option<&crate::managed_network::ManagedNetworkProxy>,
) -> Result<PreparedCommand, SandboxError> {
    let mut env = HashMap::new();
    if !state.sandbox_policy.has_full_network_access() {
        env.insert(
            CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR.to_string(),
            "1".to_string(),
        );
    }
    env.insert(
        ALLOW_LOCAL_BINDING_ENV_KEY.to_string(),
        if state.managed_network_policy.allow_local_binding {
            "1".to_string()
        } else {
            "0".to_string()
        },
    );
    env.insert(
        MANAGED_ALLOWED_DOMAINS_ENV_KEY.to_string(),
        state.managed_network_policy.allowed_domains.join(","),
    );
    env.insert(
        MANAGED_DENIED_DOMAINS_ENV_KEY.to_string(),
        state.managed_network_policy.denied_domains.join(","),
    );
    env.insert(
        MANAGED_NETWORK_ENV_KEY.to_string(),
        if state.managed_network_policy.has_domain_restrictions() {
            "1".to_string()
        } else {
            "0".to_string()
        },
    );
    if let Some(proxy) = managed_network_proxy {
        proxy.apply_to_env(&mut env);
    }

    ensure_session_temp_dir(&state.session_temp_dir)?;
    {
        let temp_dir = state.session_temp_dir.to_string_lossy().to_string();
        env.insert("TMPDIR".to_string(), temp_dir.clone());
        env.insert(R_SESSION_TMPDIR_ENV.to_string(), temp_dir.clone());
        #[cfg(target_os = "windows")]
        {
            // Ensure Windows sandbox policy and runtime temp resolution both target the
            // per-session temp directory instead of the full user TEMP tree.
            env.insert(
                "TEMP".to_string(),
                state.session_temp_dir.to_string_lossy().to_string(),
            );
            env.insert(
                "TMP".to_string(),
                state.session_temp_dir.to_string_lossy().to_string(),
            );
            if managed_network_proxy.is_some() {
                env.insert("HOME".to_string(), temp_dir.clone());
                env.insert("R_USER".to_string(), temp_dir.clone());
                env.insert("USERPROFILE".to_string(), temp_dir);
            }
        }
        #[cfg(target_os = "linux")]
        if !state.sandbox_policy.has_full_disk_read_access() {
            env.insert("HOME".to_string(), temp_dir.clone());
            env.insert("R_USER".to_string(), temp_dir);
        }
    }

    #[cfg(target_family = "unix")]
    configure_embedded_r_runtime_env(&mut env);

    if !state.sandbox_policy.requires_sandbox() {
        return Ok(PreparedCommand {
            program: program.to_path_buf(),
            args,
            env,
            arg0: None,
            #[cfg(target_os = "macos")]
            denial_logger: None,
        });
    }

    #[cfg(target_os = "macos")]
    {
        if !Path::new(MACOS_PATH_TO_SEATBELT_EXECUTABLE).exists() {
            return Err(SandboxError::SeatbeltMissing);
        }

        let mut network_env = sandbox_network_env_snapshot();
        for key in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
            ALLOW_LOCAL_BINDING_ENV_KEY,
            MANAGED_NETWORK_ENV_KEY,
            MANAGED_ALLOWED_DOMAINS_ENV_KEY,
            MANAGED_DENIED_DOMAINS_ENV_KEY,
        ] {
            if let Some(value) = env.get(key) {
                network_env.insert(key.to_string(), value.clone());
            }
        }
        let command = build_command_vec(program, &args);
        let mut seatbelt_args = create_seatbelt_command_args(
            command,
            &state.sandbox_policy,
            &state.managed_network_policy,
            &network_env,
            &state.sandbox_cwd,
            &state.session_temp_dir,
        );
        let mut full_command = Vec::with_capacity(1 + seatbelt_args.len());
        full_command.push(MACOS_PATH_TO_SEATBELT_EXECUTABLE.to_string());
        full_command.append(&mut seatbelt_args);
        env.insert(CODEX_SANDBOX_ENV_VAR.to_string(), "seatbelt".to_string());
        let denial_logger = log_denials_enabled().then(DenialLogger::new).flatten();
        Ok(PreparedCommand {
            program: PathBuf::from(MACOS_PATH_TO_SEATBELT_EXECUTABLE),
            args: full_command[1..].to_vec(),
            env,
            arg0: None,
            denial_logger,
        })
    }

    #[cfg(target_os = "linux")]
    {
        let policy = sanitize_linux_sandbox_policy(&state.sandbox_policy);
        let command = build_command_vec(program, &args);
        let sandbox_args = create_linux_sandbox_command_args(
            command,
            &policy,
            &state.sandbox_cwd,
            &state.session_temp_dir,
            state.use_linux_sandbox_bwrap,
            env_var_truthy(LINUX_BWRAP_NO_PROC_ENV),
        );
        let sandbox_program =
            std::env::current_exe().map_err(|err| SandboxError::LinuxSandbox(err.to_string()))?;
        Ok(PreparedCommand {
            program: sandbox_program,
            args: sandbox_args,
            env,
            arg0: Some("codex-linux-sandbox".to_string()),
        })
    }

    #[cfg(target_os = "windows")]
    {
        let command = build_command_vec(program, &args);
        let use_offline_identity = managed_network_proxy.is_some();
        if use_offline_identity {
            crate::managed_network::ManagedNetworkProxy::route_local_targets_through_proxy(
                &mut env,
            );
        }
        let sandbox_args = create_windows_sandbox_command_args(
            command,
            &state.sandbox_policy,
            &state.sandbox_cwd,
            use_offline_identity,
        )
        .map_err(SandboxError::WindowsSandbox)?;
        let sandbox_program = std::env::current_exe().map_err(|err| {
            SandboxError::WindowsSandbox(format!("failed to resolve current executable: {err}"))
        })?;
        Ok(PreparedCommand {
            program: sandbox_program,
            args: sandbox_args,
            env,
            arg0: None,
        })
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        Ok(PreparedCommand {
            program: program.to_path_buf(),
            args,
            env,
            arg0: None,
        })
    }
}

fn build_command_vec(program: &Path, args: &[String]) -> Vec<String> {
    let mut command = Vec::with_capacity(1 + args.len());
    command.push(program.to_string_lossy().to_string());
    command.extend(args.iter().cloned());
    command
}

#[cfg(target_os = "linux")]
fn create_linux_sandbox_command_args(
    command: Vec<String>,
    sandbox_policy: &SandboxPolicy,
    sandbox_policy_cwd: &Path,
    session_temp_dir: &Path,
    use_bwrap_sandbox: bool,
    no_proc: bool,
) -> Vec<String> {
    let sandbox_policy_cwd = sandbox_policy_cwd.to_string_lossy().to_string();
    let session_temp_dir = session_temp_dir.to_string_lossy().to_string();
    let sanitized_policy = sanitize_linux_sandbox_policy(sandbox_policy);
    let sandbox_policy_json =
        serde_json::to_string(&sanitized_policy).expect("failed to serialize Linux sandbox policy");
    let mut linux_cmd: Vec<String> = vec![
        "--sandbox-policy-cwd".to_string(),
        sandbox_policy_cwd,
        "--session-temp-dir".to_string(),
        session_temp_dir,
        "--sandbox-policy".to_string(),
        sandbox_policy_json,
    ];
    if use_bwrap_sandbox {
        linux_cmd.push("--use-bwrap-sandbox".to_string());
    }
    if no_proc {
        linux_cmd.push("--no-proc".to_string());
    }
    linux_cmd.extend(["--".to_string()]);
    linux_cmd.extend(command);
    linux_cmd
}

#[cfg(target_os = "windows")]
fn create_windows_sandbox_command_args(
    command: Vec<String>,
    sandbox_policy: &SandboxPolicy,
    sandbox_policy_cwd: &Path,
    use_offline_identity: bool,
) -> Result<Vec<String>, String> {
    let sandbox_policy_cwd = sandbox_policy_cwd.to_string_lossy().to_string();
    let sandbox_policy_json =
        serde_json::to_string(sandbox_policy).map_err(|err| err.to_string())?;
    let mut windows_cmd: Vec<String> = Vec::new();
    if use_offline_identity {
        windows_cmd.push("--windows-sandbox-logon-offline".to_string());
    }
    windows_cmd.extend([
        "--windows-sandbox".to_string(),
        "--sandbox-policy-cwd".to_string(),
        sandbox_policy_cwd,
        "--sandbox-policy".to_string(),
        sandbox_policy_json,
        "--".to_string(),
    ]);
    windows_cmd.extend(command);
    Ok(windows_cmd)
}

#[cfg(target_os = "linux")]
fn sanitize_linux_sandbox_policy(policy: &SandboxPolicy) -> SandboxPolicy {
    match policy {
        SandboxPolicy::WorkspaceWrite {
            writable_roots,
            network_access,
            exclude_tmpdir_env_var,
            exclude_slash_tmp,
        } => {
            let writable_roots = writable_roots
                .iter()
                .filter_map(|root| ensure_absolute(root.clone()))
                .collect();
            SandboxPolicy::WorkspaceWrite {
                writable_roots,
                network_access: *network_access,
                exclude_tmpdir_env_var: *exclude_tmpdir_env_var,
                exclude_slash_tmp: *exclude_slash_tmp,
            }
        }
        SandboxPolicy::ExternalSandbox { network_access } => SandboxPolicy::ExternalSandbox {
            network_access: *network_access,
        },
        SandboxPolicy::Managed {
            file_system,
            network_access,
        } => SandboxPolicy::Managed {
            file_system: file_system.clone(),
            network_access: *network_access,
        },
        SandboxPolicy::DangerFullAccess => SandboxPolicy::DangerFullAccess,
        SandboxPolicy::ReadOnly { network_access } => SandboxPolicy::ReadOnly {
            network_access: *network_access,
        },
    }
}

#[cfg(target_os = "linux")]
fn linux_effective_file_system_policy(
    policy: &SandboxPolicy,
    session_temp_dir: &Path,
) -> FileSystemSandboxPolicy {
    let mut file_system = file_system_policy_from_legacy(policy);
    if matches!(file_system.kind, FileSystemSandboxKind::Restricted)
        && !file_system.has_full_disk_write_access()
        && let Some(session_temp_dir) = ensure_absolute(session_temp_dir.to_path_buf())
    {
        let temp_entry = FileSystemSandboxEntry {
            path: FileSystemPath::Path {
                path: session_temp_dir,
            },
            access: FileSystemAccessMode::Write,
        };
        if !file_system.entries.contains(&temp_entry) {
            file_system.entries.push(temp_entry);
        }
    }
    if matches!(file_system.kind, FileSystemSandboxKind::Restricted)
        && !file_system.has_full_disk_read_access()
        && let Ok(current_exe) = std::env::current_exe()
        && let Some(current_exe) = ensure_absolute(current_exe)
    {
        push_linux_implementation_read_entry(&mut file_system, current_exe);
    }
    if matches!(file_system.kind, FileSystemSandboxKind::Restricted)
        && !file_system.has_full_disk_read_access()
        && let Some(r_home) = embedded_r_home()
        && let Some(r_home) = ensure_absolute(r_home.clone())
    {
        push_linux_implementation_read_entry(&mut file_system, r_home);
    }
    file_system
}

#[cfg(target_os = "linux")]
fn push_linux_implementation_read_entry(file_system: &mut FileSystemSandboxPolicy, path: PathBuf) {
    let entry = FileSystemSandboxEntry {
        path: FileSystemPath::Path { path },
        access: FileSystemAccessMode::Read,
    };
    if !file_system.entries.contains(&entry) {
        file_system.entries.push(entry);
    }
}

// Allocate the server-owned session temp root. Today SandboxState keeps this
// path stable across worker respawns and resets it in place before each spawn.
fn build_session_temp_dir_path() -> PathBuf {
    Builder::new()
        .prefix("mcp-repl-session-")
        .tempdir()
        .map(|dir| dir.keep())
        .unwrap_or_else(|err| {
            eprintln!("Failed to create session temp dir: {err}");
            let mut path = std::env::temp_dir();
            let pid = std::process::id();
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            path.push(format!("mcp-repl-session-{pid}-{nanos}"));
            path
        })
}

// Prepare the server-owned session temp dir for a fresh worker launch by
// clearing any old contents and recreating the directory at the configured
// path.
pub(crate) fn prepare_session_temp_dir(path: &Path) -> Result<(), SandboxError> {
    if !path.is_absolute() {
        return Err(SandboxError::SessionTempDir(format!(
            "session temp dir is not absolute: {}",
            path.to_string_lossy()
        )));
    }
    let base_tmp = std::env::temp_dir();
    if !path.starts_with(&base_tmp) {
        return Err(SandboxError::SessionTempDir(format!(
            "session temp dir outside system temp: {} (base: {})",
            path.to_string_lossy(),
            base_tmp.to_string_lossy()
        )));
    }
    if path.parent().is_none() {
        return Err(SandboxError::SessionTempDir(
            "refusing to use a temp dir without parent".to_string(),
        ));
    }
    reset_session_temp_dir(path)
}

fn ensure_session_temp_dir(path: &Path) -> Result<(), SandboxError> {
    if !path.is_absolute() {
        return Err(SandboxError::SessionTempDir(format!(
            "session temp dir is not absolute: {}",
            path.to_string_lossy()
        )));
    }
    let base_tmp = std::env::temp_dir();
    if !path.starts_with(&base_tmp) {
        return Err(SandboxError::SessionTempDir(format!(
            "session temp dir outside system temp: {} (base: {})",
            path.to_string_lossy(),
            base_tmp.to_string_lossy()
        )));
    }
    if path.parent().is_none() {
        return Err(SandboxError::SessionTempDir(
            "refusing to use a temp dir without parent".to_string(),
        ));
    }
    std::fs::create_dir_all(path).map_err(|err| SandboxError::SessionTempDir(err.to_string()))?;
    Ok(())
}

// Reset the current session temp location in place. This intentionally keeps
// the configured path stable even though the contents are per-launch.
fn reset_session_temp_dir(path: &Path) -> Result<(), SandboxError> {
    if let Err(err) = std::fs::remove_dir_all(path)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        return Err(SandboxError::SessionTempDir(err.to_string()));
    }
    std::fs::create_dir_all(path).map_err(|err| SandboxError::SessionTempDir(err.to_string()))?;
    Ok(())
}

#[cfg(target_os = "macos")]
const MACOS_PATH_TO_SEATBELT_EXECUTABLE: &str = "/usr/bin/sandbox-exec";

#[cfg(target_os = "macos")]
const MACOS_SEATBELT_BASE_POLICY: &str = include_str!("sandbox/seatbelt_base_policy.sbpl");
#[cfg(target_os = "macos")]
const MACOS_SEATBELT_NETWORK_POLICY: &str = include_str!("sandbox/seatbelt_network_policy.sbpl");
#[cfg(target_os = "macos")]
const MACOS_RESTRICTED_READ_ONLY_PLATFORM_DEFAULTS: &str =
    include_str!("sandbox/restricted_read_only_platform_defaults.sbpl");
#[cfg(target_os = "macos")]
const PROXY_URL_ENV_KEYS: [&str; 6] = [
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
];
const MANAGED_NETWORK_ENV_KEY: &str = "MCP_REPL_MANAGED_NETWORK";
const ALLOW_LOCAL_BINDING_ENV_KEY: &str = "ALLOW_LOCAL_BINDING";

pub fn sandbox_state_defaults_with_environment() -> SandboxState {
    let mut defaults = SandboxState::default();
    defaults.managed_network_policy.allow_local_binding =
        env_var_truthy(ALLOW_LOCAL_BINDING_ENV_KEY);
    #[cfg(target_os = "linux")]
    {
        if let Some(use_bwrap) = env_var_truthy_if_set(LINUX_BWRAP_ENABLED_ENV) {
            defaults.use_linux_sandbox_bwrap = use_bwrap;
        }
    }
    defaults
}

#[cfg(target_os = "macos")]
#[derive(Debug, Default)]
struct ProxyPolicyInputs {
    ports: Vec<u16>,
    has_proxy_config: bool,
}

#[cfg(target_os = "macos")]
fn env_bool(value: Option<&str>) -> bool {
    value.is_some_and(|v| {
        let trimmed = v.trim();
        trimmed == "1" || trimmed.eq_ignore_ascii_case("true")
    })
}

#[cfg(target_os = "macos")]
fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1"
}

#[cfg(target_os = "macos")]
fn proxy_scheme_default_port(scheme: &str) -> u16 {
    match scheme {
        "https" => 443,
        "socks5" | "socks5h" | "socks4" | "socks4a" => 1080,
        _ => 80,
    }
}

#[cfg(target_os = "macos")]
fn has_proxy_url_env_vars(env: &HashMap<String, String>) -> bool {
    PROXY_URL_ENV_KEYS
        .iter()
        .filter_map(|key| env.get(*key))
        .any(|value| !value.trim().is_empty())
}

#[cfg(target_os = "macos")]
fn proxy_loopback_ports_from_env(env: &HashMap<String, String>) -> Vec<u16> {
    let mut ports = std::collections::BTreeSet::<u16>::new();
    for key in PROXY_URL_ENV_KEYS {
        let Some(proxy_url) = env.get(key) else {
            continue;
        };
        let trimmed = proxy_url.trim();
        if trimmed.is_empty() {
            continue;
        }

        let candidate = if trimmed.contains("://") {
            trimmed.to_string()
        } else {
            format!("http://{trimmed}")
        };
        let Ok(parsed) = Url::parse(&candidate) else {
            continue;
        };
        let Some(host) = parsed.host_str() else {
            continue;
        };
        if !is_loopback_host(host) {
            continue;
        }

        let scheme = parsed.scheme().to_ascii_lowercase();
        let port = parsed
            .port()
            .unwrap_or_else(|| proxy_scheme_default_port(scheme.as_str()));
        ports.insert(port);
    }
    ports.into_iter().collect()
}

#[cfg(target_os = "macos")]
fn proxy_policy_inputs_from_env(env: &HashMap<String, String>) -> ProxyPolicyInputs {
    ProxyPolicyInputs {
        ports: proxy_loopback_ports_from_env(env),
        has_proxy_config: has_proxy_url_env_vars(env),
    }
}

#[cfg(target_os = "macos")]
fn managed_network_enabled(env: &HashMap<String, String>) -> bool {
    env_bool(env.get(MANAGED_NETWORK_ENV_KEY).map(String::as_str))
}

#[cfg(target_os = "macos")]
fn dynamic_network_policy(
    sandbox_policy: &SandboxPolicy,
    enforce_managed_network: bool,
    allow_local_binding: bool,
    proxy: &ProxyPolicyInputs,
) -> String {
    if !sandbox_policy.has_full_network_access() {
        return String::new();
    }

    if !proxy.ports.is_empty() {
        let mut policy =
            String::from("; allow outbound access only to configured loopback proxy endpoints\n");
        if allow_local_binding {
            policy.push_str("; allow localhost-only binding and loopback traffic\n");
            policy.push_str("(allow network-bind (local ip \"localhost:*\"))\n");
            policy.push_str("(allow network-inbound (local ip \"localhost:*\"))\n");
            policy.push_str("(allow network-outbound (remote ip \"localhost:*\"))\n");
        }
        for port in &proxy.ports {
            policy.push_str(&format!(
                "(allow network-outbound (remote ip \"localhost:{port}\"))\n"
            ));
        }
        return format!("{policy}{MACOS_SEATBELT_NETWORK_POLICY}");
    }

    if proxy.has_proxy_config || enforce_managed_network {
        return String::new();
    }

    format!("(allow network-outbound)\n(allow network-inbound)\n{MACOS_SEATBELT_NETWORK_POLICY}")
}

#[cfg(target_os = "macos")]
fn sandbox_network_env_snapshot() -> HashMap<String, String> {
    let mut env = HashMap::new();
    for key in PROXY_URL_ENV_KEYS {
        if let Ok(value) = std::env::var(key) {
            env.insert(key.to_string(), value);
        }
    }
    for key in [MANAGED_NETWORK_ENV_KEY, ALLOW_LOCAL_BINDING_ENV_KEY] {
        if let Ok(value) = std::env::var(key) {
            env.insert(key.to_string(), value);
        }
    }
    env
}

#[cfg(target_os = "macos")]
struct SeatbeltAccessRoot {
    root: PathBuf,
    excluded_subpaths: Vec<PathBuf>,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn sandbox_path_variants(path: &Path) -> Vec<PathBuf> {
    let mut variants = Vec::new();
    push_unique_path(&mut variants, path.to_path_buf());
    if let Ok(canonical) = path.canonicalize() {
        push_unique_path(&mut variants, canonical);
    }
    if let Some(canonical) = canonicalize_from_existing_parent(path) {
        push_unique_path(&mut variants, canonical);
    }
    variants
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SandboxPathRelation {
    Same,
    Descendant,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn sandbox_path_relation(path: &Path, root: &Path) -> Option<SandboxPathRelation> {
    let path_variants = sandbox_path_variants(path);
    let root_variants = sandbox_path_variants(root);
    let mut descendant = false;
    for path_variant in &path_variants {
        for root_variant in &root_variants {
            if path_variant == root_variant {
                return Some(SandboxPathRelation::Same);
            }
            if path_variant.starts_with(root_variant) {
                descendant = true;
            }
        }
    }
    descendant.then_some(SandboxPathRelation::Descendant)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn path_is_at_or_under_root(path: &Path, root: &Path) -> bool {
    sandbox_path_relation(path, root).is_some()
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn path_is_descendant_of_root(path: &Path, root: &Path) -> bool {
    matches!(
        sandbox_path_relation(path, root),
        Some(SandboxPathRelation::Descendant)
    )
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn sandbox_path_specificity(path: &Path) -> usize {
    sandbox_path_variants(path)
        .iter()
        .map(|path| path.components().count())
        .max()
        .unwrap_or(0)
}

#[cfg(target_os = "macos")]
fn descendant_paths(paths: &[PathBuf], root: &Path) -> Vec<PathBuf> {
    paths
        .iter()
        .filter(|path| path_is_descendant_of_root(path, root))
        .cloned()
        .collect()
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn canonicalize_from_existing_parent(path: &Path) -> Option<PathBuf> {
    let mut suffix = Vec::new();
    let mut current = path;
    loop {
        if let Ok(mut canonical) = current.canonicalize() {
            for component in suffix.iter().rev() {
                canonical.push(component);
            }
            return Some(canonical);
        }
        suffix.push(current.file_name()?.to_os_string());
        current = current.parent()?;
    }
}

#[cfg(target_os = "macos")]
fn build_seatbelt_access_policy(
    action: &str,
    param_prefix: &str,
    roots: Vec<SeatbeltAccessRoot>,
) -> (String, Vec<(String, PathBuf)>) {
    let mut policy_components = Vec::new();
    let mut params = Vec::new();

    for (root_index, access_root) in roots.into_iter().enumerate() {
        for (variant_index, root) in sandbox_path_variants(&access_root.root)
            .into_iter()
            .enumerate()
        {
            let root_param = if variant_index == 0 {
                format!("{param_prefix}_{root_index}")
            } else {
                format!("{param_prefix}_{root_index}_{variant_index}")
            };
            params.push((root_param.clone(), root));
            if access_root.excluded_subpaths.is_empty() {
                policy_components.push(format!("(subpath (param \"{root_param}\"))"));
                continue;
            }

            let mut require_parts = vec![format!("(subpath (param \"{root_param}\"))")];
            for (excluded_index, excluded_subpath) in
                access_root.excluded_subpaths.iter().enumerate()
            {
                for (excluded_variant_index, excluded) in sandbox_path_variants(excluded_subpath)
                    .into_iter()
                    .enumerate()
                {
                    let excluded_param = if excluded_variant_index == 0 {
                        format!("{param_prefix}_{root_index}_EXCLUDED_{excluded_index}")
                    } else {
                        format!(
                            "{param_prefix}_{root_index}_EXCLUDED_{excluded_index}_{excluded_variant_index}"
                        )
                    };
                    require_parts.push(format!(
                        "(require-not (literal (param \"{excluded_param}\")))"
                    ));
                    require_parts.push(format!(
                        "(require-not (subpath (param \"{excluded_param}\")))"
                    ));
                    params.push((excluded_param, excluded));
                }
            }
            policy_components.push(format!("(require-all {} )", require_parts.join(" ")));
        }
    }

    if policy_components.is_empty() {
        (String::new(), Vec::new())
    } else {
        (
            format!("(allow {action}\n{}\n)", policy_components.join(" ")),
            params,
        )
    }
}

#[cfg(target_os = "macos")]
fn build_seatbelt_unreadable_glob_policy(
    file_system: &FileSystemSandboxPolicy,
    cwd: &Path,
) -> String {
    let mut policy_components = Vec::new();
    for pattern in file_system.get_unreadable_globs_with_cwd(cwd) {
        for pattern in seatbelt_unreadable_glob_variants(&pattern) {
            let Some(regex) = seatbelt_regex_for_unreadable_glob(&pattern) else {
                continue;
            };
            let regex = regex.replace('"', "\\\"");
            policy_components.push(format!(r#"(deny file-read* (regex #"{regex}"))"#));
            policy_components.push(format!(r#"(deny file-write-unlink (regex #"{regex}"))"#));
        }
    }
    policy_components.sort();
    policy_components.dedup();
    policy_components.join("\n")
}

#[cfg(target_os = "macos")]
fn build_seatbelt_unreadable_path_policy(
    unreadable_roots: &[PathBuf],
    readable_roots: &[PathBuf],
    writable_roots: &[PathBuf],
) -> (String, Vec<(String, PathBuf)>) {
    let mut policy_components = Vec::new();
    let mut params = Vec::new();

    for (root_index, root) in unreadable_roots.iter().enumerate() {
        let readable_exceptions = descendant_paths(readable_roots, root);
        let writable_exceptions = descendant_paths(writable_roots, root);
        for (variant_index, root) in sandbox_path_variants(root).into_iter().enumerate() {
            let root_param = if variant_index == 0 {
                format!("UNREADABLE_ROOT_{root_index}")
            } else {
                format!("UNREADABLE_ROOT_{root_index}_{variant_index}")
            };
            policy_components.push(format!(
                "(deny file-read* (literal (param \"{root_param}\")))"
            ));
            push_seatbelt_unreadable_subpath_rule(
                &mut policy_components,
                &mut params,
                "file-read*",
                &root_param,
                &readable_exceptions,
                &format!("UNREADABLE_ROOT_{root_index}_{variant_index}_READ_EXCEPTED"),
            );
            policy_components.push(format!(
                "(deny file-write-unlink (literal (param \"{root_param}\")))"
            ));
            push_seatbelt_unreadable_subpath_rule(
                &mut policy_components,
                &mut params,
                "file-write-unlink",
                &root_param,
                &writable_exceptions,
                &format!("UNREADABLE_ROOT_{root_index}_{variant_index}_WRITE_EXCEPTED"),
            );
            params.push((root_param, root));
        }
    }

    (policy_components.join("\n"), params)
}

#[cfg(target_os = "macos")]
fn push_seatbelt_unreadable_subpath_rule(
    policy_components: &mut Vec<String>,
    params: &mut Vec<(String, PathBuf)>,
    action: &str,
    root_param: &str,
    exceptions: &[PathBuf],
    exception_param_prefix: &str,
) {
    if exceptions.is_empty() {
        policy_components.push(format!(
            "(deny {action} (subpath (param \"{root_param}\")))"
        ));
        return;
    }

    let mut require_parts = vec![format!("(subpath (param \"{root_param}\"))")];
    for (exception_index, exception) in exceptions.iter().enumerate() {
        for (variant_index, exception) in sandbox_path_variants(exception).into_iter().enumerate() {
            let exception_param = if variant_index == 0 {
                format!("{exception_param_prefix}_{exception_index}")
            } else {
                format!("{exception_param_prefix}_{exception_index}_{variant_index}")
            };
            require_parts.push(format!(
                "(require-not (literal (param \"{exception_param}\")))"
            ));
            require_parts.push(format!(
                "(require-not (subpath (param \"{exception_param}\")))"
            ));
            params.push((exception_param, exception));
        }
    }
    policy_components.push(format!(
        "(deny {action} (require-all {} ))",
        require_parts.join(" ")
    ));
}

#[cfg(target_os = "macos")]
fn seatbelt_unreadable_glob_variants(pattern: &str) -> Vec<String> {
    let mut variants = vec![pattern.to_string()];
    if let Some(canonical) = canonicalized_static_prefix_glob_pattern(pattern)
        && !variants.iter().any(|existing| existing == &canonical)
    {
        variants.push(canonical);
    }
    variants
}

#[cfg(target_os = "macos")]
fn canonicalized_static_prefix_glob_pattern(pattern: &str) -> Option<String> {
    let first_glob_index = pattern
        .char_indices()
        .find_map(|(index, ch)| matches!(ch, '*' | '?' | '[' | ']').then_some(index))
        .unwrap_or(pattern.len());
    let static_prefix = &pattern[..first_glob_index];
    let slash_index = static_prefix.rfind('/')?;
    let suffix = &pattern[slash_index + 1..];
    let static_dir = if slash_index == 0 {
        "/"
    } else {
        &static_prefix[..slash_index]
    };
    let mut candidate = PathBuf::from(static_dir);
    let mut missing_suffix = PathBuf::new();

    loop {
        if let Ok(canonical) = candidate.canonicalize() {
            let mut rebuilt = canonical;
            if !missing_suffix.as_os_str().is_empty() {
                rebuilt.push(missing_suffix);
            }
            let mut canonical_pattern = rebuilt.to_string_lossy().into_owned();
            if !canonical_pattern.ends_with('/') {
                canonical_pattern.push('/');
            }
            canonical_pattern.push_str(suffix);
            return (canonical_pattern != pattern).then_some(canonical_pattern);
        }

        let file_name = candidate.file_name()?;
        let mut next_missing_suffix = PathBuf::from(file_name);
        if !missing_suffix.as_os_str().is_empty() {
            next_missing_suffix.push(missing_suffix);
        }
        missing_suffix = next_missing_suffix;
        if !candidate.pop() {
            return None;
        }
    }
}

#[cfg(target_os = "macos")]
fn seatbelt_regex_for_unreadable_glob(pattern: &str) -> Option<String> {
    if pattern.is_empty() {
        return None;
    }

    let mut regex = String::from("^");
    let mut chars = pattern.chars().collect::<std::collections::VecDeque<_>>();
    let mut saw_glob = false;

    while let Some(ch) = chars.pop_front() {
        match ch {
            '*' => {
                saw_glob = true;
                if chars.front() == Some(&'*') {
                    chars.pop_front();
                    if chars.front() == Some(&'/') {
                        chars.pop_front();
                        regex.push_str("(.*/)?");
                    } else {
                        regex.push_str(".*");
                    }
                } else {
                    regex.push_str("[^/]*");
                }
            }
            '?' => {
                saw_glob = true;
                regex.push_str("[^/]");
            }
            '[' => {
                saw_glob = true;
                let mut class = Vec::new();
                let mut closed = false;
                while let Some(class_ch) = chars.pop_front() {
                    if class_ch == ']' {
                        closed = true;
                        break;
                    }
                    class.push(class_ch);
                }
                if !closed {
                    regex.push_str("\\[");
                    for class_ch in class.into_iter().rev() {
                        chars.push_front(class_ch);
                    }
                    continue;
                }
                regex.push('[');
                for class_ch in class {
                    match class_ch {
                        '\\' => regex.push_str("\\\\"),
                        '!' if regex.ends_with('[') => regex.push('^'),
                        '^' if regex.ends_with('[') => regex.push_str("\\^"),
                        _ => regex.push(class_ch),
                    }
                }
                regex.push(']');
            }
            ']' => {
                saw_glob = true;
                regex.push_str("\\]");
            }
            _ => regex.push_str(&regex_lite::escape(&ch.to_string())),
        }
    }

    if !saw_glob {
        while regex.len() > 2 && regex.ends_with('/') {
            regex.pop();
        }
        if regex == "^/" {
            regex.push_str(".*");
        } else {
            regex.push_str("(/.*)?");
        }
    }
    regex.push('$');
    Some(regex)
}

#[cfg(target_os = "macos")]
fn create_seatbelt_command_args(
    command: Vec<String>,
    sandbox_policy: &SandboxPolicy,
    managed_network_policy: &ManagedNetworkPolicy,
    network_env: &HashMap<String, String>,
    sandbox_policy_cwd: &Path,
    session_temp_dir: &Path,
) -> Vec<String> {
    let mut file_system = file_system_policy_from_legacy(sandbox_policy);
    let mut required_temp_roots = vec![session_temp_dir.to_path_buf()];
    match sandbox_policy {
        SandboxPolicy::ReadOnly { .. } => {
            required_temp_roots.extend(temp_roots_from_system(false, false));
        }
        SandboxPolicy::WorkspaceWrite {
            exclude_tmpdir_env_var,
            exclude_slash_tmp,
            ..
        } => {
            required_temp_roots.extend(temp_roots_from_system(
                *exclude_tmpdir_env_var,
                *exclude_slash_tmp,
            ));
        }
        _ => {}
    }
    required_temp_roots.sort();
    required_temp_roots.dedup();
    for root in required_temp_roots {
        if matches!(file_system.kind, FileSystemSandboxKind::Restricted)
            && !file_system.can_write_path_with_cwd(
                &root,
                sandbox_policy_cwd,
                Some(session_temp_dir),
            )
        {
            file_system.entries.push(FileSystemSandboxEntry {
                path: FileSystemPath::Path { path: root },
                access: FileSystemAccessMode::Write,
            });
        }
    }
    for root in helper_read_roots_from_command(&command) {
        if matches!(file_system.kind, FileSystemSandboxKind::Restricted)
            && !file_system.can_read_path_with_cwd(
                &root,
                sandbox_policy_cwd,
                Some(session_temp_dir),
            )
        {
            file_system.entries.push(FileSystemSandboxEntry {
                path: FileSystemPath::Path { path: root },
                access: FileSystemAccessMode::Read,
            });
        }
    }
    let unreadable_roots =
        file_system.get_unreadable_roots_with_cwd(sandbox_policy_cwd, Some(session_temp_dir));
    let readable_roots = if file_system.has_full_disk_read_access() {
        Vec::new()
    } else {
        file_system.get_readable_roots_with_cwd(sandbox_policy_cwd, Some(session_temp_dir))
    };
    let writable_roots = if file_system.has_full_disk_write_access() {
        Vec::new()
    } else {
        file_system.get_writable_roots_with_cwd(sandbox_policy_cwd, Some(session_temp_dir))
    };
    let writable_root_paths = writable_roots
        .iter()
        .map(|root| root.root.clone())
        .collect::<Vec<_>>();

    let (file_write_policy, file_write_dir_params) = if file_system.has_full_disk_write_access() {
        if unreadable_roots.is_empty() {
            (
                r#"(allow file-write* (regex #"^/"))"#.to_string(),
                Vec::new(),
            )
        } else {
            build_seatbelt_access_policy(
                "file-write*",
                "WRITABLE_ROOT",
                vec![SeatbeltAccessRoot {
                    root: PathBuf::from("/"),
                    excluded_subpaths: unreadable_roots.clone(),
                }],
            )
        }
    } else {
        build_seatbelt_access_policy(
            "file-write*",
            "WRITABLE_ROOT",
            writable_roots
                .iter()
                .map(|root| SeatbeltAccessRoot {
                    root: root.root.clone(),
                    excluded_subpaths: root.read_only_subpaths.clone(),
                })
                .collect(),
        )
    };

    let (file_read_policy, file_read_dir_params) = if file_system.has_full_disk_read_access() {
        if unreadable_roots.is_empty() {
            (
                "; allow read-only file operations\n(allow file-read*)".to_string(),
                Vec::new(),
            )
        } else {
            let (policy, params) = build_seatbelt_access_policy(
                "file-read*",
                "READABLE_ROOT",
                vec![SeatbeltAccessRoot {
                    root: PathBuf::from("/"),
                    excluded_subpaths: unreadable_roots.clone(),
                }],
            );
            (
                format!("; allow read-only file operations\n{policy}"),
                params,
            )
        }
    } else {
        let (policy, params) = build_seatbelt_access_policy(
            "file-read*",
            "READABLE_ROOT",
            readable_roots
                .iter()
                .map(|root| SeatbeltAccessRoot {
                    excluded_subpaths: descendant_paths(&unreadable_roots, root),
                    root: root.clone(),
                })
                .collect(),
        );
        if policy.is_empty() {
            (String::new(), params)
        } else {
            (
                format!("; allow read-only file operations\n{policy}"),
                params,
            )
        }
    };

    let proxy = proxy_policy_inputs_from_env(network_env);
    let allow_local_binding = managed_network_policy.allow_local_binding;
    let enforce_managed_network =
        managed_network_enabled(network_env) || managed_network_policy.has_domain_restrictions();
    let network_policy = dynamic_network_policy(
        sandbox_policy,
        enforce_managed_network,
        allow_local_binding,
        &proxy,
    );

    let deny_read_policy = build_seatbelt_unreadable_glob_policy(&file_system, sandbox_policy_cwd);
    let (deny_path_policy, deny_path_params) = build_seatbelt_unreadable_path_policy(
        &unreadable_roots,
        &readable_roots,
        &writable_root_paths,
    );
    let mut policy_sections = vec![
        MACOS_SEATBELT_BASE_POLICY.to_string(),
        file_read_policy,
        file_write_policy,
        network_policy,
    ];
    if file_system.include_platform_defaults() {
        policy_sections.push(MACOS_RESTRICTED_READ_ONLY_PLATFORM_DEFAULTS.to_string());
    }
    policy_sections.push(deny_read_policy);
    policy_sections.push(deny_path_policy);
    let full_policy = policy_sections.join("\n");
    if let Some(path) = crate::debug_logs::log_path("seatbelt-policy.sbpl") {
        let _ = std::fs::write(path, &full_policy);
    }

    let dir_params = [
        file_read_dir_params,
        file_write_dir_params,
        deny_path_params,
        macos_dir_params(),
    ]
    .concat();

    let mut seatbelt_args = vec!["-p".to_string(), full_policy];
    let definition_args = dir_params
        .into_iter()
        .map(|(key, value)| format!("-D{key}={value}", value = value.to_string_lossy()));
    seatbelt_args.extend(definition_args);
    seatbelt_args.push("--".to_string());
    seatbelt_args.extend(command);
    seatbelt_args
}

#[cfg(target_os = "macos")]
fn helper_read_roots_from_command(command: &[String]) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(program) = command.first()
        && let Some(parent) = Path::new(program).parent()
        && let Some(parent) = ensure_absolute(parent.to_path_buf())
    {
        roots.push(parent);
    }
    roots.sort();
    roots.dedup();
    roots
}

#[cfg(target_os = "linux")]
pub fn run_linux_sandbox_main() -> ! {
    match linux_sandbox_main_impl() {
        Ok(()) => process::exit(0),
        Err(err) => {
            eprintln!("{err}");
            process::exit(1);
        }
    }
}

#[cfg(target_os = "linux")]
struct LinuxSandboxArgs {
    sandbox_policy_cwd: PathBuf,
    session_temp_dir: PathBuf,
    sandbox_policy: SandboxPolicy,
    command: Vec<std::ffi::OsString>,
    use_bwrap_sandbox: bool,
    apply_seccomp_then_exec: bool,
    no_proc: bool,
}

#[cfg(target_os = "linux")]
fn linux_sandbox_main_impl() -> Result<(), String> {
    let args = linux_sandbox_parse_args()?;
    if args.apply_seccomp_then_exec {
        linux_apply_sandbox_policy_to_current_thread(
            &args.sandbox_policy,
            &args.sandbox_policy_cwd,
            &args.session_temp_dir,
            false,
        )?;
        linux_execvp(args.command)?;
        return Ok(());
    }
    if args.use_bwrap_sandbox {
        linux_exec_bwrap_sandbox(args)?;
        return Ok(());
    }
    linux_apply_sandbox_policy_to_current_thread(
        &args.sandbox_policy,
        &args.sandbox_policy_cwd,
        &args.session_temp_dir,
        true,
    )?;
    linux_execvp(args.command)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_sandbox_parse_args() -> Result<LinuxSandboxArgs, String> {
    let mut sandbox_policy_cwd: Option<PathBuf> = None;
    let mut session_temp_dir: Option<PathBuf> = None;
    let mut sandbox_policy: Option<SandboxPolicy> = None;
    let mut command: Vec<std::ffi::OsString> = Vec::new();
    let mut use_bwrap_sandbox = false;
    let mut apply_seccomp_then_exec = false;
    let mut no_proc = false;

    let mut args = std::env::args_os().skip(1).peekable();
    while let Some(arg) = args.next() {
        if arg == "--use-bwrap-sandbox" {
            use_bwrap_sandbox = true;
            continue;
        }
        if arg == "--apply-seccomp-then-exec" {
            apply_seccomp_then_exec = true;
            continue;
        }
        if arg == "--no-proc" {
            no_proc = true;
            continue;
        }
        if arg == "--sandbox-policy-cwd" {
            let value = args
                .next()
                .ok_or_else(|| "missing value for --sandbox-policy-cwd".to_string())?;
            sandbox_policy_cwd = Some(PathBuf::from(value));
            continue;
        }
        if arg == "--session-temp-dir" {
            let value = args
                .next()
                .ok_or_else(|| "missing value for --session-temp-dir".to_string())?;
            session_temp_dir = Some(PathBuf::from(value));
            continue;
        }
        if arg == "--sandbox-policy" {
            let value = args
                .next()
                .ok_or_else(|| "missing value for --sandbox-policy".to_string())?;
            let value = value
                .into_string()
                .map_err(|_| "--sandbox-policy must be valid UTF-8".to_string())?;
            sandbox_policy = Some(
                serde_json::from_str(&value)
                    .map_err(|err| format!("failed to parse --sandbox-policy: {err}"))?,
            );
            continue;
        }
        if arg == "--" {
            command.extend(args);
            break;
        }
        return Err(format!("unknown argument: {}", arg.to_string_lossy()));
    }

    let sandbox_policy_cwd =
        sandbox_policy_cwd.ok_or_else(|| "missing --sandbox-policy-cwd".to_string())?;
    let session_temp_dir =
        session_temp_dir.ok_or_else(|| "missing --session-temp-dir".to_string())?;
    let sandbox_policy = sandbox_policy.ok_or_else(|| "missing --sandbox-policy".to_string())?;
    if command.is_empty() {
        return Err("no command specified to execute".to_string());
    }

    Ok(LinuxSandboxArgs {
        sandbox_policy_cwd,
        session_temp_dir,
        sandbox_policy,
        command,
        use_bwrap_sandbox,
        apply_seccomp_then_exec,
        no_proc,
    })
}

#[cfg(target_os = "linux")]
fn linux_find_bwrap_program() -> Option<PathBuf> {
    let absolute = PathBuf::from("/usr/bin/bwrap");
    if absolute.is_file() {
        return Some(absolute);
    }

    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("bwrap");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn linux_build_inner_seccomp_command(args: &LinuxSandboxArgs) -> Result<Vec<String>, String> {
    let current_exe = std::env::current_exe().map_err(|err| err.to_string())?;
    let policy = sanitize_linux_sandbox_policy(&args.sandbox_policy);
    let policy_json = serde_json::to_string(&policy).map_err(|err| err.to_string())?;
    let mut inner = vec![
        current_exe.to_string_lossy().to_string(),
        "--sandbox-policy-cwd".to_string(),
        args.sandbox_policy_cwd.to_string_lossy().to_string(),
        "--session-temp-dir".to_string(),
        args.session_temp_dir.to_string_lossy().to_string(),
        "--sandbox-policy".to_string(),
        policy_json,
        "--apply-seccomp-then-exec".to_string(),
        "--".to_string(),
    ];
    inner.extend(
        args.command
            .iter()
            .map(|arg| arg.to_string_lossy().to_string()),
    );
    Ok(inner)
}

#[cfg(target_os = "linux")]
struct LinuxBwrapCommand {
    args: Vec<String>,
    preserved_files: Vec<std::fs::File>,
    empty_file_source_index: Option<usize>,
    preserved_tempdirs: Vec<tempfile::TempDir>,
    synthetic_mount_targets: Vec<LinuxSyntheticMountTarget>,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, PartialEq, Eq)]
enum LinuxSyntheticMountTarget {
    EmptyFile(PathBuf),
    EmptyDirectory(PathBuf),
}

#[cfg(target_os = "linux")]
fn linux_exec_bwrap_sandbox(args: LinuxSandboxArgs) -> Result<(), String> {
    let bwrap_program = linux_find_bwrap_program()
        .ok_or_else(|| "bwrap executable not found (tried /usr/bin/bwrap and PATH)".to_string())?;
    let inner = linux_build_inner_seccomp_command(&args)?;
    let file_system_policy =
        linux_effective_file_system_policy(&args.sandbox_policy, &args.session_temp_dir);
    let network_mode = linux_bwrap_network_mode(&args.sandbox_policy);
    let mount_proc = !args.no_proc
        && linux_bwrap_supports_proc_mount(
            bwrap_program.as_path(),
            &file_system_policy,
            &args.sandbox_policy_cwd,
            &args.session_temp_dir,
            network_mode,
        );
    let bwrap_command = create_linux_bwrap_command_args(
        inner,
        &file_system_policy,
        &args.sandbox_policy_cwd,
        &args.session_temp_dir,
        mount_proc,
        network_mode,
    )?;
    let LinuxBwrapCommand {
        args,
        preserved_files,
        empty_file_source_index: _,
        preserved_tempdirs,
        synthetic_mount_targets,
    } = bwrap_command;
    make_linux_files_inheritable(&preserved_files)?;
    let synthetic_mount_targets_for_cleanup = synthetic_mount_targets.clone();
    let mut full_command = Vec::with_capacity(1 + args.len());
    full_command.push(bwrap_program.into_os_string());
    full_command.extend(args.into_iter().map(std::ffi::OsString::from));
    if !synthetic_mount_targets_for_cleanup.is_empty() || !preserved_tempdirs.is_empty() {
        linux_run_bwrap_child_with_cleanup(
            full_command,
            synthetic_mount_targets_for_cleanup,
            preserved_files,
            preserved_tempdirs,
        )?;
        return Ok(());
    }
    let exec_result = linux_execvp(full_command);
    drop(preserved_files);
    drop(preserved_tempdirs);
    exec_result?;
    Ok(())
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinuxBwrapNetworkMode {
    FullAccess,
    Isolated,
}

#[cfg(target_os = "linux")]
impl LinuxBwrapNetworkMode {
    fn should_unshare_network(self) -> bool {
        matches!(self, LinuxBwrapNetworkMode::Isolated)
    }
}

#[cfg(target_os = "linux")]
fn linux_bwrap_network_mode(policy: &SandboxPolicy) -> LinuxBwrapNetworkMode {
    if policy.has_full_network_access() {
        LinuxBwrapNetworkMode::FullAccess
    } else {
        LinuxBwrapNetworkMode::Isolated
    }
}

#[cfg(target_os = "linux")]
fn linux_bwrap_supports_proc_mount(
    bwrap_program: &Path,
    file_system_policy: &FileSystemSandboxPolicy,
    sandbox_policy_cwd: &Path,
    session_temp_dir: &Path,
    network_mode: LinuxBwrapNetworkMode,
) -> bool {
    let true_path = if Path::new("/usr/bin/true").is_file() {
        "/usr/bin/true"
    } else if Path::new("/bin/true").is_file() {
        "/bin/true"
    } else {
        "true"
    };
    let bwrap_command = match create_linux_bwrap_command_args(
        vec![true_path.to_string()],
        file_system_policy,
        sandbox_policy_cwd,
        session_temp_dir,
        true,
        network_mode,
    ) {
        Ok(command) => command,
        Err(_) => return false,
    };
    if make_linux_files_inheritable(&bwrap_command.preserved_files).is_err() {
        return false;
    }
    let output = std::process::Command::new(bwrap_program)
        .args(&bwrap_command.args)
        .output();
    let synthetic_mount_targets = bwrap_command.synthetic_mount_targets.clone();
    let _ = cleanup_linux_synthetic_mount_targets(&synthetic_mount_targets);
    let Ok(output) = output else {
        return false;
    };
    if output.status.success() {
        return true;
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if is_proc_mount_failure(stderr.as_ref()) {
        eprintln!("codex-linux-sandbox: bwrap could not mount /proc; retrying with --no-proc");
        return false;
    }
    if is_bwrap_proc_probe_quiet_retry_failure(stderr.as_ref()) {
        return false;
    }
    eprintln!(
        "codex-linux-sandbox: bwrap /proc probe failed; retrying with --no-proc: {}",
        stderr.trim()
    );
    false
}

#[cfg(target_os = "linux")]
fn create_linux_bwrap_command_args(
    command: Vec<String>,
    file_system_policy: &FileSystemSandboxPolicy,
    sandbox_policy_cwd: &Path,
    session_temp_dir: &Path,
    mount_proc: bool,
    network_mode: LinuxBwrapNetworkMode,
) -> Result<LinuxBwrapCommand, String> {
    let mut bwrap_args = vec![
        "--die-with-parent".to_string(),
        "--new-session".to_string(),
        "--unshare-user".to_string(),
        "--unshare-pid".to_string(),
    ];
    let mut bwrap_command =
        linux_bwrap_filesystem_args(file_system_policy, sandbox_policy_cwd, session_temp_dir)?;
    bwrap_args.append(&mut bwrap_command.args);
    if network_mode.should_unshare_network() {
        bwrap_args.push("--unshare-net".to_string());
    }
    if mount_proc {
        bwrap_args.push("--proc".to_string());
        bwrap_args.push("/proc".to_string());
    }
    bwrap_args.push("--".to_string());
    bwrap_args.extend(command);
    Ok(LinuxBwrapCommand {
        args: bwrap_args,
        preserved_files: bwrap_command.preserved_files,
        empty_file_source_index: bwrap_command.empty_file_source_index,
        preserved_tempdirs: bwrap_command.preserved_tempdirs,
        synthetic_mount_targets: bwrap_command.synthetic_mount_targets,
    })
}

#[cfg(target_os = "linux")]
const LINUX_PLATFORM_DEFAULT_READ_ROOTS: &[&str] = &[
    "/bin",
    "/sbin",
    "/usr",
    "/etc",
    "/lib",
    "/lib64",
    "/nix/store",
    "/run/current-system/sw",
];

#[cfg(target_os = "linux")]
const MAX_UNREADABLE_GLOB_MATCHES: usize = 8192;

#[cfg(target_os = "linux")]
fn linux_bwrap_filesystem_args(
    file_system_policy: &FileSystemSandboxPolicy,
    cwd: &Path,
    session_temp_dir: &Path,
) -> Result<LinuxBwrapCommand, String> {
    let unreadable_globs = file_system_policy.get_unreadable_globs_with_cwd(cwd);
    if file_system_policy.has_full_disk_write_access() && unreadable_globs.is_empty() {
        return Ok(LinuxBwrapCommand {
            args: vec!["--bind".to_string(), "/".to_string(), "/".to_string()],
            preserved_files: Vec::new(),
            empty_file_source_index: None,
            preserved_tempdirs: Vec::new(),
            synthetic_mount_targets: Vec::new(),
        });
    }
    let mut writable_roots = file_system_policy
        .get_writable_roots_with_cwd(cwd, Some(session_temp_dir))
        .into_iter()
        .collect::<Vec<_>>();
    if writable_roots.is_empty()
        && file_system_policy.has_full_disk_write_access()
        && !unreadable_globs.is_empty()
    {
        writable_roots.push(WritableRoot {
            root: PathBuf::from("/"),
            read_only_subpaths: Vec::new(),
            protected_metadata_names: Vec::new(),
        });
    }
    validate_linux_unreadable_globs_for_future_writes(&unreadable_globs, cwd, &writable_roots)?;

    let mut unreadable_roots = file_system_policy
        .get_unreadable_roots_with_cwd(cwd, Some(session_temp_dir))
        .into_iter()
        .collect::<Vec<_>>();
    unreadable_roots.extend(expand_linux_unreadable_globs(
        &unreadable_globs,
        cwd,
        file_system_policy.glob_scan_max_depth,
    )?);
    unreadable_roots.sort();
    unreadable_roots.dedup();

    let mut command = LinuxBwrapCommand {
        args: if file_system_policy.has_full_disk_read_access() {
            vec![
                "--ro-bind".to_string(),
                "/".to_string(),
                "/".to_string(),
                "--dev".to_string(),
                "/dev".to_string(),
            ]
        } else {
            let mut args = vec![
                "--tmpfs".to_string(),
                "/".to_string(),
                "--dev".to_string(),
                "/dev".to_string(),
            ];
            let mut readable_roots = file_system_policy
                .get_readable_roots_with_cwd(cwd, Some(session_temp_dir))
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>();
            if file_system_policy.include_platform_defaults() {
                readable_roots.extend(
                    LINUX_PLATFORM_DEFAULT_READ_ROOTS
                        .iter()
                        .map(PathBuf::from)
                        .filter(|path| path.exists()),
                );
            }
            if readable_roots.iter().any(|root| root == Path::new("/")) {
                args = vec![
                    "--ro-bind".to_string(),
                    "/".to_string(),
                    "/".to_string(),
                    "--dev".to_string(),
                    "/dev".to_string(),
                ];
            } else {
                for root in readable_roots {
                    if !root.exists() {
                        continue;
                    }
                    let mount_root = if writable_roots
                        .iter()
                        .any(|writable_root| root.starts_with(&writable_root.root))
                    {
                        linux_canonical_target_if_symlinked_path(&root).unwrap_or(root)
                    } else {
                        root
                    };
                    append_linux_mount_target_parent_dir_args(
                        &mut args,
                        &mount_root,
                        Path::new("/"),
                    );
                    args.push("--ro-bind".to_string());
                    args.push(linux_path_to_string(&mount_root));
                    args.push(linux_path_to_string(&mount_root));
                }
            }
            args
        },
        preserved_files: Vec::new(),
        empty_file_source_index: None,
        preserved_tempdirs: Vec::new(),
        synthetic_mount_targets: Vec::new(),
    };
    prepare_linux_missing_writable_roots(&mut command, &writable_roots)?;

    let mut allowed_write_paths = Vec::with_capacity(writable_roots.len() * 2);
    for writable_root in &writable_roots {
        allowed_write_paths.push(writable_root.root.clone());
        if let Some(target) = linux_canonical_target_if_symlinked_path(&writable_root.root) {
            allowed_write_paths.push(target);
        }
    }

    let unreadable_paths = unreadable_roots
        .iter()
        .cloned()
        .collect::<std::collections::HashSet<_>>();
    writable_roots.sort_by_key(|writable_root| linux_path_depth(&writable_root.root));

    let mut unreadable_ancestors_of_writable_roots = unreadable_roots
        .iter()
        .filter(|path| {
            let unreadable_root = path.as_path();
            !allowed_write_paths
                .iter()
                .any(|root| unreadable_root.starts_with(root))
                && allowed_write_paths
                    .iter()
                    .any(|root| root.starts_with(unreadable_root))
        })
        .cloned()
        .collect::<Vec<_>>();
    unreadable_ancestors_of_writable_roots.sort_by_key(|path| linux_path_depth(path));
    for unreadable_root in &unreadable_ancestors_of_writable_roots {
        append_linux_unreadable_root_args(&mut command, unreadable_root, &allowed_write_paths)?;
    }

    for writable_root in &writable_roots {
        let root = writable_root.root.as_path();
        if let Some(masking_root) = unreadable_roots
            .iter()
            .map(PathBuf::as_path)
            .filter(|unreadable_root| root.starts_with(unreadable_root))
            .max_by_key(|unreadable_root| linux_path_depth(unreadable_root))
        {
            append_linux_mount_target_parent_dir_args(&mut command.args, root, masking_root);
        } else if !file_system_policy.has_full_disk_read_access() {
            append_linux_mount_target_parent_dir_args(&mut command.args, root, Path::new("/"));
        }

        let symlink_target = linux_canonical_target_if_symlinked_path(root);
        let mount_root = symlink_target.as_deref().unwrap_or(root);
        if symlink_target.is_some() && !file_system_policy.has_full_disk_read_access() {
            append_linux_mount_target_parent_dir_args(
                &mut command.args,
                mount_root,
                Path::new("/"),
            );
        }
        command.args.push("--bind".to_string());
        command.args.push(linux_path_to_string(mount_root));
        command.args.push(linux_path_to_string(mount_root));
        if symlink_target.is_some() {
            command.args.push("--bind".to_string());
            command.args.push(linux_path_to_string(mount_root));
            command.args.push(linux_path_to_string(root));
        }

        let mut read_only_subpaths = writable_root
            .read_only_subpaths
            .iter()
            .filter(|path| !unreadable_paths.contains(*path))
            .cloned()
            .collect::<Vec<_>>();
        for name in &writable_root.protected_metadata_names {
            let path = root.join(name);
            if !read_only_subpaths.iter().any(|subpath| subpath == &path) {
                read_only_subpaths.push(path);
            }
        }
        if let Some(target) = &symlink_target {
            read_only_subpaths =
                remap_linux_paths_for_symlink_target(read_only_subpaths, root, target);
        }
        read_only_subpaths.sort_by_key(|path| linux_path_depth(path));
        for subpath in read_only_subpaths {
            append_linux_read_only_subpath_args(&mut command, &subpath, &allowed_write_paths)?;
        }

        let mut nested_unreadable_roots = unreadable_roots
            .iter()
            .filter(|path| path.starts_with(root))
            .cloned()
            .collect::<Vec<_>>();
        if let Some(target) = &symlink_target {
            nested_unreadable_roots =
                remap_linux_paths_for_symlink_target(nested_unreadable_roots, root, target);
        }
        nested_unreadable_roots.sort_by_key(|path| linux_path_depth(path));
        for unreadable_root in nested_unreadable_roots {
            append_linux_unreadable_root_args(
                &mut command,
                &unreadable_root,
                &allowed_write_paths,
            )?;
        }
    }

    let mut rootless_unreadable_roots = unreadable_roots
        .iter()
        .filter(|path| {
            let unreadable_root = path.as_path();
            !allowed_write_paths
                .iter()
                .any(|root| unreadable_root.starts_with(root) || root.starts_with(unreadable_root))
        })
        .cloned()
        .collect::<Vec<_>>();
    rootless_unreadable_roots.sort_by_key(|path| linux_path_depth(path));
    for unreadable_root in rootless_unreadable_roots {
        append_linux_unreadable_root_args(&mut command, &unreadable_root, &allowed_write_paths)?;
    }

    Ok(command)
}

#[cfg(target_os = "linux")]
fn linux_path_to_string(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

#[cfg(target_os = "linux")]
fn linux_path_depth(path: &Path) -> usize {
    path.components().count()
}

#[cfg(target_os = "linux")]
fn validate_linux_unreadable_globs_for_future_writes(
    patterns: &[String],
    cwd: &Path,
    writable_roots: &[WritableRoot],
) -> Result<(), String> {
    for pattern in patterns {
        if !linux_pattern_has_glob_metachar(pattern) {
            continue;
        }
        let Some((search_root, _glob)) = split_linux_glob_pattern_for_search(pattern, cwd) else {
            continue;
        };
        if writable_roots.iter().any(|writable_root| {
            path_is_at_or_under_root(&writable_root.root, &search_root)
                || path_is_at_or_under_root(&search_root, &writable_root.root)
        }) {
            return Err(format!(
                "cannot enforce sandbox deny-read glob {pattern} for future paths under writable roots with the Linux bubblewrap backend"
            ));
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_pattern_has_glob_metachar(pattern: &str) -> bool {
    pattern
        .chars()
        .any(|ch| matches!(ch, '*' | '?' | '[' | ']'))
}

#[cfg(target_os = "linux")]
fn is_within_allowed_write_paths(path: &Path, allowed_write_paths: &[PathBuf]) -> bool {
    allowed_write_paths
        .iter()
        .any(|root| path.starts_with(root))
}

#[cfg(target_os = "linux")]
fn linux_canonical_target_if_symlinked_path(path: &Path) -> Option<PathBuf> {
    let mut current = PathBuf::new();
    for component in path.components() {
        use std::path::Component;
        match component {
            Component::RootDir => {
                current.push(Path::new("/"));
                continue;
            }
            Component::CurDir => continue,
            Component::ParentDir => {
                current.pop();
                continue;
            }
            Component::Normal(part) => current.push(part),
            Component::Prefix(_) => continue,
        }

        let metadata = std::fs::symlink_metadata(&current).ok()?;
        if metadata.file_type().is_symlink() {
            let target = std::fs::canonicalize(path).ok()?;
            if target.as_path() == path {
                return None;
            }
            return Some(target);
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn remap_linux_paths_for_symlink_target(
    paths: Vec<PathBuf>,
    root: &Path,
    target: &Path,
) -> Vec<PathBuf> {
    paths
        .into_iter()
        .map(|path| {
            if let Ok(relative) = path.strip_prefix(root) {
                target.join(relative)
            } else {
                path
            }
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn append_linux_mount_target_parent_dir_args(
    args: &mut Vec<String>,
    mount_target: &Path,
    anchor: &Path,
) {
    let mount_target_dir = if mount_target.is_dir() {
        mount_target
    } else if let Some(parent) = mount_target.parent() {
        parent
    } else {
        return;
    };
    let mut mount_target_dirs = mount_target_dir
        .ancestors()
        .take_while(|path| *path != anchor)
        .map(Path::to_path_buf)
        .collect::<Vec<_>>();
    mount_target_dirs.reverse();
    for dir in mount_target_dirs {
        args.push("--perms".to_string());
        args.push("555".to_string());
        args.push("--dir".to_string());
        args.push(linux_path_to_string(&dir));
    }
}

#[cfg(target_os = "linux")]
fn prepare_linux_missing_writable_roots(
    command: &mut LinuxBwrapCommand,
    writable_roots: &[WritableRoot],
) -> Result<(), String> {
    for writable_root in writable_roots {
        let root = writable_root.root.as_path();
        if root.exists() {
            continue;
        }
        let Some(first_missing) = find_first_non_existent_component(root) else {
            continue;
        };
        std::fs::create_dir_all(root).map_err(|err| {
            format!(
                "failed to create missing sandbox writable root {}: {err}",
                root.display()
            )
        })?;

        let mut created_dirs = root
            .ancestors()
            .take_while(|path| *path != first_missing)
            .map(Path::to_path_buf)
            .collect::<Vec<_>>();
        created_dirs.push(first_missing);
        created_dirs.reverse();
        command.synthetic_mount_targets.extend(
            created_dirs
                .into_iter()
                .map(LinuxSyntheticMountTarget::EmptyDirectory),
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn append_linux_read_only_subpath_args(
    command: &mut LinuxBwrapCommand,
    subpath: &Path,
    allowed_write_paths: &[PathBuf],
) -> Result<(), String> {
    if let Some(symlink) = first_writable_symlink_component_in_path(subpath, allowed_write_paths) {
        return Err(format!(
            "cannot enforce sandbox read-only path {} because it crosses writable symlink {}",
            subpath.display(),
            symlink.display()
        ));
    }
    if !subpath.exists() {
        if let Some(first_missing) = find_first_non_existent_component(subpath)
            && is_within_allowed_write_paths(&first_missing, allowed_write_paths)
        {
            append_linux_missing_read_only_subpath_args(
                command,
                &first_missing,
                allowed_write_paths,
            )?;
        }
        return Ok(());
    }

    if is_within_allowed_write_paths(subpath, allowed_write_paths) {
        if linux_protected_metadata_name(subpath).is_some()
            && linux_path_is_empty_directory(subpath)
        {
            append_linux_empty_directory_ro_bind_args(command, subpath, allowed_write_paths)?;
        } else {
            command.args.push("--ro-bind".to_string());
            command.args.push(linux_path_to_string(subpath));
            command.args.push(linux_path_to_string(subpath));
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_protected_metadata_name(path: &Path) -> Option<&str> {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| PROTECTED_METADATA_SUBPATHS.contains(name))
}

#[cfg(target_os = "linux")]
fn linux_path_is_empty_directory(path: &Path) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !metadata.file_type().is_dir() {
        return false;
    }
    std::fs::read_dir(path)
        .ok()
        .and_then(|mut entries| entries.next())
        .is_none()
}

#[cfg(target_os = "linux")]
fn append_linux_missing_read_only_subpath_args(
    command: &mut LinuxBwrapCommand,
    path: &Path,
    allowed_write_paths: &[PathBuf],
) -> Result<(), String> {
    if linux_protected_metadata_name(path).is_some() {
        append_linux_empty_directory_ro_bind_args(command, path, allowed_write_paths)?;
        command
            .synthetic_mount_targets
            .push(LinuxSyntheticMountTarget::EmptyDirectory(
                path.to_path_buf(),
            ));
    } else {
        append_linux_empty_file_bind_data_args(command, path, Some("000"), true)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn append_linux_empty_directory_ro_bind_args(
    command: &mut LinuxBwrapCommand,
    path: &Path,
    allowed_write_paths: &[PathBuf],
) -> Result<(), String> {
    let source_dir = create_linux_empty_directory_bind_source(allowed_write_paths)?;
    command.args.push("--dir".to_string());
    command.args.push(linux_path_to_string(path));
    command.args.push("--ro-bind".to_string());
    command.args.push(linux_path_to_string(source_dir.path()));
    command.args.push(linux_path_to_string(path));
    command.preserved_tempdirs.push(source_dir);
    Ok(())
}

#[cfg(target_os = "linux")]
fn create_linux_empty_directory_bind_source(
    allowed_write_paths: &[PathBuf],
) -> Result<tempfile::TempDir, String> {
    let mut errors = Vec::new();
    for parent in linux_empty_directory_bind_source_parents() {
        let Some(parent) = ensure_absolute(parent) else {
            continue;
        };
        if linux_path_is_within_allowed_write_paths_or_canonical(&parent, allowed_write_paths) {
            errors.push(format!(
                "{} is inside a sandbox writable root",
                parent.display()
            ));
            continue;
        }
        if let Err(err) = std::fs::create_dir_all(&parent) {
            errors.push(format!("{}: {err}", parent.display()));
            continue;
        }
        let tempdir = match Builder::new()
            .prefix("mcp-repl-bwrap-empty-dir-")
            .tempdir_in(&parent)
        {
            Ok(tempdir) => tempdir,
            Err(err) => {
                errors.push(format!("{}: {err}", parent.display()));
                continue;
            }
        };
        if linux_path_is_within_allowed_write_paths_or_canonical(
            tempdir.path(),
            allowed_write_paths,
        ) {
            errors.push(format!(
                "{} is inside a sandbox writable root",
                tempdir.path().display()
            ));
            drop(tempdir);
            continue;
        }
        return Ok(tempdir);
    }

    let detail = if errors.is_empty() {
        String::new()
    } else {
        format!(": {}", errors.join("; "))
    };
    Err(format!(
        "failed to create host-only empty directory for Linux bubblewrap read-only bind source outside writable roots{detail}"
    ))
}

#[cfg(target_os = "linux")]
fn linux_empty_directory_bind_source_parents() -> Vec<PathBuf> {
    let mut parents = Vec::new();
    if let Some(home) = std::env::var_os("HOME")
        && !home.is_empty()
    {
        parents.push(
            PathBuf::from(home)
                .join(".cache")
                .join("mcp-repl")
                .join("bwrap"),
        );
    }
    if let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR")
        && !runtime_dir.is_empty()
    {
        parents.push(PathBuf::from(runtime_dir).join("mcp-repl-bwrap"));
    }
    parents.push(PathBuf::from("/var/tmp"));
    parents.push(std::env::temp_dir());
    parents.sort();
    parents.dedup();
    parents
}

#[cfg(target_os = "linux")]
fn linux_path_is_within_allowed_write_paths_or_canonical(
    path: &Path,
    allowed_write_paths: &[PathBuf],
) -> bool {
    if is_within_allowed_write_paths(path, allowed_write_paths) {
        return true;
    }
    std::fs::canonicalize(path)
        .ok()
        .is_some_and(|canonical| is_within_allowed_write_paths(&canonical, allowed_write_paths))
}

#[cfg(target_os = "linux")]
fn append_linux_empty_file_bind_data_args(
    command: &mut LinuxBwrapCommand,
    path: &Path,
    perms: Option<&str>,
    synthetic: bool,
) -> Result<(), String> {
    let empty_file_source_index = if let Some(index) = command.empty_file_source_index {
        index
    } else {
        command
            .preserved_files
            .push(std::fs::File::open("/dev/null").map_err(|err| err.to_string())?);
        let index = command.preserved_files.len() - 1;
        command.empty_file_source_index = Some(index);
        index
    };
    if let Some(perms) = perms {
        command.args.push("--perms".to_string());
        command.args.push(perms.to_string());
    }
    let fd = command.preserved_files[empty_file_source_index]
        .as_raw_fd()
        .to_string();
    command.args.push("--ro-bind-data".to_string());
    command.args.push(fd);
    command.args.push(linux_path_to_string(path));
    if synthetic {
        command
            .synthetic_mount_targets
            .push(LinuxSyntheticMountTarget::EmptyFile(path.to_path_buf()));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn make_linux_files_inheritable(files: &[std::fs::File]) -> Result<(), String> {
    for file in files {
        let fd = file.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let result = unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) };
        if result < 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_run_bwrap_child_with_cleanup(
    command: Vec<std::ffi::OsString>,
    synthetic_mount_targets: Vec<LinuxSyntheticMountTarget>,
    preserved_files: Vec<std::fs::File>,
    preserved_tempdirs: Vec<tempfile::TempDir>,
) -> Result<(), String> {
    let setup_signal_mask = LinuxBwrapForwardedSignalMask::block()?;
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    if pid == 0 {
        reset_linux_bwrap_forwarded_signal_handlers_to_default();
        if let Err(err) = setup_signal_mask.restore() {
            eprintln!("failed to restore signal mask before bubblewrap exec: {err}");
            unsafe { libc::_exit(1) };
        }
        let setpgid_result = unsafe { libc::setpgid(0, 0) };
        if setpgid_result < 0 {
            eprintln!(
                "failed to place bubblewrap child in its own process group: {}",
                std::io::Error::last_os_error()
            );
            unsafe { libc::_exit(1) };
        }
        if let Err(err) = linux_execvp(command) {
            eprintln!("{err}");
            unsafe { libc::_exit(1) };
        }
        unsafe { libc::_exit(0) };
    }

    let signal_forwarders = LinuxBwrapForwardedSignalHandlers::install(pid)?;
    setup_signal_mask.restore()?;
    let status = linux_wait_for_child(pid)?;
    let cleanup_signal_mask = LinuxBwrapForwardedSignalMask::block()?;
    LINUX_BWRAP_CHILD_PID.store(0, Ordering::SeqCst);
    let cleanup_result = cleanup_linux_synthetic_mount_targets(&synthetic_mount_targets);
    let restore_result = signal_forwarders.restore();
    let mask_restore_result = cleanup_signal_mask.restore();
    drop(preserved_files);
    drop(preserved_tempdirs);
    cleanup_result?;
    restore_result?;
    mask_restore_result?;
    linux_exit_with_wait_status(status);
}

#[cfg(target_os = "linux")]
struct LinuxBwrapForwardedSignalMask {
    previous: libc::sigset_t,
}

#[cfg(target_os = "linux")]
impl LinuxBwrapForwardedSignalMask {
    fn block() -> Result<Self, String> {
        let mut blocked: libc::sigset_t = unsafe { std::mem::zeroed() };
        let mut previous: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe {
            libc::sigemptyset(&mut blocked);
            for signal in LINUX_BWRAP_FORWARDED_SIGNALS {
                libc::sigaddset(&mut blocked, *signal);
            }
            if libc::sigprocmask(libc::SIG_BLOCK, &blocked, &mut previous) < 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
        }
        Ok(Self { previous })
    }

    fn restore(&self) -> Result<(), String> {
        let restored = self.previous;
        let result =
            unsafe { libc::sigprocmask(libc::SIG_SETMASK, &restored, std::ptr::null_mut()) };
        if result < 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
struct LinuxBwrapForwardedSignalHandlers {
    previous: Vec<(libc::c_int, libc::sigaction)>,
}

#[cfg(target_os = "linux")]
impl LinuxBwrapForwardedSignalHandlers {
    fn install(pid: libc::pid_t) -> Result<Self, String> {
        LINUX_BWRAP_CHILD_PID.store(pid, Ordering::SeqCst);
        let mut previous = Vec::with_capacity(LINUX_BWRAP_FORWARDED_SIGNALS.len());
        for signal in LINUX_BWRAP_FORWARDED_SIGNALS {
            let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
            let mut previous_action: libc::sigaction = unsafe { std::mem::zeroed() };
            action.sa_sigaction =
                forward_signal_to_linux_bwrap_child as *const () as libc::sighandler_t;
            unsafe {
                libc::sigemptyset(&mut action.sa_mask);
                if libc::sigaction(*signal, &action, &mut previous_action) < 0 {
                    return Err(std::io::Error::last_os_error().to_string());
                }
            }
            previous.push((*signal, previous_action));
        }
        replay_pending_linux_bwrap_signal(pid);
        Ok(Self { previous })
    }

    fn restore(self) -> Result<(), String> {
        LINUX_BWRAP_CHILD_PID.store(0, Ordering::SeqCst);
        LINUX_BWRAP_PENDING_FORWARDED_SIGNAL.store(0, Ordering::SeqCst);
        for (signal, previous_action) in self.previous {
            let result = unsafe { libc::sigaction(signal, &previous_action, std::ptr::null_mut()) };
            if result < 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
extern "C" fn forward_signal_to_linux_bwrap_child(signal: libc::c_int) {
    LINUX_BWRAP_PENDING_FORWARDED_SIGNAL.store(signal, Ordering::SeqCst);
    let pid = LINUX_BWRAP_CHILD_PID.load(Ordering::SeqCst);
    if pid > 0 {
        send_signal_to_linux_bwrap_child(pid, signal);
    }
}

#[cfg(target_os = "linux")]
fn replay_pending_linux_bwrap_signal(pid: libc::pid_t) {
    let signal = LINUX_BWRAP_PENDING_FORWARDED_SIGNAL.swap(0, Ordering::SeqCst);
    if signal > 0 {
        send_signal_to_linux_bwrap_child(pid, signal);
    }
}

#[cfg(target_os = "linux")]
fn send_signal_to_linux_bwrap_child(pid: libc::pid_t, signal: libc::c_int) {
    unsafe {
        libc::kill(-pid, signal);
        libc::kill(pid, signal);
    }
}

#[cfg(target_os = "linux")]
fn reset_linux_bwrap_forwarded_signal_handlers_to_default() {
    for signal in LINUX_BWRAP_FORWARDED_SIGNALS {
        unsafe {
            libc::signal(*signal, libc::SIG_DFL);
        }
    }
}

#[cfg(target_os = "linux")]
fn linux_wait_for_child(pid: libc::pid_t) -> Result<libc::c_int, String> {
    loop {
        let mut status = 0;
        let result = unsafe { libc::waitpid(pid, &mut status, 0) };
        if result >= 0 {
            return Ok(status);
        }
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        return Err(err.to_string());
    }
}

#[cfg(target_os = "linux")]
fn linux_exit_with_wait_status(status: libc::c_int) -> ! {
    if libc::WIFEXITED(status) {
        process::exit(libc::WEXITSTATUS(status));
    }
    if libc::WIFSIGNALED(status) {
        let signal = libc::WTERMSIG(status);
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
            libc::kill(libc::getpid(), signal);
        }
        process::exit(128 + signal);
    }
    process::exit(1);
}

#[cfg(target_os = "linux")]
fn cleanup_linux_synthetic_mount_targets(
    targets: &[LinuxSyntheticMountTarget],
) -> Result<(), String> {
    for target in targets.iter().rev() {
        match target {
            LinuxSyntheticMountTarget::EmptyFile(path) => {
                cleanup_linux_synthetic_empty_file(path)?;
            }
            LinuxSyntheticMountTarget::EmptyDirectory(path) => {
                cleanup_linux_synthetic_empty_directory(path)?;
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn cleanup_linux_synthetic_empty_file(path: &Path) -> Result<(), String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.to_string()),
    };
    if metadata.file_type().is_file() && metadata.len() == 0 {
        std::fs::remove_file(path).map_err(|err| err.to_string())?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn cleanup_linux_synthetic_empty_directory(path: &Path) -> Result<(), String> {
    match std::fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::DirectoryNotEmpty => Ok(()),
        Err(err) => Err(err.to_string()),
    }
}

#[cfg(target_os = "linux")]
fn append_linux_unreadable_root_args(
    command: &mut LinuxBwrapCommand,
    unreadable_root: &Path,
    allowed_write_paths: &[PathBuf],
) -> Result<(), String> {
    if let Some(symlink) =
        first_writable_symlink_component_in_path(unreadable_root, allowed_write_paths)
    {
        return Err(format!(
            "cannot enforce sandbox deny-read path {} because it crosses writable symlink {}",
            unreadable_root.display(),
            symlink.display()
        ));
    }

    if !unreadable_root.exists() {
        if let Some(first_missing) = find_first_non_existent_component(unreadable_root)
            && is_within_allowed_write_paths(&first_missing, allowed_write_paths)
        {
            append_linux_empty_file_bind_data_args(command, &first_missing, Some("000"), true)?;
        }
        return Ok(());
    }

    if unreadable_root.is_dir() {
        let mut writable_descendants = allowed_write_paths
            .iter()
            .map(PathBuf::as_path)
            .filter(|path| *path != unreadable_root && path.starts_with(unreadable_root))
            .collect::<Vec<_>>();
        command.args.push("--perms".to_string());
        command.args.push(if writable_descendants.is_empty() {
            "000".to_string()
        } else {
            "111".to_string()
        });
        command.args.push("--tmpfs".to_string());
        command.args.push(linux_path_to_string(unreadable_root));
        writable_descendants.sort_by_key(|path| linux_path_depth(path));
        for writable_descendant in writable_descendants {
            append_linux_mount_target_parent_dir_args(
                &mut command.args,
                writable_descendant,
                unreadable_root,
            );
        }
        command.args.push("--remount-ro".to_string());
        command.args.push(linux_path_to_string(unreadable_root));
    } else {
        append_linux_empty_file_bind_data_args(command, unreadable_root, Some("000"), false)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn first_writable_symlink_component_in_path(
    target_path: &Path,
    allowed_write_paths: &[PathBuf],
) -> Option<PathBuf> {
    let mut current = PathBuf::new();
    for component in target_path.components() {
        use std::path::Component;
        match component {
            Component::RootDir => {
                current.push(Path::new("/"));
                continue;
            }
            Component::CurDir => continue,
            Component::ParentDir => {
                current.pop();
                continue;
            }
            Component::Normal(part) => current.push(part),
            Component::Prefix(_) => continue,
        }

        let metadata = match std::fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(_) => break,
        };
        if metadata.file_type().is_symlink()
            && is_within_allowed_write_paths(&current, allowed_write_paths)
        {
            return Some(current);
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn expand_linux_unreadable_globs(
    patterns: &[String],
    cwd: &Path,
    max_depth: Option<usize>,
) -> Result<Vec<PathBuf>, String> {
    if patterns.is_empty() {
        return Ok(Vec::new());
    }

    let mut patterns_by_search_root = std::collections::BTreeMap::<PathBuf, Vec<String>>::new();
    let mut expanded = std::collections::BTreeSet::new();
    for pattern in patterns {
        if let Some((search_root, glob)) = split_linux_glob_pattern_for_search(pattern, cwd)
            && search_root.is_dir()
        {
            if search_root == Path::new("/") && !matches!(max_depth, Some(depth) if depth > 0) {
                return Err(format!(
                    "unreadable glob pattern {pattern} is rooted at / and requires a positive glob_scan_max_depth"
                ));
            }
            if max_depth == Some(0) {
                continue;
            }
            patterns_by_search_root
                .entry(search_root)
                .or_default()
                .push(glob);
        } else if let Some(path) = resolve_candidate_path(Path::new(pattern), cwd) {
            if let Some(target) = linux_canonical_target_if_symlinked_path(&path) {
                expanded.insert(target);
            }
            expanded.insert(path);
        }
    }

    for (search_root, globs) in patterns_by_search_root {
        for path in linux_glob_files(search_root.as_path(), &globs, max_depth)? {
            if let Some(target) = linux_canonical_target_if_symlinked_path(&path) {
                expanded.insert(target);
            }
            expanded.insert(path);
            if expanded.len() > MAX_UNREADABLE_GLOB_MATCHES {
                return Err(format!(
                    "unreadable glob expansion for {} matched more than {MAX_UNREADABLE_GLOB_MATCHES} paths",
                    search_root.display()
                ));
            }
        }
    }
    Ok(expanded.into_iter().collect())
}

#[cfg(target_os = "linux")]
fn split_linux_glob_pattern_for_search(pattern: &str, cwd: &Path) -> Option<(PathBuf, String)> {
    let absolute = resolve_candidate_path(Path::new(pattern), cwd)?;
    let pattern = absolute.to_string_lossy();
    let first_glob_index = pattern
        .char_indices()
        .find_map(|(index, ch)| matches!(ch, '*' | '?' | '[' | ']').then_some(index))?;
    let static_prefix = &pattern[..first_glob_index];
    if static_prefix.is_empty() {
        return None;
    }
    let search_root_end = if static_prefix.ends_with('/') {
        static_prefix.len() - 1
    } else {
        static_prefix.rfind('/').unwrap_or(0)
    };
    let search_root = if search_root_end == 0 {
        PathBuf::from("/")
    } else {
        PathBuf::from(&pattern[..search_root_end])
    };
    let glob = escape_unclosed_glob_classes(&pattern[search_root_end + 1..]);
    (!glob.is_empty()).then_some((search_root, glob))
}

#[cfg(target_os = "linux")]
fn escape_unclosed_glob_classes(glob: &str) -> String {
    let mut escaped = String::with_capacity(glob.len());
    let mut chars = glob.chars();
    while let Some(ch) = chars.next() {
        if ch != '[' {
            escaped.push(ch);
            continue;
        }
        let mut class = String::new();
        let mut closed = false;
        for class_ch in chars.by_ref() {
            if class_ch == ']' {
                closed = true;
                break;
            }
            class.push(class_ch);
        }
        if closed {
            escaped.push('[');
            escaped.push_str(&class);
            escaped.push(']');
        } else {
            escaped.push_str(r"\[");
            escaped.push_str(&class);
        }
    }
    escaped
}

#[cfg(target_os = "linux")]
fn linux_glob_files(
    search_root: &Path,
    globs: &[String],
    max_depth: Option<usize>,
) -> Result<Vec<PathBuf>, String> {
    let mut command = std::process::Command::new("rg");
    command
        .arg("--files")
        .arg("--hidden")
        .arg("--no-ignore")
        .arg("--null");
    if let Some(max_depth) = max_depth {
        command.arg("--max-depth").arg(max_depth.to_string());
    }
    for glob in globs {
        command.arg("--glob").arg(glob);
    }
    command.arg("--").arg(search_root);

    let output = match command.output() {
        Ok(output) => output,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return linux_glob_files_internal(search_root, globs, max_depth);
        }
        Err(err) => return Err(err.to_string()),
    };
    let mut paths = if !output.status.success() {
        if output.status.code() == Some(1) && output.stderr.is_empty() {
            Vec::new()
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "ripgrep unreadable glob scan failed for {}: {stderr}",
                search_root.display()
            ));
        }
    } else {
        output
            .stdout
            .split(|byte| *byte == b'\0')
            .filter(|path| !path.is_empty())
            .map(|path| {
                let path = PathBuf::from(std::ffi::OsString::from_vec(path.to_vec()));
                if path.is_absolute() {
                    path
                } else {
                    search_root.join(path)
                }
            })
            .collect::<Vec<_>>()
    };

    paths.extend(linux_glob_directories_internal(
        search_root,
        globs,
        max_depth,
    )?);
    Ok(paths)
}

#[cfg(target_os = "linux")]
fn linux_glob_directories_internal(
    search_root: &Path,
    globs: &[String],
    max_depth: Option<usize>,
) -> Result<Vec<PathBuf>, String> {
    let mut builder = globset::GlobSetBuilder::new();
    for glob in globs {
        let glob = globset::GlobBuilder::new(glob)
            .literal_separator(true)
            .allow_unclosed_class(true)
            .build()
            .map_err(|err| {
                format!(
                    "unreadable glob pattern is invalid for {}: {err}",
                    search_root.display()
                )
            })?;
        builder.add(glob);
    }
    let glob_set = builder.build().map_err(|err| {
        format!(
            "unreadable glob matcher failed for {}: {err}",
            search_root.display()
        )
    })?;
    let mut paths = Vec::new();
    collect_linux_glob_directories(search_root, search_root, &glob_set, max_depth, &mut paths)?;
    Ok(paths)
}

#[cfg(target_os = "linux")]
fn collect_linux_glob_directories(
    search_root: &Path,
    dir: &Path,
    glob_set: &globset::GlobSet,
    remaining_depth: Option<usize>,
    paths: &mut Vec<PathBuf>,
) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|err| err.to_string())?;
    for entry in entries {
        let entry = entry.map_err(|err| err.to_string())?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|err| err.to_string())?;
        if !file_type.is_dir() {
            continue;
        }
        let relative = path.strip_prefix(search_root).unwrap_or(path.as_path());
        if glob_set.is_match(relative) {
            paths.push(path.clone());
        }
        let remaining_depth = match remaining_depth {
            Some(0 | 1) => continue,
            Some(depth) => Some(depth - 1),
            None => None,
        };
        collect_linux_glob_directories(search_root, &path, glob_set, remaining_depth, paths)?;
    }
    Ok(())
}
#[cfg(target_os = "linux")]
fn linux_glob_files_internal(
    search_root: &Path,
    globs: &[String],
    max_depth: Option<usize>,
) -> Result<Vec<PathBuf>, String> {
    let mut builder = globset::GlobSetBuilder::new();
    for glob in globs {
        let glob = globset::GlobBuilder::new(glob)
            .literal_separator(true)
            .allow_unclosed_class(true)
            .build()
            .map_err(|err| {
                format!(
                    "unreadable glob pattern is invalid for {}: {err}",
                    search_root.display()
                )
            })?;
        builder.add(glob);
    }
    let glob_set = builder.build().map_err(|err| {
        format!(
            "unreadable glob matcher failed for {}: {err}",
            search_root.display()
        )
    })?;
    let mut paths = Vec::new();
    collect_linux_glob_files(search_root, search_root, &glob_set, max_depth, &mut paths)?;
    Ok(paths)
}

#[cfg(target_os = "linux")]
fn collect_linux_glob_files(
    search_root: &Path,
    dir: &Path,
    glob_set: &globset::GlobSet,
    remaining_depth: Option<usize>,
    paths: &mut Vec<PathBuf>,
) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|err| err.to_string())?;
    for entry in entries {
        let entry = entry.map_err(|err| err.to_string())?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|err| err.to_string())?;
        let relative = path.strip_prefix(search_root).unwrap_or(path.as_path());
        if (file_type.is_file() || file_type.is_symlink() || file_type.is_dir())
            && glob_set.is_match(relative)
        {
            paths.push(path.clone());
        }
        if !file_type.is_dir() {
            continue;
        }
        let remaining_depth = match remaining_depth {
            Some(0 | 1) => continue,
            Some(depth) => Some(depth - 1),
            None => None,
        };
        collect_linux_glob_files(search_root, &path, glob_set, remaining_depth, paths)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn find_first_non_existent_component(target_path: &Path) -> Option<PathBuf> {
    let mut current = PathBuf::new();
    for component in target_path.components() {
        use std::path::Component;
        match component {
            Component::RootDir => {
                current.push(Path::new("/"));
                continue;
            }
            Component::CurDir => continue,
            Component::ParentDir => {
                current.pop();
                continue;
            }
            Component::Normal(part) => current.push(part),
            Component::Prefix(_) => continue,
        }
        if !current.exists() {
            return Some(current);
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn is_proc_mount_failure(stderr: &str) -> bool {
    stderr.contains("Can't mount proc") || stderr.contains("mount proc")
}

#[cfg(target_os = "linux")]
fn is_bwrap_proc_probe_quiet_retry_failure(stderr: &str) -> bool {
    stderr.contains("Can't bind mount /oldroot/")
        && stderr.contains(" on /newroot/")
        && (stderr.contains("Unable to mount source on destination: No such file or directory")
            || stderr
                .contains("Unable to remount destination with correct flags: Invalid argument"))
}

#[cfg(target_os = "linux")]
fn linux_apply_sandbox_policy_to_current_thread(
    sandbox_policy: &SandboxPolicy,
    cwd: &Path,
    session_temp_dir: &Path,
    apply_landlock_fs: bool,
) -> Result<(), String> {
    let file_system_policy = linux_effective_file_system_policy(sandbox_policy, session_temp_dir);
    if !file_system_policy.has_full_disk_write_access() || !sandbox_policy.has_full_network_access()
    {
        linux_set_no_new_privs()?;
    }

    if !sandbox_policy.has_full_network_access() {
        linux_install_network_seccomp_filter_on_current_thread(
            LinuxNetworkSeccompMode::Restricted,
        )?;
    }

    if apply_landlock_fs && !file_system_policy.has_full_disk_write_access() {
        if !file_system_policy.has_full_disk_read_access() {
            return Err(
                "restricted read access is not supported by the legacy Linux Landlock filesystem backend"
                    .to_string(),
            );
        }
        let writable_roots =
            linux_landlock_writable_root_paths(&file_system_policy, cwd, session_temp_dir)?;
        linux_install_filesystem_landlock_rules_on_current_thread(writable_roots)?;
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_landlock_writable_root_paths(
    file_system_policy: &FileSystemSandboxPolicy,
    cwd: &Path,
    session_temp_dir: &Path,
) -> Result<Vec<PathBuf>, String> {
    let writable_roots =
        file_system_policy.get_writable_roots_with_cwd(cwd, Some(session_temp_dir));
    for writable_root in &writable_roots {
        if matches!(
            sandbox_path_relation(&writable_root.root, session_temp_dir),
            Some(SandboxPathRelation::Same)
        ) {
            continue;
        }
        if !writable_root.read_only_subpaths.is_empty()
            || !writable_root.protected_metadata_names.is_empty()
        {
            return Err(format!(
                "read-only carveouts inside writable root {} are not supported by the legacy Linux Landlock filesystem backend",
                writable_root.root.display()
            ));
        }
    }
    Ok(writable_roots.into_iter().map(|root| root.root).collect())
}

#[cfg(target_os = "linux")]
fn linux_set_no_new_privs() -> Result<(), String> {
    let result = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if result != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_install_filesystem_landlock_rules_on_current_thread(
    writable_roots: Vec<PathBuf>,
) -> Result<(), String> {
    use landlock::{
        ABI, Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr, RulesetCreatedAttr,
    };

    let abi = ABI::V5;
    let access_rw = AccessFs::from_all(abi);
    let access_ro = AccessFs::from_read(abi);

    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(access_rw)
        .map_err(|err| err.to_string())?
        .create()
        .map_err(|err| err.to_string())?
        .add_rules(landlock::path_beneath_rules(&["/"], access_ro))
        .map_err(|err| err.to_string())?
        .add_rules(landlock::path_beneath_rules(&["/dev/null"], access_rw))
        .map_err(|err| err.to_string())?
        .set_no_new_privs(true);

    if !writable_roots.is_empty() {
        ruleset = ruleset
            .add_rules(landlock::path_beneath_rules(&writable_roots, access_rw))
            .map_err(|err| err.to_string())?;
    }

    let status = ruleset.restrict_self().map_err(|err| err.to_string())?;
    if status.ruleset == landlock::RulesetStatus::NotEnforced {
        return Err("landlock ruleset not enforced".to_string());
    }

    Ok(())
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinuxNetworkSeccompMode {
    Restricted,
    #[allow(dead_code)]
    ProxyRouted,
}

#[cfg(target_os = "linux")]
fn linux_install_network_seccomp_filter_on_current_thread(
    mode: LinuxNetworkSeccompMode,
) -> Result<(), String> {
    use seccompiler::{
        BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
        SeccompRule, TargetArch, apply_filter,
    };
    use std::collections::BTreeMap;

    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    let mut deny_syscall = |nr: i64| {
        rules.insert(nr, vec![]);
    };

    deny_syscall(libc::SYS_ptrace);
    deny_syscall(libc::SYS_process_vm_readv);
    deny_syscall(libc::SYS_process_vm_writev);
    deny_syscall(libc::SYS_io_uring_setup);
    deny_syscall(libc::SYS_io_uring_enter);
    deny_syscall(libc::SYS_io_uring_register);

    match mode {
        LinuxNetworkSeccompMode::Restricted => {
            deny_syscall(libc::SYS_connect);
            deny_syscall(libc::SYS_accept);
            deny_syscall(libc::SYS_accept4);
            deny_syscall(libc::SYS_bind);
            deny_syscall(libc::SYS_listen);
            deny_syscall(libc::SYS_getpeername);
            deny_syscall(libc::SYS_getsockname);
            deny_syscall(libc::SYS_shutdown);
            deny_syscall(libc::SYS_sendto);
            deny_syscall(libc::SYS_sendmmsg);
            deny_syscall(libc::SYS_recvmmsg);
            deny_syscall(libc::SYS_getsockopt);
            deny_syscall(libc::SYS_setsockopt);

            let unix_only_rule = SeccompRule::new(vec![
                SeccompCondition::new(
                    0,
                    SeccompCmpArgLen::Dword,
                    SeccompCmpOp::Ne,
                    libc::AF_UNIX as u64,
                )
                .map_err(|err| err.to_string())?,
            ])
            .map_err(|err| err.to_string())?;

            rules.insert(libc::SYS_socket, vec![unix_only_rule.clone()]);
            rules.insert(libc::SYS_socketpair, vec![unix_only_rule]);
        }
        LinuxNetworkSeccompMode::ProxyRouted => {
            let deny_non_ip_socket = SeccompRule::new(vec![
                SeccompCondition::new(
                    0,
                    SeccompCmpArgLen::Dword,
                    SeccompCmpOp::Ne,
                    libc::AF_INET as u64,
                )
                .map_err(|err| err.to_string())?,
                SeccompCondition::new(
                    0,
                    SeccompCmpArgLen::Dword,
                    SeccompCmpOp::Ne,
                    libc::AF_INET6 as u64,
                )
                .map_err(|err| err.to_string())?,
            ])
            .map_err(|err| err.to_string())?;
            let deny_non_unix_socketpair = SeccompRule::new(vec![
                SeccompCondition::new(
                    0,
                    SeccompCmpArgLen::Dword,
                    SeccompCmpOp::Ne,
                    libc::AF_UNIX as u64,
                )
                .map_err(|err| err.to_string())?,
            ])
            .map_err(|err| err.to_string())?;
            rules.insert(libc::SYS_socket, vec![deny_non_ip_socket]);
            rules.insert(libc::SYS_socketpair, vec![deny_non_unix_socketpair]);
        }
    }

    let arch = if cfg!(target_arch = "x86_64") {
        TargetArch::x86_64
    } else if cfg!(target_arch = "aarch64") {
        TargetArch::aarch64
    } else {
        return Err("unsupported architecture for seccomp filter".to_string());
    };

    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        arch,
    )
    .map_err(|err| err.to_string())?;

    let prog: BpfProgram = filter
        .try_into()
        .map_err(|err: seccompiler::BackendError| err.to_string())?;
    apply_filter(&prog).map_err(|err: seccompiler::Error| err.to_string())?;

    Ok(())
}

#[cfg(target_os = "windows")]
pub fn invoked_as_codex_windows_sandbox() -> bool {
    std::env::args_os().nth(1).as_deref() == Some(OsStr::new("--windows-sandbox"))
}

#[cfg(target_os = "windows")]
pub fn invoked_as_codex_windows_sandbox_logon_offline() -> bool {
    std::env::args_os().nth(1).as_deref() == Some(OsStr::new("--windows-sandbox-logon-offline"))
}

#[cfg(target_os = "windows")]
pub fn run_windows_sandbox_main() -> ! {
    match windows_sandbox_main_impl() {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(1);
        }
    }
}

#[cfg(target_os = "windows")]
pub fn run_windows_sandbox_logon_offline_main() -> ! {
    let child_args = std::env::args_os()
        .skip(2)
        .map(|arg| arg.to_string_lossy().to_string())
        .collect::<Vec<_>>();
    match crate::windows_sandbox_setup::run_offline_logon_wrapper(child_args) {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(1);
        }
    }
}

#[cfg(target_os = "windows")]
fn windows_sandbox_main_impl() -> Result<i32, String> {
    let args = windows_sandbox_parse_args()?;
    crate::windows_sandbox::run_sandboxed_command(
        &args.sandbox_policy,
        &args.sandbox_policy_cwd,
        &args.command,
        &args.prepared_capability_sid,
    )
}

#[cfg(target_os = "windows")]
struct WindowsSandboxArgs {
    sandbox_policy_cwd: PathBuf,
    sandbox_policy: SandboxPolicy,
    prepared_capability_sid: String,
    command: Vec<String>,
}

#[cfg(target_os = "windows")]
fn windows_sandbox_parse_args() -> Result<WindowsSandboxArgs, String> {
    windows_sandbox_parse_args_from(std::env::args_os().skip(1).collect())
}

#[cfg(target_os = "windows")]
fn windows_sandbox_parse_args_from(raw_args: Vec<OsString>) -> Result<WindowsSandboxArgs, String> {
    let mut sandbox_policy_cwd: Option<PathBuf> = None;
    let mut sandbox_policy: Option<SandboxPolicy> = None;
    let mut prepared_capability_sid: Option<String> = None;
    let mut command: Vec<String> = Vec::new();

    let mut args = raw_args.into_iter().peekable();
    while let Some(arg) = args.next() {
        if arg == "--windows-sandbox" {
            continue;
        }
        if arg == "--sandbox-policy-cwd" {
            let value = args
                .next()
                .ok_or_else(|| "missing value for --sandbox-policy-cwd".to_string())?;
            sandbox_policy_cwd = Some(PathBuf::from(value));
            continue;
        }
        if arg == "--sandbox-policy" {
            let value = args
                .next()
                .ok_or_else(|| "missing value for --sandbox-policy".to_string())?;
            let value = value
                .into_string()
                .map_err(|_| "--sandbox-policy must be valid UTF-8".to_string())?;
            sandbox_policy = Some(
                serde_json::from_str(&value)
                    .map_err(|err| format!("failed to parse --sandbox-policy: {err}"))?,
            );
            continue;
        }
        if arg == "--prepared-capability-sid" {
            let value = args
                .next()
                .ok_or_else(|| "missing value for --prepared-capability-sid".to_string())?;
            prepared_capability_sid = Some(
                value
                    .into_string()
                    .map_err(|_| "--prepared-capability-sid must be valid UTF-8".to_string())?,
            );
            continue;
        }
        if arg == "--" {
            command.extend(args.map(|value| value.to_string_lossy().to_string()));
            break;
        }
        return Err(format!("unknown argument: {}", arg.to_string_lossy()));
    }

    let sandbox_policy_cwd =
        sandbox_policy_cwd.ok_or_else(|| "missing --sandbox-policy-cwd".to_string())?;
    let sandbox_policy = sandbox_policy.ok_or_else(|| "missing --sandbox-policy".to_string())?;
    let prepared_capability_sid =
        prepared_capability_sid.ok_or_else(|| "missing --prepared-capability-sid".to_string())?;
    if command.is_empty() {
        return Err("no command specified to execute".to_string());
    }

    Ok(WindowsSandboxArgs {
        sandbox_policy_cwd,
        sandbox_policy,
        prepared_capability_sid,
        command,
    })
}

#[cfg(target_os = "windows")]
pub fn append_windows_prepared_capability_sid(
    args: &mut Vec<String>,
    capability_sid: &str,
) -> Result<(), String> {
    let separator_index = args
        .iter()
        .position(|arg| arg == "--")
        .ok_or_else(|| "windows sandbox args missing command separator".to_string())?;
    args.insert(separator_index, "--prepared-capability-sid".to_string());
    args.insert(separator_index + 1, capability_sid.to_string());
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_execvp(command: Vec<std::ffi::OsString>) -> Result<(), String> {
    let cstrings: Vec<CString> = command
        .iter()
        .map(|arg| {
            CString::new(arg.as_os_str().as_bytes()).map_err(|_| "NUL byte in arg".to_string())
        })
        .collect::<Result<_, _>>()?;
    let mut ptrs: Vec<*const libc::c_char> = cstrings.iter().map(|arg| arg.as_ptr()).collect();
    ptrs.push(std::ptr::null());

    unsafe {
        libc::execvp(cstrings[0].as_ptr(), ptrs.as_ptr());
    }

    Err(format!(
        "failed to execvp {}: {}",
        PathBuf::from(&command[0]).display(),
        std::io::Error::last_os_error()
    ))
}

#[cfg(target_os = "macos")]
fn confstr(name: libc::c_int) -> Option<String> {
    let mut buf = vec![0_i8; (libc::PATH_MAX as usize) + 1];
    let len = unsafe { libc::confstr(name, buf.as_mut_ptr(), buf.len()) };
    if len == 0 {
        return None;
    }
    let cstr = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) };
    cstr.to_str().ok().map(ToString::to_string)
}

#[cfg(target_os = "macos")]
fn confstr_path(name: libc::c_int) -> Option<PathBuf> {
    let s = confstr(name)?;
    let path = PathBuf::from(s);
    path.canonicalize().ok().or(Some(path))
}

#[cfg(target_os = "macos")]
fn macos_dir_params() -> Vec<(String, PathBuf)> {
    if let Some(p) = confstr_path(libc::_CS_DARWIN_USER_CACHE_DIR) {
        return vec![("DARWIN_USER_CACHE_DIR".to_string(), p)];
    }
    vec![]
}

#[cfg(target_os = "macos")]
fn log_denials_enabled() -> bool {
    std::env::var_os(SANDBOX_LOG_DENIALS_ENV).is_some()
}

#[cfg(target_os = "macos")]
pub use macos_denials::{DenialLogger, SandboxDenial};

#[cfg(target_os = "macos")]
mod macos_denials {
    use std::collections::HashSet;
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};
    use std::thread::JoinHandle;

    pub struct SandboxDenial {
        pub name: String,
        pub capability: String,
    }

    pub struct DenialLogger {
        log_stream: Child,
        pid_tracker: Option<PidTracker>,
        log_reader: Option<JoinHandle<Vec<u8>>>,
    }

    impl DenialLogger {
        pub(crate) fn new() -> Option<Self> {
            let mut log_stream = start_log_stream()?;
            let stdout = log_stream.stdout.take()?;
            let log_reader = std::thread::spawn(move || {
                let mut reader = BufReader::new(stdout);
                let mut logs = Vec::new();
                let mut chunk = Vec::new();
                loop {
                    match reader.read_until(b'\n', &mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            logs.extend_from_slice(&chunk);
                            chunk.clear();
                        }
                    }
                }
                logs
            });

            Some(Self {
                log_stream,
                pid_tracker: None,
                log_reader: Some(log_reader),
            })
        }

        pub(crate) fn on_child_spawn(&mut self, child: &Child) {
            let root_pid = child.id() as i32;
            if root_pid > 0 {
                self.pid_tracker = PidTracker::new(root_pid);
            }
        }

        pub(crate) fn finish(mut self) -> Vec<SandboxDenial> {
            let pid_set = match self.pid_tracker {
                Some(tracker) => tracker.stop(),
                None => Default::default(),
            };

            if pid_set.is_empty() {
                return Vec::new();
            }

            let _ = self.log_stream.kill();
            let _ = self.log_stream.wait();

            let logs_bytes = match self.log_reader.take() {
                Some(handle) => handle.join().unwrap_or_default(),
                None => Vec::new(),
            };
            let logs = String::from_utf8_lossy(&logs_bytes);

            let mut seen: HashSet<(String, String)> = HashSet::new();
            let mut denials = Vec::new();
            for line in logs.lines() {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(line)
                    && let Some(msg) = json.get("eventMessage").and_then(|v| v.as_str())
                    && let Some((pid, name, capability)) = parse_message(msg)
                    && pid_set.contains(&pid)
                    && seen.insert((name.clone(), capability.clone()))
                {
                    denials.push(SandboxDenial { name, capability });
                }
            }
            denials
        }
    }

    fn start_log_stream() -> Option<Child> {
        const PREDICATE: &str = r#"(((processID == 0) AND (senderImagePath CONTAINS "/Sandbox")) OR (subsystem == "com.apple.sandbox.reporting"))"#;

        Command::new("log")
            .args(["stream", "--style", "ndjson", "--predicate", PREDICATE])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()
    }

    fn parse_message(msg: &str) -> Option<(i32, String, String)> {
        static RE: std::sync::OnceLock<regex_lite::Regex> = std::sync::OnceLock::new();
        let re = RE.get_or_init(|| {
            regex_lite::Regex::new(r"^Sandbox:\s*(.+?)\((\d+)\)\s+deny\(.*?\)\s*(.+)$")
                .expect("failed to compile sandbox denial regex")
        });

        let (_, [name, pid_str, capability]) = re.captures(msg)?.extract();
        let pid = pid_str.trim().parse::<i32>().ok()?;
        Some((pid, name.to_string(), capability.to_string()))
    }

    struct PidTracker {
        kq: libc::c_int,
        handle: JoinHandle<HashSet<i32>>,
    }

    impl PidTracker {
        fn new(root_pid: i32) -> Option<Self> {
            if root_pid <= 0 {
                return None;
            }

            let kq = unsafe { libc::kqueue() };
            let handle = std::thread::spawn(move || track_descendants(kq, root_pid));

            Some(Self { kq, handle })
        }

        fn stop(self) -> HashSet<i32> {
            trigger_stop_event(self.kq);
            self.handle.join().unwrap_or_default()
        }
    }

    unsafe extern "C" {
        fn proc_listchildpids(
            ppid: libc::c_int,
            buffer: *mut libc::c_void,
            buffersize: libc::c_int,
        ) -> libc::c_int;
    }

    fn list_child_pids(parent: i32) -> Vec<i32> {
        unsafe {
            let mut capacity: usize = 16;
            loop {
                let mut buf: Vec<i32> = vec![0; capacity];
                let count = proc_listchildpids(
                    parent as libc::c_int,
                    buf.as_mut_ptr() as *mut libc::c_void,
                    (buf.len() * std::mem::size_of::<i32>()) as libc::c_int,
                );
                if count <= 0 {
                    return Vec::new();
                }
                let returned = count as usize;
                if returned < capacity {
                    buf.truncate(returned);
                    return buf;
                }
                capacity = capacity.saturating_mul(2).max(returned + 16);
            }
        }
    }

    fn pid_is_alive(pid: i32) -> bool {
        if pid <= 0 {
            return false;
        }
        let res = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if res == 0 {
            true
        } else {
            matches!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EPERM)
            )
        }
    }

    enum WatchPidError {
        ProcessGone,
        Other(std::io::Error),
    }

    fn watch_pid(kq: libc::c_int, pid: i32) -> Result<(), WatchPidError> {
        if pid <= 0 {
            return Err(WatchPidError::ProcessGone);
        }

        let kev = libc::kevent {
            ident: pid as libc::uintptr_t,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_CLEAR,
            fflags: libc::NOTE_FORK | libc::NOTE_EXEC | libc::NOTE_EXIT,
            data: 0,
            udata: std::ptr::null_mut(),
        };

        let res = unsafe { libc::kevent(kq, &kev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
        if res < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ESRCH) {
                Err(WatchPidError::ProcessGone)
            } else {
                Err(WatchPidError::Other(err))
            }
        } else {
            Ok(())
        }
    }

    fn watch_children(
        kq: libc::c_int,
        parent: i32,
        seen: &mut HashSet<i32>,
        active: &mut HashSet<i32>,
    ) {
        for child_pid in list_child_pids(parent) {
            add_pid_watch(kq, child_pid, seen, active);
        }
    }

    fn add_pid_watch(
        kq: libc::c_int,
        pid: i32,
        seen: &mut HashSet<i32>,
        active: &mut HashSet<i32>,
    ) {
        if pid <= 0 {
            return;
        }

        let newly_seen = seen.insert(pid);
        let mut should_recurse = newly_seen;

        if active.insert(pid) {
            match watch_pid(kq, pid) {
                Ok(()) => {
                    should_recurse = true;
                }
                Err(WatchPidError::ProcessGone) => {
                    active.remove(&pid);
                    return;
                }
                Err(WatchPidError::Other(err)) => {
                    eprintln!("failed to watch pid {pid}: {err}");
                    active.remove(&pid);
                    return;
                }
            }
        }

        if should_recurse {
            watch_children(kq, pid, seen, active);
        }
    }

    const STOP_IDENT: libc::uintptr_t = 1;

    fn register_stop_event(kq: libc::c_int) -> bool {
        let kev = libc::kevent {
            ident: STOP_IDENT,
            filter: libc::EVFILT_USER,
            flags: libc::EV_ADD | libc::EV_CLEAR,
            fflags: 0,
            data: 0,
            udata: std::ptr::null_mut(),
        };

        let res = unsafe { libc::kevent(kq, &kev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
        res >= 0
    }

    fn trigger_stop_event(kq: libc::c_int) {
        if kq < 0 {
            return;
        }

        let kev = libc::kevent {
            ident: STOP_IDENT,
            filter: libc::EVFILT_USER,
            flags: 0,
            fflags: libc::NOTE_TRIGGER,
            data: 0,
            udata: std::ptr::null_mut(),
        };

        let _ = unsafe { libc::kevent(kq, &kev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
    }

    fn track_descendants(kq: libc::c_int, root_pid: i32) -> HashSet<i32> {
        if kq < 0 {
            let mut seen = HashSet::new();
            seen.insert(root_pid);
            return seen;
        }

        if !register_stop_event(kq) {
            let mut seen = HashSet::new();
            seen.insert(root_pid);
            let _ = unsafe { libc::close(kq) };
            return seen;
        }

        let mut seen: HashSet<i32> = HashSet::new();
        let mut active: HashSet<i32> = HashSet::new();

        add_pid_watch(kq, root_pid, &mut seen, &mut active);

        const EVENTS_CAP: usize = 32;
        let mut events: [libc::kevent; EVENTS_CAP] =
            unsafe { std::mem::MaybeUninit::zeroed().assume_init() };

        let mut stop_requested = false;
        loop {
            if active.is_empty() {
                if !pid_is_alive(root_pid) {
                    break;
                }
                add_pid_watch(kq, root_pid, &mut seen, &mut active);
                if active.is_empty() {
                    continue;
                }
            }

            let nev = unsafe {
                libc::kevent(
                    kq,
                    std::ptr::null::<libc::kevent>(),
                    0,
                    events.as_mut_ptr(),
                    EVENTS_CAP as libc::c_int,
                    std::ptr::null(),
                )
            };

            if nev < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }

            if nev == 0 {
                continue;
            }

            for ev in events.iter().take(nev as usize) {
                let pid = ev.ident as i32;

                if ev.filter == libc::EVFILT_USER && ev.ident == STOP_IDENT {
                    stop_requested = true;
                    break;
                }

                if (ev.flags & libc::EV_ERROR) != 0 {
                    if ev.data == libc::ESRCH as isize {
                        active.remove(&pid);
                    }
                    continue;
                }

                if (ev.fflags & libc::NOTE_FORK) != 0 {
                    watch_children(kq, pid, &mut seen, &mut active);
                }

                if (ev.fflags & libc::NOTE_EXIT) != 0 {
                    active.remove(&pid);
                }
            }

            if stop_requested {
                break;
            }
        }

        let _ = unsafe { libc::close(kq) };

        seen
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[cfg(target_os = "macos")]
    use std::collections::HashMap;
    use std::path::Path;
    use std::path::PathBuf;
    #[cfg(target_os = "linux")]
    use std::sync::{Mutex, OnceLock};

    #[cfg(target_os = "linux")]
    fn linux_bwrap_env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .expect("linux bwrap env lock poisoned")
    }

    #[test]
    fn session_temp_dir_rejects_outside_system_tmp() {
        #[cfg(target_os = "windows")]
        let outside = {
            let system_drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
            PathBuf::from(format!(r"{system_drive}\mcp-repl-test"))
        };
        #[cfg(not(target_os = "windows"))]
        let base_tmp = std::env::temp_dir();
        #[cfg(not(target_os = "windows"))]
        let outside = if base_tmp.starts_with("/tmp") {
            PathBuf::from("/var/mcp-repl-test")
        } else {
            PathBuf::from("/tmp/mcp-repl-test")
        };
        let err = prepare_session_temp_dir(&outside).expect_err("expected failure");
        match err {
            SandboxError::SessionTempDir(message) => {
                assert!(
                    message.contains("outside system temp"),
                    "unexpected error message: {message}"
                );
            }
            #[cfg(target_os = "macos")]
            SandboxError::SeatbeltMissing => {
                panic!("unexpected error: SeatbeltMissing")
            }
            #[cfg(target_os = "linux")]
            SandboxError::LinuxSandbox(message) => {
                panic!("unexpected error: {message}")
            }
            #[cfg(target_os = "windows")]
            SandboxError::WindowsSandbox(message) => {
                panic!("unexpected error: {message}")
            }
        }
    }

    #[test]
    fn prepare_worker_command_preserves_existing_session_tempdir_contents() {
        let session_temp_dir = std::env::temp_dir().join(format!(
            "mcp-repl-session-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        prepare_session_temp_dir(&session_temp_dir).expect("prepare session temp dir");
        let marker = session_temp_dir.join("marker.txt");
        std::fs::write(&marker, "keep").expect("write marker");

        let state = SandboxState {
            session_temp_dir: session_temp_dir.clone(),
            ..SandboxState::default()
        };
        let _ = prepare_worker_command(Path::new("echo"), vec!["ok".to_string()], &state)
            .expect("prepare worker command");

        assert!(
            marker.exists(),
            "prepare_worker_command should not reset the session temp dir"
        );

        std::fs::remove_file(&marker).expect("remove marker");
        std::fs::remove_dir_all(&session_temp_dir).expect("cleanup session temp dir");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn proxy_loopback_ports_from_env_extracts_loopback_endpoints() {
        let mut env = HashMap::new();
        env.insert(
            "HTTP_PROXY".to_string(),
            "http://127.0.0.1:8080".to_string(),
        );
        env.insert("HTTPS_PROXY".to_string(), "https://localhost".to_string());
        env.insert(
            "ALL_PROXY".to_string(),
            "http://example.com:3128".to_string(),
        );

        let ports = proxy_loopback_ports_from_env(&env);
        assert_eq!(ports, vec![443, 8080]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn dynamic_network_policy_fails_closed_when_proxy_config_has_no_loopback_endpoint() {
        let mut env = HashMap::new();
        env.insert(
            "HTTP_PROXY".to_string(),
            "http://example.com:3128".to_string(),
        );
        let proxy = proxy_policy_inputs_from_env(&env);

        let policy = SandboxPolicy::WorkspaceWrite {
            writable_roots: Vec::new(),
            network_access: true,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
        };
        let rendered = dynamic_network_policy(&policy, false, false, &proxy);
        assert!(rendered.is_empty(), "expected fail-closed policy");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn dynamic_network_policy_fails_closed_for_managed_network_without_proxy() {
        let env = HashMap::new();
        let proxy = proxy_policy_inputs_from_env(&env);
        let policy = SandboxPolicy::WorkspaceWrite {
            writable_roots: Vec::new(),
            network_access: true,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
        };

        let rendered = dynamic_network_policy(&policy, true, false, &proxy);
        assert!(rendered.is_empty(), "expected fail-closed policy");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn dynamic_network_policy_allows_proxy_only_outbound_when_configured() {
        let mut env = HashMap::new();
        env.insert(
            "HTTP_PROXY".to_string(),
            "http://127.0.0.1:8080".to_string(),
        );
        let proxy = proxy_policy_inputs_from_env(&env);
        let policy = SandboxPolicy::WorkspaceWrite {
            writable_roots: Vec::new(),
            network_access: true,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
        };

        let rendered = dynamic_network_policy(&policy, false, false, &proxy);
        assert!(rendered.contains("localhost:8080"));
        assert!(!rendered.contains("(allow network-inbound)\n"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn prepare_worker_command_respects_legacy_workspace_temp_exclusions() {
        let ambient_tmpdir = std::env::temp_dir();
        let root = ambient_tmpdir.join(format!("mcp-repl-temp-exclusions-{}", std::process::id()));
        let session_temp_dir = root.join("session-tempdir");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");

        let state = SandboxState {
            sandbox_policy: SandboxPolicy::WorkspaceWrite {
                writable_roots: vec![workspace.clone()],
                network_access: false,
                exclude_tmpdir_env_var: true,
                exclude_slash_tmp: true,
            },
            sandbox_cwd: workspace,
            session_temp_dir: session_temp_dir.clone(),
            ..SandboxState::default()
        };
        let prepared =
            prepare_worker_command(Path::new("/bin/echo"), vec!["ok".to_string()], &state);

        let prepared = prepared.expect("prepare_worker_command should succeed");
        let writable_roots = prepared
            .args
            .iter()
            .filter_map(|arg| arg.strip_prefix("-DWRITABLE_ROOT_"))
            .filter_map(|definition| {
                definition
                    .split_once('=')
                    .map(|(_, value)| PathBuf::from(value))
            })
            .collect::<Vec<_>>();

        let required_session_variants = sandbox_path_variants(&session_temp_dir);
        assert!(
            required_session_variants
                .iter()
                .any(|path| writable_roots.iter().any(|root| root == path)),
            "session temp dir must stay writable: {writable_roots:?}"
        );

        let mut excluded_temp_variants = sandbox_path_variants(Path::new("/tmp"));
        excluded_temp_variants.extend(sandbox_path_variants(&ambient_tmpdir));
        assert!(
            excluded_temp_variants
                .iter()
                .all(|path| !writable_roots.iter().any(|root| root == path)),
            "excluded temp roots must not be re-added as writable: {writable_roots:?}"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn prepare_worker_command_with_managed_proxy_injects_proxy_env_and_seatbelt_ports() {
        let proxy = crate::managed_network::ManagedNetworkProxy::start(
            crate::managed_network::ManagedProxyConfig {
                allowed_domains: vec!["example.com".to_string()],
                denied_domains: Vec::new(),
                allow_local_binding: false,
            },
        )
        .expect("managed proxy");
        let mut state = SandboxState {
            sandbox_policy: SandboxPolicy::WorkspaceWrite {
                writable_roots: Vec::new(),
                network_access: true,
                exclude_tmpdir_env_var: false,
                exclude_slash_tmp: false,
            },
            ..SandboxState::default()
        };
        state.managed_network_policy.allowed_domains = vec!["example.com".to_string()];

        let prepared = prepare_worker_command_with_managed_network(
            Path::new("/bin/echo"),
            vec!["ok".to_string()],
            &state,
            Some(&proxy),
        )
        .expect("prepare worker command");

        assert_eq!(
            prepared.env.get("HTTP_PROXY").map(String::as_str),
            Some(format!("http://{}", proxy.http_addr()).as_str())
        );
        assert_eq!(
            prepared.env.get("ALL_PROXY").map(String::as_str),
            Some(format!("socks5h://{}", proxy.socks_addr()).as_str())
        );
        let policy = prepared
            .args
            .windows(2)
            .find(|pair| pair[0] == "-p")
            .map(|pair| pair[1].as_str())
            .expect("seatbelt policy argument");
        assert!(
            policy.contains(&format!("localhost:{}", proxy.http_addr().port())),
            "{policy}"
        );
        assert!(
            policy.contains(&format!("localhost:{}", proxy.socks_addr().port())),
            "{policy}"
        );
        assert!(
            !policy.contains("(allow network-outbound)\n(allow network-inbound)\n"),
            "{policy}"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn prepare_worker_command_with_managed_proxy_uses_session_temp_home() {
        let proxy = crate::managed_network::ManagedNetworkProxy::start(
            crate::managed_network::ManagedProxyConfig {
                allowed_domains: vec!["example.com".to_string()],
                denied_domains: Vec::new(),
                allow_local_binding: false,
            },
        )
        .expect("managed proxy");
        let tmp = Builder::new()
            .prefix("mcp-repl-offline-home-test-")
            .tempdir()
            .expect("tempdir");
        let session_temp_dir = tmp.path().join("session");
        let mut state = SandboxState {
            sandbox_policy: SandboxPolicy::WorkspaceWrite {
                writable_roots: Vec::new(),
                network_access: true,
                exclude_tmpdir_env_var: false,
                exclude_slash_tmp: false,
            },
            session_temp_dir: session_temp_dir.clone(),
            ..SandboxState::default()
        };
        state.managed_network_policy.allowed_domains = vec!["example.com".to_string()];

        let prepared = prepare_worker_command_with_managed_network(
            Path::new("worker.exe"),
            vec!["worker".to_string()],
            &state,
            Some(&proxy),
        )
        .expect("prepare worker command");
        let session_temp = session_temp_dir.to_string_lossy().to_string();

        assert_eq!(
            prepared.env.get("HOME").map(String::as_str),
            Some(session_temp.as_str())
        );
        assert_eq!(
            prepared.env.get("R_USER").map(String::as_str),
            Some(session_temp.as_str())
        );
        assert_eq!(
            prepared.env.get("USERPROFILE").map(String::as_str),
            Some(session_temp.as_str())
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn prepare_worker_command_with_managed_proxy_routes_local_targets_through_proxy() {
        let proxy = crate::managed_network::ManagedNetworkProxy::start(
            crate::managed_network::ManagedProxyConfig {
                allowed_domains: vec!["example.com".to_string()],
                denied_domains: Vec::new(),
                allow_local_binding: true,
            },
        )
        .expect("managed proxy");
        let tmp = Builder::new()
            .prefix("mcp-repl-offline-proxy-test-")
            .tempdir()
            .expect("tempdir");
        let mut state = SandboxState {
            sandbox_policy: SandboxPolicy::WorkspaceWrite {
                writable_roots: Vec::new(),
                network_access: true,
                exclude_tmpdir_env_var: false,
                exclude_slash_tmp: false,
            },
            session_temp_dir: tmp.path().join("session"),
            ..SandboxState::default()
        };
        state.managed_network_policy.allowed_domains = vec!["example.com".to_string()];
        state.managed_network_policy.allow_local_binding = true;

        let prepared = prepare_worker_command_with_managed_network(
            Path::new("worker.exe"),
            vec!["worker".to_string()],
            &state,
            Some(&proxy),
        )
        .expect("prepare worker command");

        assert_eq!(prepared.env.get("NO_PROXY").map(String::as_str), Some(""));
        assert_eq!(prepared.env.get("no_proxy").map(String::as_str), Some(""));
        assert_eq!(
            prepared.env.get("npm_config_noproxy").map(String::as_str),
            Some("")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_mount_failure_detects_expected_stderr() {
        assert!(is_proc_mount_failure(
            "bwrap: Can't mount proc on /newroot/proc: Invalid argument"
        ));
        assert!(!is_proc_mount_failure("bwrap: unrelated failure"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bwrap_proc_probe_quiet_retry_detects_old_bind_target_stderr() {
        assert!(is_bwrap_proc_probe_quiet_retry_failure(
            "bwrap: Can't bind mount /oldroot/home/runner/.cache/mcp-repl/bwrap/mcp-repl-bwrap-empty-dir-bm2Cuc on /newroot/home/runner/work/mcp-repl/mcp-repl/.agents: Unable to mount source on destination: No such file or directory"
        ));
        assert!(is_bwrap_proc_probe_quiet_retry_failure(
            "bwrap: Can't bind mount /oldroot/home/runner/.cache/mcp-repl/bwrap/mcp-repl-bwrap-empty-dir-FlEdOV on /newroot/home/runner/work/mcp-repl/mcp-repl/.agents: Unable to remount destination with correct flags: Invalid argument"
        ));
        assert!(!is_bwrap_proc_probe_quiet_retry_failure(
            "bwrap: Unable to remount destination with correct flags: Invalid argument"
        ));
        assert!(!is_bwrap_proc_probe_quiet_retry_failure(
            "bwrap: unrelated failure"
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_root_anchored_unreadable_globs_split_under_root() {
        let cwd = Path::new("/tmp");

        assert_eq!(
            split_linux_glob_pattern_for_search("/*", cwd),
            Some((PathBuf::from("/"), "*".to_string()))
        );
        assert_eq!(
            split_linux_glob_pattern_for_search("/**/*.pem", cwd),
            Some((PathBuf::from("/"), "**/*.pem".to_string()))
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_root_anchored_unreadable_globs_without_bounded_scan_fail_closed() {
        let patterns = vec!["/*".to_string()];
        let err = expand_linux_unreadable_globs(&patterns, Path::new("/tmp"), None)
            .expect_err("root-anchored glob without max depth should fail closed");
        assert!(
            err.contains("requires a positive glob_scan_max_depth"),
            "unexpected error: {err}"
        );

        let patterns = vec!["/**/*.pem".to_string()];
        let err = expand_linux_unreadable_globs(&patterns, Path::new("/tmp"), Some(0))
            .expect_err("root-anchored glob with disabled scan should fail closed");
        assert!(
            err.contains("requires a positive glob_scan_max_depth"),
            "unexpected error: {err}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_unreadable_wildcard_globs_under_writable_roots_fail_closed() {
        let writable_roots = vec![WritableRoot {
            root: PathBuf::from("/tmp/mcp-repl-writable"),
            read_only_subpaths: Vec::new(),
            protected_metadata_names: Vec::new(),
        }];
        let patterns = vec!["/tmp/mcp-repl-writable/**/*.env".to_string()];

        let err = validate_linux_unreadable_globs_for_future_writes(
            &patterns,
            Path::new("/tmp/mcp-repl-writable"),
            &writable_roots,
        )
        .expect_err("writable wildcard glob should fail closed");

        assert!(
            err.contains("cannot enforce sandbox deny-read glob"),
            "unexpected error: {err}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_landlock_writable_roots_with_carveouts_fail_closed() {
        let writable_root = PathBuf::from("/tmp/mcp-repl-landlock-writable");
        let session_temp_dir = PathBuf::from("/tmp/mcp-repl-landlock-session");
        let file_system_policy = FileSystemSandboxPolicy::restricted(vec![
            FileSystemSandboxEntry {
                path: FileSystemPath::Special {
                    value: FileSystemSpecialPath::Root,
                },
                access: FileSystemAccessMode::Read,
            },
            FileSystemSandboxEntry {
                path: FileSystemPath::Path {
                    path: writable_root.clone(),
                },
                access: FileSystemAccessMode::Write,
            },
        ]);

        let err = linux_landlock_writable_root_paths(
            &file_system_policy,
            &writable_root,
            &session_temp_dir,
        )
        .expect_err("legacy Landlock should reject writable roots with protected carveouts");

        assert!(
            err.contains("read-only carveouts inside writable root"),
            "unexpected error: {err}"
        );
        assert!(
            err.contains(&writable_root.display().to_string()),
            "error should identify the widened writable root: {err}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_landlock_allows_internal_session_temp_writable_root() {
        let cwd = PathBuf::from("/tmp/mcp-repl-landlock-workspace");
        let session_temp_dir = PathBuf::from("/tmp/mcp-repl-landlock-session");
        let file_system_policy = linux_effective_file_system_policy(
            &SandboxPolicy::ReadOnly {
                network_access: false,
            },
            &session_temp_dir,
        );

        let writable_roots =
            linux_landlock_writable_root_paths(&file_system_policy, &cwd, &session_temp_dir)
                .expect("session temp writable root should not make legacy Landlock fail closed");

        assert_eq!(writable_roots, vec![session_temp_dir]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_literal_unreadable_globs_include_missing_future_path() {
        let cwd = Path::new("/tmp/mcp-repl-literal-glob");
        let patterns = vec!["secret.env".to_string()];

        let expanded = expand_linux_unreadable_globs(&patterns, cwd, None)
            .expect("literal missing glob should expand to a concrete path");

        assert_eq!(expanded, vec![cwd.join("secret.env")]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_internal_glob_scanner_includes_matching_directories() {
        let root = Builder::new()
            .prefix("mcp-repl-deny-glob-dir-")
            .tempdir()
            .expect("tempdir");
        let denied_dir = root.path().join("secrets");
        let denied_child = denied_dir.join("token.txt");
        std::fs::create_dir_all(&denied_dir).expect("create denied dir");
        std::fs::write(&denied_child, "secret").expect("write denied child");
        std::fs::write(root.path().join("secrets.txt"), "secret").expect("write denied file");

        let expanded = linux_glob_files_internal(root.path(), &["secrets*".to_string()], None)
            .expect("fallback glob scan should succeed");

        let denied_file = root.path().join("secrets.txt");
        assert!(
            expanded.iter().any(|path| path == &denied_dir),
            "fallback glob scan should include matching directories: {expanded:?}"
        );
        assert!(
            expanded.iter().any(|path| path == &denied_file),
            "fallback glob scan should still include matching files: {expanded:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_glob_scanner_includes_matching_directories() {
        let root = Builder::new()
            .prefix("mcp-repl-deny-glob-rg-dir-")
            .tempdir()
            .expect("tempdir");
        let denied_dir = root.path().join("secrets");
        let denied_child = denied_dir.join("token.txt");
        std::fs::create_dir_all(&denied_dir).expect("create denied dir");
        std::fs::write(&denied_child, "secret").expect("write denied child");
        std::fs::write(root.path().join("secrets.txt"), "secret").expect("write denied file");

        let expanded = linux_glob_files(root.path(), &["secrets*".to_string()], None)
            .expect("glob scan should succeed");

        let denied_file = root.path().join("secrets.txt");
        assert!(
            expanded.iter().any(|path| path == &denied_dir),
            "glob scan should include matching directories: {expanded:?}"
        );
        assert!(
            expanded.iter().any(|path| path == &denied_file),
            "glob scan should still include matching files: {expanded:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_bwrap_minimal_with_deny_keeps_platform_defaults_readable() {
        let Some(platform_root) = LINUX_PLATFORM_DEFAULT_READ_ROOTS
            .iter()
            .map(Path::new)
            .find(|path| path.exists())
        else {
            eprintln!("no Linux platform default roots exist; skipping");
            return;
        };
        let cwd = Path::new("/tmp/mcp-repl-minimal-deny-workspace");
        let session_temp_dir = Path::new("/tmp/mcp-repl-minimal-deny-session");
        let denied_path = cwd.join("secret.txt");
        let file_system_policy = FileSystemSandboxPolicy::restricted(vec![
            FileSystemSandboxEntry {
                path: FileSystemPath::Special {
                    value: FileSystemSpecialPath::Minimal,
                },
                access: FileSystemAccessMode::Read,
            },
            FileSystemSandboxEntry {
                path: FileSystemPath::Special {
                    value: FileSystemSpecialPath::ProjectRoots { subpath: None },
                },
                access: FileSystemAccessMode::Read,
            },
            FileSystemSandboxEntry {
                path: FileSystemPath::Special {
                    value: FileSystemSpecialPath::Tmpdir,
                },
                access: FileSystemAccessMode::Write,
            },
            FileSystemSandboxEntry {
                path: FileSystemPath::Path { path: denied_path },
                access: FileSystemAccessMode::Deny,
            },
        ]);

        let command = linux_bwrap_filesystem_args(&file_system_policy, cwd, session_temp_dir)
            .expect("minimal deny profile should build bwrap args");
        let platform_root = linux_path_to_string(platform_root);

        assert!(
            command.args.windows(3).any(|args| args[0] == "--ro-bind"
                && args[1] == platform_root
                && args[2] == platform_root),
            "minimal deny profile should mount platform runtime roots: {:?}",
            command.args
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_bwrap_project_roots_read_keeps_platform_defaults_readable() {
        let Some(platform_root) = LINUX_PLATFORM_DEFAULT_READ_ROOTS
            .iter()
            .map(Path::new)
            .find(|path| path.exists())
        else {
            eprintln!("no Linux platform default roots exist; skipping");
            return;
        };
        let root = Builder::new()
            .prefix("mcp-repl-project-roots-read-")
            .tempdir()
            .expect("tempdir");
        let session_temp_dir = root.path().join("session");
        std::fs::create_dir_all(&session_temp_dir).expect("create session temp dir");
        let file_system_policy = FileSystemSandboxPolicy::restricted(vec![
            FileSystemSandboxEntry {
                path: FileSystemPath::Special {
                    value: FileSystemSpecialPath::ProjectRoots { subpath: None },
                },
                access: FileSystemAccessMode::Read,
            },
            FileSystemSandboxEntry {
                path: FileSystemPath::Special {
                    value: FileSystemSpecialPath::Tmpdir,
                },
                access: FileSystemAccessMode::Write,
            },
        ]);

        let command = linux_bwrap_filesystem_args(
            &file_system_policy,
            root.path(),
            session_temp_dir.as_path(),
        )
        .expect("project-roots read profile should build bwrap args");
        let platform_root = linux_path_to_string(platform_root);

        assert!(
            command.args.windows(3).any(|args| args[0] == "--ro-bind"
                && args[1] == platform_root
                && args[2] == platform_root),
            "project-roots read profile should mount platform runtime roots: {:?}",
            command.args
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_bwrap_missing_protected_metadata_uses_namespace_dir_and_read_only_empty_bind() {
        let root = Builder::new()
            .prefix("mcp-repl-bwrap-protected-metadata-")
            .tempdir()
            .expect("tempdir");
        let codex_path = root.path().join(".codex");
        let session_temp_dir = root.path().join("session");
        let file_system_policy = FileSystemSandboxPolicy::restricted(vec![
            FileSystemSandboxEntry {
                path: FileSystemPath::Special {
                    value: FileSystemSpecialPath::Root,
                },
                access: FileSystemAccessMode::Read,
            },
            FileSystemSandboxEntry {
                path: FileSystemPath::Path {
                    path: root.path().to_path_buf(),
                },
                access: FileSystemAccessMode::Write,
            },
            FileSystemSandboxEntry {
                path: FileSystemPath::Path {
                    path: codex_path.clone(),
                },
                access: FileSystemAccessMode::Read,
            },
        ]);

        let command =
            linux_bwrap_filesystem_args(&file_system_policy, root.path(), &session_temp_dir)
                .expect("missing protected metadata should build bwrap args");
        let codex_text = linux_path_to_string(&codex_path);
        let dir_index = command
            .args
            .windows(2)
            .position(|args| args[0] == "--dir" && args[1] == codex_text)
            .expect("missing protected metadata should create its mount target inside bwrap");
        let bind_index = command
            .args
            .windows(3)
            .position(|args| args[0] == "--ro-bind" && args[2] == codex_text)
            .expect("missing protected metadata should use a read-only empty directory bind");
        assert!(
            dir_index < bind_index,
            "protected metadata mount target must exist before the bind: {:?}",
            command.args
        );
        let bind_args = command
            .args
            .windows(3)
            .find(|args| args[0] == "--ro-bind" && args[2] == codex_text)
            .expect("missing protected metadata should use a read-only empty directory bind");
        let source_path = PathBuf::from(&bind_args[1]);

        assert!(
            !codex_path.exists(),
            "missing protected metadata target should be created inside bwrap, not on the host"
        );
        assert!(
            command
                .preserved_tempdirs
                .iter()
                .any(|tempdir| tempdir.path() == source_path),
            "directory bind should reference a preserved empty source directory"
        );
        assert!(
            !command
                .args
                .windows(2)
                .any(|args| args[0] == "--remount-ro" && args[1] == codex_text),
            "missing protected metadata should not require tmpfs remount-ro: {:?}",
            command.args
        );
        let protected_target = LinuxSyntheticMountTarget::EmptyDirectory(codex_path.clone());
        assert!(
            command.synthetic_mount_targets.contains(&protected_target),
            "missing protected metadata target should be cleaned up after bwrap exits: {:?}",
            command.synthetic_mount_targets
        );
        assert!(
            command
                .preserved_tempdirs
                .iter()
                .all(|tempdir| !tempdir.path().starts_with(root.path())),
            "empty bind sources must stay outside sandbox writable roots"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_bwrap_empty_protected_metadata_uses_private_bind_source() {
        let root = Builder::new()
            .prefix("mcp-repl-bwrap-empty-protected-metadata-")
            .tempdir()
            .expect("tempdir");
        let codex_path = root.path().join(".codex");
        std::fs::create_dir_all(&codex_path).expect("empty metadata dir");
        let session_temp_dir = root.path().join("session");
        let file_system_policy = FileSystemSandboxPolicy::restricted(vec![
            FileSystemSandboxEntry {
                path: FileSystemPath::Special {
                    value: FileSystemSpecialPath::Root,
                },
                access: FileSystemAccessMode::Read,
            },
            FileSystemSandboxEntry {
                path: FileSystemPath::Path {
                    path: root.path().to_path_buf(),
                },
                access: FileSystemAccessMode::Write,
            },
            FileSystemSandboxEntry {
                path: FileSystemPath::Path {
                    path: codex_path.clone(),
                },
                access: FileSystemAccessMode::Read,
            },
        ]);

        let command =
            linux_bwrap_filesystem_args(&file_system_policy, root.path(), &session_temp_dir)
                .expect("empty protected metadata should build bwrap args");
        let codex_text = linux_path_to_string(&codex_path);
        let bind_args = command
            .args
            .windows(3)
            .find(|args| args[0] == "--ro-bind" && args[2] == codex_text)
            .expect("empty protected metadata should use a read-only empty directory bind");
        let source_path = PathBuf::from(&bind_args[1]);

        assert_ne!(
            source_path, codex_path,
            "empty protected metadata must not use the transient target as its bind source"
        );
        assert!(
            command
                .preserved_tempdirs
                .iter()
                .any(|tempdir| tempdir.path() == source_path),
            "directory bind should reference a preserved empty source directory"
        );
        assert!(
            !command
                .synthetic_mount_targets
                .contains(&LinuxSyntheticMountTarget::EmptyDirectory(codex_path)),
            "existing empty protected metadata should not require host cleanup: {:?}",
            command.synthetic_mount_targets
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_bwrap_temp_roots_do_not_synthesize_global_metadata_dirs() {
        let root = Builder::new()
            .prefix("mcp-repl-bwrap-temp-root-")
            .tempdir_in(std::env::temp_dir())
            .expect("tempdir");
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let session_temp_dir = root.path().join("session");
        let file_system_policy = FileSystemSandboxPolicy::workspace_write(&[], false, false);

        let command =
            linux_bwrap_filesystem_args(&file_system_policy, &workspace, &session_temp_dir)
                .expect("workspace-write profile should build bwrap args");
        let temp_roots = temp_roots_from_system(false, false);
        let global_metadata_paths = temp_roots
            .iter()
            .flat_map(|root| {
                PROTECTED_METADATA_SUBPATHS
                    .iter()
                    .map(|name| root.join(name))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();

        for metadata_path in global_metadata_paths {
            let metadata_text = linux_path_to_string(&metadata_path);
            assert!(
                !command
                    .args
                    .windows(3)
                    .any(|args| args[0] == "--ro-bind" && args[2] == metadata_text),
                "ambient temp metadata should not become a synthetic bwrap bind target: {:?}",
                command.args
            );
            assert!(
                !command
                    .synthetic_mount_targets
                    .contains(&LinuxSyntheticMountTarget::EmptyDirectory(metadata_path)),
                "ambient temp metadata should not be created and cleaned as a synthetic target"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_bwrap_read_only_keeps_private_session_metadata_binds() {
        let root = Builder::new()
            .prefix("mcp-repl-bwrap-read-only-session-")
            .tempdir()
            .expect("tempdir");
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let session_temp_dir = root.path().join("session");
        let file_system_policy = linux_effective_file_system_policy(
            &SandboxPolicy::ReadOnly {
                network_access: false,
            },
            &session_temp_dir,
        );

        let command =
            linux_bwrap_filesystem_args(&file_system_policy, &workspace, &session_temp_dir)
                .expect("read-only profile should build bwrap args");
        let git_text = linux_path_to_string(&session_temp_dir.join(".git"));

        assert!(
            command
                .args
                .windows(3)
                .any(|args| args[0] == "--ro-bind" && args[2] == git_text),
            "read-only session temp metadata should use private synthetic binds: {:?}",
            command.args
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_bwrap_command_does_not_require_newer_argv0_option() {
        let root = Builder::new()
            .prefix("mcp-repl-bwrap-argv0-")
            .tempdir()
            .expect("tempdir");
        let session_temp_dir = root.path().join("session");
        let file_system_policy = FileSystemSandboxPolicy::restricted(vec![
            FileSystemSandboxEntry {
                path: FileSystemPath::Special {
                    value: FileSystemSpecialPath::Root,
                },
                access: FileSystemAccessMode::Read,
            },
            FileSystemSandboxEntry {
                path: FileSystemPath::Path {
                    path: root.path().to_path_buf(),
                },
                access: FileSystemAccessMode::Write,
            },
        ]);

        let command = create_linux_bwrap_command_args(
            vec!["/bin/true".to_string()],
            &file_system_policy,
            root.path(),
            &session_temp_dir,
            false,
            LinuxBwrapNetworkMode::FullAccess,
        )
        .expect("workspace-write profile should build bwrap args");

        assert!(
            !command.args.iter().any(|arg| arg == "--argv0"),
            "bwrap args should work with Ubuntu 22.04 bubblewrap 0.6.1: {:?}",
            command.args
        );
    }

    #[test]
    fn prepare_worker_command_sets_allow_local_binding_one_when_enabled() {
        let mut state = SandboxState {
            sandbox_policy: SandboxPolicy::DangerFullAccess,
            ..SandboxState::default()
        };
        state.managed_network_policy.allow_local_binding = true;

        let prepared =
            prepare_worker_command(Path::new("/bin/echo"), vec!["ok".to_string()], &state)
                .expect("prepare_worker_command should succeed");
        assert_eq!(
            prepared
                .env
                .get(ALLOW_LOCAL_BINDING_ENV_KEY)
                .map(String::as_str),
            Some("1"),
            "explicit true value should enable local binding"
        );
    }

    #[test]
    fn prepare_worker_command_sets_allow_local_binding_zero_when_explicitly_disabled() {
        let mut state = SandboxState {
            sandbox_policy: SandboxPolicy::DangerFullAccess,
            ..SandboxState::default()
        };
        state.managed_network_policy.allow_local_binding = false;

        let prepared =
            prepare_worker_command(Path::new("/bin/echo"), vec!["ok".to_string()], &state)
                .expect("prepare_worker_command should succeed");
        assert_eq!(
            prepared
                .env
                .get(ALLOW_LOCAL_BINDING_ENV_KEY)
                .map(String::as_str),
            Some("0"),
            "explicit false override should disable local binding even when inherited env enables it"
        );
    }

    #[test]
    fn prepare_worker_command_clears_managed_domain_env_when_lists_are_empty() {
        let mut state = SandboxState {
            sandbox_policy: SandboxPolicy::DangerFullAccess,
            ..SandboxState::default()
        };
        state.managed_network_policy.allowed_domains = Vec::new();
        state.managed_network_policy.denied_domains = Vec::new();

        let prepared =
            prepare_worker_command(Path::new("/bin/echo"), vec!["ok".to_string()], &state)
                .expect("prepare_worker_command should succeed");

        assert_eq!(
            prepared
                .env
                .get(MANAGED_ALLOWED_DOMAINS_ENV_KEY)
                .map(String::as_str),
            Some(""),
            "allowed domains must be explicitly cleared for child processes"
        );
        assert_eq!(
            prepared
                .env
                .get(MANAGED_DENIED_DOMAINS_ENV_KEY)
                .map(String::as_str),
            Some(""),
            "denied domains must be explicitly cleared for child processes"
        );
        assert_eq!(
            prepared
                .env
                .get(MANAGED_NETWORK_ENV_KEY)
                .map(String::as_str),
            Some("0"),
            "managed network marker must be explicitly disabled when no domain restrictions exist"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn read_only_windows_command_keeps_current_user_wrapper_without_managed_proxy() {
        let session_temp_dir = std::env::temp_dir().join(format!(
            "mcp-repl-session-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let state = SandboxState {
            sandbox_policy: SandboxPolicy::ReadOnly {
                network_access: false,
            },
            session_temp_dir: session_temp_dir.clone(),
            ..SandboxState::default()
        };

        let prepared =
            prepare_worker_command(Path::new("worker.exe"), vec!["worker".to_string()], &state)
                .expect("read-only Windows worker command should prepare");

        assert!(
            prepared.args.contains(&"--windows-sandbox".to_string()),
            "read-only Windows workers should still use the Windows sandbox wrapper"
        );
        assert!(
            !prepared
                .args
                .contains(&"--windows-sandbox-logon-offline".to_string()),
            "read-only without managed proxy should stay on the current-user launch path"
        );

        let _ = std::fs::remove_dir_all(session_temp_dir);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn append_windows_prepared_capability_sid_inserts_before_command_separator() {
        let mut args = vec![
            "--windows-sandbox".to_string(),
            "--sandbox-policy-cwd".to_string(),
            "C:\\workspace".to_string(),
            "--sandbox-policy".to_string(),
            "{\"type\":\"workspace-write\"}".to_string(),
            "--".to_string(),
            "worker".to_string(),
        ];

        append_windows_prepared_capability_sid(&mut args, "S-1-5-21-1-2-3-4")
            .expect("prepared capability sid should insert");

        assert_eq!(
            args,
            vec![
                "--windows-sandbox".to_string(),
                "--sandbox-policy-cwd".to_string(),
                "C:\\workspace".to_string(),
                "--sandbox-policy".to_string(),
                "{\"type\":\"workspace-write\"}".to_string(),
                "--prepared-capability-sid".to_string(),
                "S-1-5-21-1-2-3-4".to_string(),
                "--".to_string(),
                "worker".to_string(),
            ]
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_sandbox_parse_args_accepts_prepared_capability_sid() {
        let args = vec![
            OsString::from("--windows-sandbox"),
            OsString::from("--sandbox-policy-cwd"),
            OsString::from("C:\\workspace"),
            OsString::from("--sandbox-policy"),
            OsString::from("{\"type\":\"workspace-write\"}"),
            OsString::from("--prepared-capability-sid"),
            OsString::from("S-1-5-21-1-2-3-4"),
            OsString::from("--"),
            OsString::from("worker"),
        ];

        let parsed = windows_sandbox_parse_args_from(args).expect("windows sandbox args");

        assert_eq!(parsed.prepared_capability_sid.as_str(), "S-1-5-21-1-2-3-4");
        assert_eq!(parsed.command, vec!["worker".to_string()]);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_sandbox_parse_args_requires_prepared_capability_sid() {
        let args = vec![
            OsString::from("--windows-sandbox"),
            OsString::from("--sandbox-policy-cwd"),
            OsString::from("C:\\workspace"),
            OsString::from("--sandbox-policy"),
            OsString::from("{\"type\":\"workspace-write\"}"),
            OsString::from("--"),
            OsString::from("worker"),
        ];

        let err = match windows_sandbox_parse_args_from(args) {
            Ok(_) => panic!("missing prepared capability sid should fail"),
            Err(err) => err,
        };

        assert!(
            err.contains("missing --prepared-capability-sid"),
            "expected prepared-capability-sid requirement, got: {err}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn prepare_worker_command_bwrap_env_does_not_override_explicit_false() {
        let _guard = linux_bwrap_env_lock();
        let previous_env = std::env::var_os(LINUX_BWRAP_ENABLED_ENV);
        unsafe {
            std::env::set_var(LINUX_BWRAP_ENABLED_ENV, "1");
        }

        let state = SandboxState {
            sandbox_policy: SandboxPolicy::WorkspaceWrite {
                writable_roots: Vec::new(),
                network_access: false,
                exclude_tmpdir_env_var: false,
                exclude_slash_tmp: false,
            },
            use_linux_sandbox_bwrap: false,
            ..SandboxState::default()
        };

        let prepared =
            prepare_worker_command(Path::new("/bin/echo"), vec!["ok".to_string()], &state)
                .expect("prepare_worker_command should succeed");

        match previous_env {
            Some(value) => unsafe {
                std::env::set_var(LINUX_BWRAP_ENABLED_ENV, value);
            },
            None => unsafe {
                std::env::remove_var(LINUX_BWRAP_ENABLED_ENV);
            },
        }

        assert!(
            !prepared.args.contains(&"--use-bwrap-sandbox".to_string()),
            "explicit false override should disable bwrap even when env enables it"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn prepare_worker_command_uses_internal_linux_sandbox_launcher() {
        let state = SandboxState {
            sandbox_policy: SandboxPolicy::WorkspaceWrite {
                writable_roots: Vec::new(),
                network_access: false,
                exclude_tmpdir_env_var: false,
                exclude_slash_tmp: false,
            },
            ..SandboxState::default()
        };

        let prepared =
            prepare_worker_command(Path::new("/bin/echo"), vec!["ok".to_string()], &state)
                .expect("prepare_worker_command should succeed");

        assert_eq!(
            prepared.program,
            std::env::current_exe().expect("current exe"),
            "Linux sandboxed workers should always launch through mcp-repl's internal helper"
        );
        assert_eq!(
            prepared.arg0.as_deref(),
            Some("codex-linux-sandbox"),
            "Linux sandboxed workers should set arg0 for internal helper dispatch"
        );
    }

    #[test]
    fn codex_sandbox_state_meta_parses_current_permission_profile_payload() {
        let sandbox_cwd = std::env::temp_dir().join("mcp-repl-codex-meta-cwd");
        let sandbox_cwd_uri = url::Url::from_file_path(&sandbox_cwd)
            .expect("absolute sandbox cwd should convert to file URI")
            .to_string();

        let update = sandbox_state_update_from_codex_meta(&json!({
            "permissionProfile": {
                "type": "managed",
                "file_system": {
                    "type": "restricted",
                    "entries": [
                        {
                            "path": {
                                "type": "special",
                                "value": { "kind": "root" }
                            },
                            "access": "read"
                        },
                        {
                            "path": {
                                "type": "special",
                                "value": { "kind": "project_roots" }
                            },
                            "access": "write"
                        },
                        {
                            "path": {
                                "type": "special",
                                "value": { "kind": "tmpdir" }
                            },
                            "access": "write"
                        },
                        {
                            "path": {
                                "type": "special",
                                "value": { "kind": "slash_tmp" }
                            },
                            "access": "write"
                        }
                    ]
                },
                "network": "restricted"
            },
            "sandboxCwd": sandbox_cwd_uri,
            "useLegacyLandlock": false,
            "codexLinuxSandboxExe": if cfg!(target_os = "linux") {
                serde_json::Value::String("/tmp/codex-linux-sandbox".to_string())
            } else {
                serde_json::Value::Null
            },
        }))
        .expect("current Codex sandbox metadata");

        assert_eq!(
            update.sandbox_policy,
            SandboxPolicy::WorkspaceWrite {
                writable_roots: Vec::new(),
                network_access: false,
                exclude_tmpdir_env_var: false,
                exclude_slash_tmp: false,
            }
        );
        assert_eq!(update.sandbox_cwd, Some(sandbox_cwd));
        #[cfg(target_os = "linux")]
        assert_eq!(update.use_legacy_landlock, Some(false));
        #[cfg(target_os = "linux")]
        assert!(update.use_linux_sandbox_bwrap.is_none());
        #[cfg(not(target_os = "linux"))]
        assert!(update.use_linux_sandbox_bwrap.is_none());
    }

    #[test]
    fn codex_sandbox_state_meta_use_legacy_landlock_disables_linux_bwrap() {
        let sandbox_cwd = std::env::temp_dir().join("mcp-repl-codex-meta-cwd");
        let sandbox_cwd_uri = url::Url::from_file_path(&sandbox_cwd)
            .expect("absolute sandbox cwd should convert to file URI")
            .to_string();
        let update = sandbox_state_update_from_codex_meta(&json!({
            "permissionProfile": {
                "type": "disabled"
            },
            "sandboxCwd": sandbox_cwd_uri,
            "useLegacyLandlock": true,
            "codexLinuxSandboxExe": serde_json::Value::Null,
        }))
        .expect("codex sandbox metadata");

        #[cfg(target_os = "linux")]
        assert_eq!(update.use_linux_sandbox_bwrap, Some(false));
        #[cfg(not(target_os = "linux"))]
        assert!(update.use_linux_sandbox_bwrap.is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn codex_sandbox_state_meta_non_legacy_preserves_disabled_linux_bwrap() {
        let sandbox_cwd = std::env::temp_dir().join("mcp-repl-codex-meta-cwd");
        let sandbox_cwd_uri = url::Url::from_file_path(&sandbox_cwd)
            .expect("absolute sandbox cwd should convert to file URI")
            .to_string();
        let non_legacy_update = sandbox_state_update_from_codex_meta(&json!({
            "permissionProfile": {
                "type": "disabled"
            },
            "sandboxCwd": sandbox_cwd_uri,
            "useLegacyLandlock": false,
            "codexLinuxSandboxExe": "/tmp/codex-linux-sandbox",
        }))
        .expect("non-legacy Codex sandbox metadata");
        let mut state = SandboxState {
            use_linux_sandbox_bwrap: false,
            ..SandboxState::default()
        };

        state.apply_update(non_legacy_update);

        assert!(
            !state.use_linux_sandbox_bwrap,
            "non-legacy Codex metadata should not override an explicitly disabled Linux bwrap default"
        );
    }

    #[test]
    fn codex_sandbox_state_meta_rejects_relative_workspace_write_roots() {
        let sandbox_cwd = std::env::temp_dir().join("mcp-repl-codex-meta-cwd");
        let sandbox_cwd_uri = url::Url::from_file_path(&sandbox_cwd)
            .expect("absolute sandbox cwd should convert to file URI")
            .to_string();
        let err = sandbox_state_update_from_codex_meta(&json!({
            "permissionProfile": {
                "type": "managed",
                "file_system": {
                    "type": "restricted",
                    "entries": [
                        {
                            "path": {
                                "type": "special",
                                "value": { "kind": "root" }
                            },
                            "access": "read"
                        },
                        {
                            "path": {
                                "type": "special",
                                "value": { "kind": "project_roots" }
                            },
                            "access": "write"
                        },
                        {
                            "path": {
                                "type": "path",
                                "path": "relative-root"
                            },
                            "access": "write"
                        }
                    ]
                },
                "network": "restricted"
            },
            "sandboxCwd": sandbox_cwd_uri,
            "useLegacyLandlock": false,
            "codexLinuxSandboxExe": if cfg!(target_os = "linux") {
                serde_json::Value::String("/tmp/codex-linux-sandbox".to_string())
            } else {
                serde_json::Value::Null
            },
        }))
        .expect_err("relative writable roots should fail closed");

        assert!(
            err.contains("permissionProfile.file_system.entries.path"),
            "expected path validation error, got: {err}"
        );
        assert!(
            err.contains("relative-root"),
            "expected failing relative root to be named in the error, got: {err}"
        );
    }

    #[test]
    fn codex_permission_profile_meta_maps_workspace_write() {
        let sandbox_cwd = std::env::temp_dir().join("mcp-repl-codex-meta-workspace");
        let update = sandbox_state_update_from_codex_meta(&json!({
            "permissionProfile": {
                "type": "managed",
                "file_system": {
                    "type": "restricted",
                    "entries": [
                        {
                            "path": {
                                "type": "special",
                                "value": { "kind": "root" }
                            },
                            "access": "read"
                        },
                        {
                            "path": {
                                "type": "path",
                                "path": sandbox_cwd
                            },
                            "access": "write"
                        },
                        {
                            "path": {
                                "type": "special",
                                "value": { "kind": "slash_tmp" }
                            },
                            "access": "write"
                        },
                        {
                            "path": {
                                "type": "special",
                                "value": { "kind": "tmpdir" }
                            },
                            "access": "write"
                        },
                        {
                            "path": {
                                "type": "path",
                                "path": sandbox_cwd.join(".git")
                            },
                            "access": "read"
                        }
                    ]
                },
                "network": "restricted"
            },
            "sandboxCwd": sandbox_cwd,
            "useLegacyLandlock": false,
            "codexLinuxSandboxExe": serde_json::Value::Null,
        }))
        .expect("Codex permission profile metadata should map to a legacy sandbox update");

        assert_eq!(update.sandbox_cwd.as_deref(), Some(sandbox_cwd.as_path()));
        assert_eq!(
            update.sandbox_policy,
            SandboxPolicy::WorkspaceWrite {
                writable_roots: Vec::new(),
                network_access: false,
                exclude_tmpdir_env_var: false,
                exclude_slash_tmp: false,
            }
        );
    }

    #[test]
    fn codex_permission_profile_meta_accepts_file_uri_sandbox_cwd() {
        let sandbox_cwd = std::env::temp_dir().join("mcp-repl-codex-meta-uri-cwd");
        let sandbox_cwd_uri = file_url_for_test_path(&sandbox_cwd);
        let update = sandbox_state_update_from_codex_meta(&json!({
            "permissionProfile": {
                "type": "managed",
                "file_system": {
                    "type": "restricted",
                    "entries": [
                        {
                            "path": {
                                "type": "special",
                                "value": { "kind": "root" }
                            },
                            "access": "read"
                        }
                    ]
                },
                "network": "restricted"
            },
            "sandboxCwd": sandbox_cwd_uri,
            "useLegacyLandlock": false,
            "codexLinuxSandboxExe": serde_json::Value::Null,
        }))
        .expect("file URI sandboxCwd should parse");

        assert_eq!(update.sandbox_cwd.as_deref(), Some(sandbox_cwd.as_path()));
        #[cfg(target_os = "macos")]
        assert_eq!(
            update.sandbox_policy,
            SandboxPolicy::Managed {
                file_system: FileSystemSandboxPolicy::read_only(),
                network_access: NetworkAccess::Restricted,
            }
        );
        #[cfg(not(target_os = "macos"))]
        assert_eq!(
            update.sandbox_policy,
            SandboxPolicy::ReadOnly {
                network_access: false,
            }
        );
    }

    fn file_url_for_test_path(path: &Path) -> String {
        let path = path.to_string_lossy().replace('\\', "/");
        if path.starts_with('/') {
            format!("file://{path}")
        } else {
            format!("file:///{path}")
        }
    }

    #[test]
    fn codex_permission_profile_meta_maps_disabled_to_full_access() {
        let sandbox_cwd = std::env::temp_dir().join("mcp-repl-codex-meta-full-access");
        let update = sandbox_state_update_from_codex_meta(&json!({
            "permissionProfile": {
                "type": "disabled"
            },
            "sandboxCwd": sandbox_cwd,
            "useLegacyLandlock": false,
            "codexLinuxSandboxExe": serde_json::Value::Null,
        }))
        .expect("disabled Codex permission profile should map to full access");

        assert_eq!(update.sandbox_cwd.as_deref(), Some(sandbox_cwd.as_path()));
        assert_eq!(update.sandbox_policy, SandboxPolicy::DangerFullAccess);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn codex_permission_profile_meta_accepts_minimal_read_policy() {
        let sandbox_cwd = std::env::temp_dir().join("mcp-repl-codex-meta-minimal-read");
        let update = sandbox_state_update_from_codex_meta(&json!({
            "permissionProfile": {
                "type": "managed",
                "file_system": {
                    "type": "restricted",
                    "entries": [
                        {
                            "path": {
                                "type": "special",
                                "value": { "kind": "minimal" }
                            },
                            "access": "read"
                        },
                        {
                            "path": {
                                "type": "special",
                                "value": { "kind": "tmpdir" }
                            },
                            "access": "write"
                        }
                    ]
                },
                "network": "restricted"
            },
            "sandboxCwd": sandbox_cwd,
            "useLegacyLandlock": false,
            "codexLinuxSandboxExe": serde_json::Value::Null,
        }))
        .expect("current Codex minimal read metadata should parse");

        assert!(
            !update.sandbox_policy.has_full_disk_read_access(),
            "minimal read policy must not be flattened to full read access"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn codex_permission_profile_meta_accepts_deny_read_carveout() {
        let sandbox_cwd = std::env::temp_dir().join("mcp-repl-codex-meta-deny-read");
        let private_dir = sandbox_cwd.join("private");
        let update = sandbox_state_update_from_codex_meta(&json!({
            "permissionProfile": {
                "type": "managed",
                "file_system": {
                    "type": "restricted",
                    "entries": [
                        {
                            "path": {
                                "type": "special",
                                "value": { "kind": "root" }
                            },
                            "access": "read"
                        },
                        {
                            "path": {
                                "type": "path",
                                "path": private_dir
                            },
                            "access": "deny"
                        }
                    ]
                },
                "network": "restricted"
            },
            "sandboxCwd": sandbox_cwd,
            "useLegacyLandlock": false,
            "codexLinuxSandboxExe": serde_json::Value::Null,
        }))
        .expect("current Codex deny-read metadata should parse");

        assert!(
            !update.sandbox_policy.has_full_disk_read_access(),
            "deny-read carveout must not be flattened to full read access"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn codex_permission_profile_project_read_includes_seatbelt_platform_defaults() {
        let sandbox_cwd = std::env::temp_dir().join("mcp-repl-codex-meta-project-read");
        let update = sandbox_state_update_from_codex_meta(&json!({
            "permissionProfile": {
                "type": "managed",
                "file_system": {
                    "type": "restricted",
                    "entries": [
                        {
                            "path": {
                                "type": "special",
                                "value": { "kind": "project_roots" }
                            },
                            "access": "read"
                        },
                        {
                            "path": {
                                "type": "special",
                                "value": { "kind": "tmpdir" }
                            },
                            "access": "write"
                        }
                    ]
                },
                "network": "restricted"
            },
            "sandboxCwd": sandbox_cwd,
            "useLegacyLandlock": false,
            "codexLinuxSandboxExe": serde_json::Value::Null,
        }))
        .expect("current Codex project read metadata should parse");
        let state = SandboxState {
            sandbox_policy: update.sandbox_policy,
            sandbox_cwd: update.sandbox_cwd.expect("sandbox cwd"),
            ..SandboxState::default()
        };
        let prepared =
            prepare_worker_command(Path::new("/bin/echo"), vec!["ok".to_string()], &state)
                .expect("seatbelt command should prepare");
        let policy = prepared
            .args
            .windows(2)
            .find(|pair| pair[0] == "-p")
            .map(|pair| pair[1].as_str())
            .expect("seatbelt policy argument");

        assert!(
            policy.contains(MACOS_RESTRICTED_READ_ONLY_PLATFORM_DEFAULTS),
            "expected restricted profile seatbelt policy to include platform defaults: {policy}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn codex_permission_profile_glob_deny_is_rendered_in_seatbelt_policy() {
        let sandbox_cwd = std::env::temp_dir().join("mcp-repl-codex-meta-glob-deny");
        let update = sandbox_state_update_from_codex_meta(&json!({
            "permissionProfile": {
                "type": "managed",
                "file_system": {
                    "type": "restricted",
                    "entries": [
                        {
                            "path": {
                                "type": "special",
                                "value": { "kind": "root" }
                            },
                            "access": "read"
                        },
                        {
                            "path": {
                                "type": "special",
                                "value": { "kind": "project_roots" }
                            },
                            "access": "write"
                        },
                        {
                            "path": {
                                "type": "glob_pattern",
                                "pattern": "**/*.env"
                            },
                            "access": "deny"
                        }
                    ]
                },
                "network": "restricted"
            },
            "sandboxCwd": sandbox_cwd,
            "useLegacyLandlock": false,
            "codexLinuxSandboxExe": serde_json::Value::Null,
        }))
        .expect("current Codex glob-deny metadata should parse");
        let state = SandboxState {
            sandbox_policy: update.sandbox_policy,
            sandbox_cwd: update.sandbox_cwd.expect("sandbox cwd"),
            ..SandboxState::default()
        };
        let prepared =
            prepare_worker_command(Path::new("/bin/echo"), vec!["ok".to_string()], &state)
                .expect("seatbelt command should prepare");
        let policy = prepared
            .args
            .windows(2)
            .find(|pair| pair[0] == "-p")
            .map(|pair| pair[1].as_str())
            .expect("seatbelt policy argument");

        assert!(
            policy.contains("(deny file-read* (regex #\""),
            "expected glob deny read rule in seatbelt policy: {policy}"
        );
        assert!(
            policy.contains("(deny file-write-unlink (regex #\""),
            "expected glob deny unlink rule in seatbelt policy: {policy}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn prepare_worker_command_includes_r_runtime_library_path() {
        let state = SandboxState {
            sandbox_policy: SandboxPolicy::DangerFullAccess,
            ..SandboxState::default()
        };

        let prepared =
            prepare_worker_command(Path::new("/bin/echo"), vec!["ok".to_string()], &state)
                .expect("prepare_worker_command should succeed");

        let r_home = embedded_r_home().expect("embedded R home should be discoverable");
        let r_home_text = r_home.to_string_lossy().to_string();
        let lib_dir = r_home.join("lib");
        let ld_library_path = prepared
            .env
            .get("LD_LIBRARY_PATH")
            .expect("LD_LIBRARY_PATH should be set for embedded R workers");
        let path_entries: Vec<PathBuf> =
            std::env::split_paths(&std::ffi::OsString::from(ld_library_path)).collect();

        assert_eq!(
            prepared.env.get("R_HOME"),
            Some(&r_home_text),
            "prepared command should pass through the detected R_HOME"
        );
        assert_eq!(
            path_entries.first(),
            Some(&lib_dir),
            "embedded R library dir should be first in LD_LIBRARY_PATH"
        );
        assert!(
            path_entries.iter().any(|entry| entry == &lib_dir),
            "embedded R library dir should be present in LD_LIBRARY_PATH"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn prepare_worker_command_accepts_managed_policy_on_linux() {
        let state = SandboxState {
            sandbox_policy: SandboxPolicy::Managed {
                file_system: FileSystemSandboxPolicy {
                    kind: FileSystemSandboxKind::Restricted,
                    glob_scan_max_depth: None,
                    entries: vec![FileSystemSandboxEntry {
                        path: FileSystemPath::Special {
                            value: FileSystemSpecialPath::Root,
                        },
                        access: FileSystemAccessMode::Read,
                    }],
                },
                network_access: NetworkAccess::Restricted,
            },
            ..SandboxState::default()
        };

        let prepared =
            prepare_worker_command(Path::new("/bin/echo"), vec!["ok".to_string()], &state)
                .expect("managed policies should prepare through the Linux helper");
        assert_eq!(
            prepared.program,
            std::env::current_exe().expect("current exe")
        );
        assert!(
            prepared.args.iter().any(|arg| arg == "--use-bwrap-sandbox"),
            "managed Linux policy should use the default bwrap path: {:?}",
            prepared.args
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sandbox_state_defaults_with_environment_respects_linux_bwrap_env() {
        let _guard = linux_bwrap_env_lock();
        let previous_env = std::env::var_os(LINUX_BWRAP_ENABLED_ENV);

        unsafe {
            std::env::remove_var(LINUX_BWRAP_ENABLED_ENV);
        }
        let defaults = sandbox_state_defaults_with_environment();
        assert!(
            defaults.use_linux_sandbox_bwrap,
            "unset Linux bwrap env should preserve the Linux default"
        );

        unsafe {
            std::env::set_var(LINUX_BWRAP_ENABLED_ENV, "0");
        }
        let defaults = sandbox_state_defaults_with_environment();
        assert!(
            !defaults.use_linux_sandbox_bwrap,
            "explicit false Linux bwrap env should disable bwrap"
        );

        unsafe {
            std::env::set_var(LINUX_BWRAP_ENABLED_ENV, "1");
        }
        let defaults = sandbox_state_defaults_with_environment();
        match previous_env {
            Some(value) => unsafe {
                std::env::set_var(LINUX_BWRAP_ENABLED_ENV, value);
            },
            None => unsafe {
                std::env::remove_var(LINUX_BWRAP_ENABLED_ENV);
            },
        }
        assert!(
            defaults.use_linux_sandbox_bwrap,
            "Linux bwrap env should be applied at defaults layer"
        );
    }
}
