#[cfg(target_os = "linux")]
use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use url::Url;

use super::{NetworkAccess, SandboxPolicy};

const PROTECTED_METADATA_NAMES: &[&str] = &[".git", ".agents", ".codex"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum NetworkSandboxPolicy {
    #[default]
    Restricted,
    Enabled,
}

impl NetworkSandboxPolicy {
    pub fn is_enabled(self) -> bool {
        matches!(self, NetworkSandboxPolicy::Enabled)
    }
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
    #[cfg(target_os = "linux")]
    fn can_read(self) -> bool {
        !matches!(self, FileSystemAccessMode::Deny)
    }

    fn can_write(self) -> bool {
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
    Unknown {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subpath: Option<PathBuf>,
    },
}

impl FileSystemSpecialPath {
    fn project_roots(subpath: Option<PathBuf>) -> Self {
        Self::ProjectRoots { subpath }
    }
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

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WritableRoot {
    pub root: PathBuf,
    pub read_only_subpaths: Vec<PathBuf>,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedEntry {
    path: PathBuf,
    access: FileSystemAccessMode,
}

impl Default for FileSystemSandboxPolicy {
    fn default() -> Self {
        Self::read_only()
    }
}

impl FileSystemSandboxPolicy {
    pub fn read_only() -> Self {
        Self::restricted(vec![FileSystemSandboxEntry {
            path: FileSystemPath::Special {
                value: FileSystemSpecialPath::Root,
            },
            access: FileSystemAccessMode::Read,
        }])
    }

    pub fn unrestricted() -> Self {
        Self {
            kind: FileSystemSandboxKind::Unrestricted,
            glob_scan_max_depth: None,
            entries: Vec::new(),
        }
    }

    pub fn external_sandbox() -> Self {
        Self {
            kind: FileSystemSandboxKind::ExternalSandbox,
            glob_scan_max_depth: None,
            entries: Vec::new(),
        }
    }

    pub fn restricted(entries: Vec<FileSystemSandboxEntry>) -> Self {
        Self {
            kind: FileSystemSandboxKind::Restricted,
            glob_scan_max_depth: None,
            entries,
        }
    }

    pub fn workspace_write(
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
                    value: FileSystemSpecialPath::project_roots(None),
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
        for name in PROTECTED_METADATA_NAMES {
            entries.push(FileSystemSandboxEntry {
                path: FileSystemPath::Special {
                    value: FileSystemSpecialPath::project_roots(Some(PathBuf::from(name))),
                },
                access: FileSystemAccessMode::Read,
            });
        }
        for root in writable_roots {
            for path in default_read_only_subpaths_for_writable_root(root) {
                entries.push(FileSystemSandboxEntry {
                    path: FileSystemPath::Path { path },
                    access: FileSystemAccessMode::Read,
                });
            }
        }
        Self::restricted(entries)
    }

    #[cfg(target_os = "linux")]
    pub fn has_full_disk_read_access(&self) -> bool {
        match self.kind {
            FileSystemSandboxKind::Unrestricted | FileSystemSandboxKind::ExternalSandbox => true,
            FileSystemSandboxKind::Restricted => {
                self.has_root_access(FileSystemAccessMode::can_read)
                    && !self.has_denied_read_restrictions()
            }
        }
    }

    pub fn has_full_disk_write_access(&self) -> bool {
        match self.kind {
            FileSystemSandboxKind::Unrestricted | FileSystemSandboxKind::ExternalSandbox => true,
            FileSystemSandboxKind::Restricted => {
                self.has_root_access(FileSystemAccessMode::can_write)
                    && !self.has_write_narrowing_entries()
            }
        }
    }

    #[cfg(target_os = "linux")]
    pub fn include_platform_defaults(&self) -> bool {
        !self.has_full_disk_read_access()
            && matches!(self.kind, FileSystemSandboxKind::Restricted)
            && self.entries.iter().any(|entry| {
                matches!(
                    &entry.path,
                    FileSystemPath::Special {
                        value: FileSystemSpecialPath::Minimal
                    } if entry.access.can_read()
                )
            })
    }

    #[cfg(target_os = "linux")]
    pub fn get_readable_roots_with_cwd(&self, cwd: &Path) -> Vec<PathBuf> {
        if self.has_full_disk_read_access() {
            return Vec::new();
        }
        let entries = self.resolved_entries_with_cwd(cwd);
        dedup_paths(
            entries
                .iter()
                .filter(|entry| entry.access.can_read())
                .filter(|entry| self.can_read_path_with_cwd(&entry.path, cwd))
                .map(|entry| entry.path.clone())
                .collect(),
        )
    }

    #[cfg(target_os = "linux")]
    pub fn get_writable_roots_with_cwd(&self, cwd: &Path) -> Vec<WritableRoot> {
        if self.has_full_disk_write_access() {
            return Vec::new();
        }

        let entries = self.resolved_entries_with_cwd(cwd);
        let writable_roots = dedup_paths(
            entries
                .iter()
                .filter(|entry| entry.access.can_write())
                .filter(|entry| self.can_write_path_with_cwd(&entry.path, cwd))
                .map(|entry| entry.path.clone())
                .collect(),
        );

        writable_roots
            .into_iter()
            .map(|root| {
                let mut read_only_subpaths = default_read_only_subpaths_for_writable_root(&root);
                read_only_subpaths.extend(
                    entries
                        .iter()
                        .filter(|entry| !entry.access.can_write())
                        .filter(|entry| entry.path.starts_with(&root))
                        .filter(|entry| !self.can_write_path_with_cwd(&entry.path, cwd))
                        .map(|entry| entry.path.clone()),
                );
                WritableRoot {
                    root,
                    read_only_subpaths: dedup_paths(read_only_subpaths),
                }
            })
            .collect()
    }

    #[cfg(target_os = "linux")]
    pub fn get_unreadable_roots_with_cwd(&self, cwd: &Path) -> Vec<PathBuf> {
        if !matches!(self.kind, FileSystemSandboxKind::Restricted) {
            return Vec::new();
        }
        dedup_paths(
            self.resolved_entries_with_cwd(cwd)
                .into_iter()
                .filter(|entry| entry.access == FileSystemAccessMode::Deny)
                .filter(|entry| !self.can_read_path_with_cwd(&entry.path, cwd))
                .map(|entry| entry.path)
                .collect(),
        )
    }

    #[cfg(target_os = "linux")]
    pub fn can_read_path_with_cwd(&self, path: &Path, cwd: &Path) -> bool {
        self.resolve_access_with_cwd(path, cwd).can_read()
    }

    #[cfg(target_os = "linux")]
    pub fn can_write_path_with_cwd(&self, path: &Path, cwd: &Path) -> bool {
        if !self.resolve_access_with_cwd(path, cwd).can_write() {
            return false;
        }
        if self.has_full_disk_write_access() {
            return true;
        }
        !self.metadata_write_denied(path, cwd)
    }

    #[cfg(target_os = "linux")]
    pub fn with_additional_writable_root(mut self, root: PathBuf) -> Self {
        if !matches!(self.kind, FileSystemSandboxKind::Restricted) {
            return self;
        }
        self.entries.push(FileSystemSandboxEntry {
            path: FileSystemPath::Path { path: root },
            access: FileSystemAccessMode::Write,
        });
        self
    }

    pub fn to_legacy_sandbox_policy(
        &self,
        network: NetworkSandboxPolicy,
        cwd: &Path,
    ) -> SandboxPolicy {
        match self.kind {
            FileSystemSandboxKind::ExternalSandbox => SandboxPolicy::ExternalSandbox {
                network_access: network.into(),
            },
            FileSystemSandboxKind::Unrestricted => {
                if network.is_enabled() {
                    SandboxPolicy::DangerFullAccess
                } else {
                    SandboxPolicy::ExternalSandbox {
                        network_access: NetworkAccess::Restricted,
                    }
                }
            }
            FileSystemSandboxKind::Restricted => {
                if self.has_full_disk_write_access() {
                    return if network.is_enabled() {
                        SandboxPolicy::DangerFullAccess
                    } else {
                        SandboxPolicy::ExternalSandbox {
                            network_access: NetworkAccess::Restricted,
                        }
                    };
                }

                let mut workspace_writable = false;
                let mut writable_roots = Vec::new();
                let mut tmpdir_writable = false;
                let mut slash_tmp_writable = false;
                for entry in &self.entries {
                    if !entry.access.can_write() {
                        continue;
                    }
                    match &entry.path {
                        FileSystemPath::Special {
                            value: FileSystemSpecialPath::ProjectRoots { subpath: None },
                        } => workspace_writable = true,
                        FileSystemPath::Special {
                            value: FileSystemSpecialPath::Tmpdir,
                        } => tmpdir_writable = true,
                        FileSystemPath::Special {
                            value: FileSystemSpecialPath::SlashTmp,
                        } => slash_tmp_writable = true,
                        _ => match resolve_file_system_path(&entry.path, cwd) {
                            Some(path) if path == cwd => workspace_writable = true,
                            Some(path) => writable_roots.push(path),
                            None => {}
                        },
                    }
                }

                if workspace_writable {
                    writable_roots.retain(|root| root != cwd);
                    SandboxPolicy::WorkspaceWrite {
                        writable_roots: dedup_paths(writable_roots),
                        network_access: network.is_enabled(),
                        exclude_tmpdir_env_var: !tmpdir_writable,
                        exclude_slash_tmp: !slash_tmp_writable,
                    }
                } else {
                    SandboxPolicy::ReadOnly {
                        network_access: network.is_enabled(),
                    }
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn resolve_access_with_cwd(&self, path: &Path, cwd: &Path) -> FileSystemAccessMode {
        match self.kind {
            FileSystemSandboxKind::Unrestricted | FileSystemSandboxKind::ExternalSandbox => {
                return FileSystemAccessMode::Write;
            }
            FileSystemSandboxKind::Restricted => {}
        }

        let path = resolve_path_against_cwd(path, cwd);
        self.resolved_entries_with_cwd(cwd)
            .into_iter()
            .filter(|entry| path.starts_with(&entry.path))
            .max_by(resolved_entry_precedence)
            .map(|entry| entry.access)
            .unwrap_or(FileSystemAccessMode::Deny)
    }

    #[cfg(target_os = "linux")]
    fn resolved_entries_with_cwd(&self, cwd: &Path) -> Vec<ResolvedEntry> {
        self.entries
            .iter()
            .filter_map(|entry| {
                resolve_entry_path(&entry.path, cwd).map(|path| ResolvedEntry {
                    path,
                    access: entry.access,
                })
            })
            .collect()
    }

    fn has_root_access(&self, predicate: impl Fn(FileSystemAccessMode) -> bool) -> bool {
        matches!(self.kind, FileSystemSandboxKind::Restricted)
            && self.entries.iter().any(|entry| {
                matches!(
                    &entry.path,
                    FileSystemPath::Special {
                        value: FileSystemSpecialPath::Root
                    } if predicate(entry.access)
                )
            })
    }

    #[cfg(target_os = "linux")]
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
                        value: FileSystemSpecialPath::Root
                    } if entry.access == FileSystemAccessMode::Read
                )
            })
    }

    #[cfg(target_os = "linux")]
    fn metadata_write_denied(&self, path: &Path, cwd: &Path) -> bool {
        let target = resolve_path_against_cwd(path, cwd);
        self.resolved_entries_with_cwd(cwd)
            .into_iter()
            .filter(|entry| !entry.access.can_write())
            .any(|entry| {
                target.starts_with(&entry.path)
                    && entry
                        .path
                        .file_name()
                        .and_then(|file_name| file_name.to_str())
                        .is_some_and(|file_name| PROTECTED_METADATA_NAMES.contains(&file_name))
            })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ManagedFileSystemPermissions {
    Restricted {
        #[serde(default)]
        entries: Vec<FileSystemSandboxEntry>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        glob_scan_max_depth: Option<usize>,
    },
    Unrestricted,
}

impl ManagedFileSystemPermissions {
    fn from_sandbox_policy(policy: &FileSystemSandboxPolicy) -> Self {
        match policy.kind {
            FileSystemSandboxKind::Restricted => Self::Restricted {
                entries: policy.entries.clone(),
                glob_scan_max_depth: policy.glob_scan_max_depth,
            },
            FileSystemSandboxKind::Unrestricted => Self::Unrestricted,
            FileSystemSandboxKind::ExternalSandbox => {
                unreachable!(
                    "external filesystem policy is represented by PermissionProfile::External"
                )
            }
        }
    }

    pub fn to_sandbox_policy(&self) -> FileSystemSandboxPolicy {
        match self {
            Self::Restricted {
                entries,
                glob_scan_max_depth,
            } => FileSystemSandboxPolicy {
                kind: FileSystemSandboxKind::Restricted,
                glob_scan_max_depth: *glob_scan_max_depth,
                entries: entries.clone(),
            },
            Self::Unrestricted => FileSystemSandboxPolicy::unrestricted(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PermissionProfile {
    Managed {
        file_system: ManagedFileSystemPermissions,
        network: NetworkSandboxPolicy,
    },
    Disabled,
    External {
        network: NetworkSandboxPolicy,
    },
}

impl PermissionProfile {
    pub fn workspace_write() -> Self {
        let file_system = FileSystemSandboxPolicy::workspace_write(&[], false, false);
        Self::Managed {
            file_system: ManagedFileSystemPermissions::from_sandbox_policy(&file_system),
            network: NetworkSandboxPolicy::Restricted,
        }
    }

    pub fn from_legacy_sandbox_policy(policy: &SandboxPolicy, cwd: &Path) -> Self {
        match policy {
            SandboxPolicy::DangerFullAccess => Self::Disabled,
            SandboxPolicy::ExternalSandbox { network_access } => Self::External {
                network: (*network_access).into(),
            },
            SandboxPolicy::ReadOnly { network_access } => Self::Managed {
                file_system: ManagedFileSystemPermissions::from_sandbox_policy(
                    &FileSystemSandboxPolicy::read_only(),
                ),
                network: NetworkSandboxPolicy::from_bool(*network_access),
            },
            SandboxPolicy::WorkspaceWrite {
                writable_roots,
                network_access,
                exclude_tmpdir_env_var,
                exclude_slash_tmp,
            } => {
                let _ = cwd;
                let file_system = FileSystemSandboxPolicy::workspace_write(
                    writable_roots,
                    *exclude_tmpdir_env_var,
                    *exclude_slash_tmp,
                );
                Self::Managed {
                    file_system: ManagedFileSystemPermissions::from_sandbox_policy(&file_system),
                    network: NetworkSandboxPolicy::from_bool(*network_access),
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    pub fn to_runtime_permissions(&self) -> (FileSystemSandboxPolicy, NetworkSandboxPolicy) {
        (
            self.file_system_sandbox_policy(),
            self.network_sandbox_policy(),
        )
    }

    pub fn file_system_sandbox_policy(&self) -> FileSystemSandboxPolicy {
        match self {
            Self::Managed { file_system, .. } => file_system.to_sandbox_policy(),
            Self::Disabled => FileSystemSandboxPolicy::unrestricted(),
            Self::External { .. } => FileSystemSandboxPolicy::external_sandbox(),
        }
    }

    pub fn network_sandbox_policy(&self) -> NetworkSandboxPolicy {
        match self {
            Self::Managed { network, .. } | Self::External { network } => *network,
            Self::Disabled => NetworkSandboxPolicy::Enabled,
        }
    }

    pub fn has_full_network_access(&self) -> bool {
        self.network_sandbox_policy().is_enabled()
    }

    #[cfg(target_os = "linux")]
    pub fn has_full_disk_write_access(&self) -> bool {
        self.file_system_sandbox_policy()
            .has_full_disk_write_access()
    }

    #[cfg(target_os = "linux")]
    pub fn requires_managed_sandbox(&self) -> bool {
        match self {
            Self::Disabled => false,
            Self::External { network } => !network.is_enabled(),
            Self::Managed { .. } => {
                !self.has_full_disk_write_access() || !self.has_full_network_access()
            }
        }
    }

    #[cfg(target_os = "linux")]
    pub fn with_additional_writable_root(self, root: PathBuf) -> Self {
        match self {
            Self::Managed {
                file_system,
                network,
            } => {
                let file_system = file_system
                    .to_sandbox_policy()
                    .with_additional_writable_root(root);
                Self::Managed {
                    file_system: ManagedFileSystemPermissions::from_sandbox_policy(&file_system),
                    network,
                }
            }
            Self::Disabled => Self::Disabled,
            Self::External { network } => Self::External { network },
        }
    }

    pub fn to_legacy_sandbox_policy(&self, cwd: &Path) -> SandboxPolicy {
        match self {
            Self::Disabled => SandboxPolicy::DangerFullAccess,
            Self::External { network } => SandboxPolicy::ExternalSandbox {
                network_access: (*network).into(),
            },
            Self::Managed {
                file_system,
                network,
            } => file_system
                .to_sandbox_policy()
                .to_legacy_sandbox_policy(*network, cwd),
        }
    }
}

impl NetworkSandboxPolicy {
    fn from_bool(value: bool) -> Self {
        if value {
            Self::Enabled
        } else {
            Self::Restricted
        }
    }
}

impl From<NetworkSandboxPolicy> for NetworkAccess {
    fn from(value: NetworkSandboxPolicy) -> Self {
        if value.is_enabled() {
            NetworkAccess::Enabled
        } else {
            NetworkAccess::Restricted
        }
    }
}

impl From<NetworkAccess> for NetworkSandboxPolicy {
    fn from(value: NetworkAccess) -> Self {
        if value.is_enabled() {
            NetworkSandboxPolicy::Enabled
        } else {
            NetworkSandboxPolicy::Restricted
        }
    }
}

#[cfg(target_os = "linux")]
fn resolve_entry_path(path: &FileSystemPath, cwd: &Path) -> Option<PathBuf> {
    match path {
        FileSystemPath::Special {
            value: FileSystemSpecialPath::Root,
        } => Some(PathBuf::from("/")),
        _ => resolve_file_system_path(path, cwd),
    }
}

fn resolve_file_system_path(path: &FileSystemPath, cwd: &Path) -> Option<PathBuf> {
    match path {
        FileSystemPath::Path { path } => Some(resolve_path_against_cwd(path, cwd)),
        FileSystemPath::GlobPattern { .. } => None,
        FileSystemPath::Special { value } => resolve_file_system_special_path(value, cwd),
    }
}

fn resolve_file_system_special_path(value: &FileSystemSpecialPath, cwd: &Path) -> Option<PathBuf> {
    match value {
        FileSystemSpecialPath::Root => Some(PathBuf::from("/")),
        FileSystemSpecialPath::Minimal => None,
        FileSystemSpecialPath::ProjectRoots { subpath: None } => Some(cwd.to_path_buf()),
        FileSystemSpecialPath::ProjectRoots {
            subpath: Some(subpath),
        } => Some(resolve_path_against_cwd(subpath, cwd)),
        FileSystemSpecialPath::Tmpdir => std::env::var_os("TMPDIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .filter(|path| path.is_absolute()),
        FileSystemSpecialPath::SlashTmp => Some(PathBuf::from("/tmp")),
        FileSystemSpecialPath::Unknown { .. } => None,
    }
}

fn resolve_path_against_cwd(path: &Path, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn default_read_only_subpaths_for_writable_root(root: &Path) -> Vec<PathBuf> {
    PROTECTED_METADATA_NAMES
        .iter()
        .map(|name| root.join(name))
        .collect()
}

#[cfg(target_os = "linux")]
fn resolved_entry_precedence(left: &ResolvedEntry, right: &ResolvedEntry) -> Ordering {
    left.path
        .components()
        .count()
        .cmp(&right.path.components().count())
        .then_with(|| left.access.cmp(&right.access))
}

fn dedup_paths(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.sort();
    paths.dedup();
    paths
}

pub fn normalize_permission_profile_paths(
    mut profile: PermissionProfile,
) -> Result<PermissionProfile, String> {
    match &mut profile {
        PermissionProfile::Managed { file_system, .. } => {
            if let ManagedFileSystemPermissions::Restricted { entries, .. } = file_system {
                for entry in entries {
                    normalize_file_system_path(&mut entry.path)?;
                }
            }
        }
        PermissionProfile::Disabled | PermissionProfile::External { .. } => {}
    }
    validate_permission_profile(&profile)?;
    Ok(profile)
}

pub fn validate_permission_profile(profile: &PermissionProfile) -> Result<(), String> {
    let file_system = profile.file_system_sandbox_policy();
    for entry in &file_system.entries {
        validate_file_system_path(&entry.path)?;
    }
    Ok(())
}

fn normalize_file_system_path(path: &mut FileSystemPath) -> Result<(), String> {
    let FileSystemPath::Path { path } = path else {
        return Ok(());
    };
    let raw = path.to_string_lossy();
    if !raw.starts_with("file:") {
        return Ok(());
    }
    let url = Url::parse(&raw).map_err(|err| {
        format!("Codex permissionProfile.file_system.entries.path has invalid file URI: {err}")
    })?;
    *path = url.to_file_path().map_err(|_| {
        format!("Codex permissionProfile.file_system.entries.path must be a local file URI: {raw}")
    })?;
    Ok(())
}

fn validate_file_system_path(path: &FileSystemPath) -> Result<(), String> {
    match path {
        FileSystemPath::Path { path } => {
            if !path.is_absolute() {
                return Err(format!(
                    "Codex permissionProfile.file_system.entries.path requires absolute paths, got: {}",
                    path.display()
                ));
            }
        }
        FileSystemPath::GlobPattern { .. } => {}
        FileSystemPath::Special {
            value:
                FileSystemSpecialPath::ProjectRoots {
                    subpath: Some(subpath),
                },
        } => {
            if subpath.is_absolute() {
                return Err(format!(
                    "Codex permissionProfile.file_system project_roots subpath must be relative, got: {}",
                    subpath.display()
                ));
            }
        }
        FileSystemPath::Special { .. } => {}
    }
    Ok(())
}
