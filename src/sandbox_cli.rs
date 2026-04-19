use std::path::PathBuf;

use crate::sandbox::{SandboxPolicy, SandboxState};

pub const MISSING_INHERITED_SANDBOX_STATE_MESSAGE: &str =
    "--sandbox inherit requested but no client sandbox state was provided";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxModeArg {
    Inherit,
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

impl SandboxModeArg {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim() {
            "inherit" => Ok(Self::Inherit),
            "read-only" => Ok(Self::ReadOnly),
            "workspace-write" => Ok(Self::WorkspaceWrite),
            "danger-full-access" => Ok(Self::DangerFullAccess),
            _ => Err(format!(
                "invalid sandbox mode: {value} (expected inherit|read-only|workspace-write|danger-full-access)"
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
pub enum SandboxConfigOperation {
    SetMode(SandboxModeArg),
    SetWorkspaceNetworkAccess(bool),
    SetWorkspaceWritableRoots(Vec<PathBuf>),
    SetWorkspaceExcludeTmpdirEnvVar(bool),
    SetWorkspaceExcludeSlashTmp(bool),
    SetAllowedDomains(Vec<String>),
    SetDeniedDomains(Vec<String>),
    SetAllowLocalBinding(bool),
    SetUseLinuxSandboxBwrap(bool),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxCliOperation {
    SetMode(SandboxModeArg),
    AddWritableRoot(PathBuf),
    AddAllowedDomain(String),
    Config(SandboxConfigOperation),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxCliPlan {
    pub operations: Vec<SandboxCliOperation>,
}

fn last_mode_operation(plan: &SandboxCliPlan) -> Option<(usize, SandboxModeArg)> {
    plan.operations
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, op)| match op {
            SandboxCliOperation::SetMode(mode)
            | SandboxCliOperation::Config(SandboxConfigOperation::SetMode(mode)) => {
                Some((index, *mode))
            }
            _ => None,
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SandboxValidationMode {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
    UnresolvedInherit,
}

impl SandboxValidationMode {
    fn from_policy(policy: &SandboxPolicy) -> Self {
        match policy {
            SandboxPolicy::ReadOnly => Self::ReadOnly,
            SandboxPolicy::WorkspaceWrite { .. } => Self::WorkspaceWrite,
            SandboxPolicy::DangerFullAccess | SandboxPolicy::ExternalSandbox { .. } => {
                Self::DangerFullAccess
            }
        }
    }
}

pub fn sandbox_plan_requests_inherited_state(plan: &SandboxCliPlan) -> bool {
    last_mode_operation(plan)
        .map(|(_, mode)| mode)
        .is_some_and(|mode| matches!(mode, SandboxModeArg::Inherit))
}

pub fn parse_sandbox_config_override(raw: &str) -> Result<SandboxConfigOperation, String> {
    let (raw_key, raw_value) = raw
        .split_once('=')
        .ok_or_else(|| format!("invalid --config override (missing '='): {raw}"))?;
    let key = canonicalize_config_key(raw_key.trim());
    let value = raw_value.trim();
    if key.is_empty() {
        return Err(format!("invalid --config override (empty key): {raw}"));
    }
    match key.as_str() {
        "sandbox_mode" => Ok(SandboxConfigOperation::SetMode(SandboxModeArg::parse(
            &parse_string_value(value),
        )?)),
        "sandbox_workspace_write.network_access" => Ok(
            SandboxConfigOperation::SetWorkspaceNetworkAccess(parse_bool_value(value)?),
        ),
        "sandbox_workspace_write.writable_roots" => {
            let roots = parse_string_array_value(value)?
                .into_iter()
                .map(|entry| {
                    let path = PathBuf::from(entry);
                    if path.is_absolute() {
                        Ok(path)
                    } else {
                        Err(format!(
                            "sandbox_workspace_write.writable_roots requires absolute paths: {}",
                            path.display()
                        ))
                    }
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(SandboxConfigOperation::SetWorkspaceWritableRoots(roots))
        }
        "sandbox_workspace_write.exclude_tmpdir_env_var" => Ok(
            SandboxConfigOperation::SetWorkspaceExcludeTmpdirEnvVar(parse_bool_value(value)?),
        ),
        "sandbox_workspace_write.exclude_slash_tmp" => Ok(
            SandboxConfigOperation::SetWorkspaceExcludeSlashTmp(parse_bool_value(value)?),
        ),
        "permissions.network.allowed_domains" => Ok(SandboxConfigOperation::SetAllowedDomains(
            parse_string_array_value(value)?,
        )),
        "permissions.network.denied_domains" => Ok(SandboxConfigOperation::SetDeniedDomains(
            parse_string_array_value(value)?,
        )),
        "permissions.network.allow_local_binding" => Ok(
            SandboxConfigOperation::SetAllowLocalBinding(parse_bool_value(value)?),
        ),
        "features.use_linux_sandbox_bwrap" => Ok(SandboxConfigOperation::SetUseLinuxSandboxBwrap(
            parse_bool_value(value)?,
        )),
        _ => Err(format!("unsupported --config key: {key}")),
    }
}

fn canonicalize_config_key(key: &str) -> String {
    if key == "use_linux_sandbox_bwrap" {
        "features.use_linux_sandbox_bwrap".to_string()
    } else {
        key.to_string()
    }
}

fn parse_bool_value(raw: &str) -> Result<bool, String> {
    match raw.trim() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(format!("expected boolean value (true|false), got: {raw}")),
    }
}

fn parse_string_value(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.len() >= 2
        && ((trimmed.starts_with('"') && trimmed.ends_with('"'))
            || (trimmed.starts_with('\'') && trimmed.ends_with('\'')))
    {
        trimmed[1..trimmed.len() - 1].to_string()
    } else {
        trimmed.to_string()
    }
}

fn parse_string_array_value(raw: &str) -> Result<Vec<String>, String> {
    let parsed = serde_json::from_str::<Vec<String>>(raw.trim())
        .map_err(|err| format!("expected JSON string array value, got {raw}: {err}"))?;
    Ok(parsed
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect())
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn resolve_effective_sandbox_state(
    plan: &SandboxCliPlan,
    inherited: Option<&SandboxState>,
) -> Result<SandboxState, String> {
    let defaults = SandboxState::default();
    resolve_effective_sandbox_state_with_defaults(plan, inherited, &defaults)
}

pub fn resolve_effective_sandbox_state_with_defaults(
    plan: &SandboxCliPlan,
    inherited: Option<&SandboxState>,
    defaults: &SandboxState,
) -> Result<SandboxState, String> {
    if plan.operations.is_empty() {
        return Ok(inherited.cloned().unwrap_or_else(|| defaults.clone()));
    }

    validate_sandbox_plan_operations(plan, inherited, defaults)?;

    let start_index = match last_mode_operation(plan) {
        Some((index, mode)) if !matches!(mode, SandboxModeArg::Inherit) => index,
        _ => 0,
    };
    let mut state = defaults.clone();
    for op in &plan.operations[start_index..] {
        match op {
            SandboxCliOperation::SetMode(mode) => {
                apply_mode(&mut state, *mode, inherited, defaults)?
            }
            SandboxCliOperation::AddWritableRoot(path) => {
                let SandboxPolicy::WorkspaceWrite { writable_roots, .. } =
                    &mut state.sandbox_policy
                else {
                    return Err(
                        "--add-writable-root can only be used while sandbox mode is workspace-write"
                            .to_string(),
                    );
                };
                if !writable_roots.iter().any(|root| root == path) {
                    writable_roots.push(path.clone());
                }
            }
            SandboxCliOperation::AddAllowedDomain(domain) => {
                let domain = domain.trim();
                if domain.is_empty() {
                    return Err("--add-allowed-domain requires a non-empty value".to_string());
                }
                if !state
                    .managed_network_policy
                    .allowed_domains
                    .iter()
                    .any(|entry| entry == domain)
                {
                    state
                        .managed_network_policy
                        .allowed_domains
                        .push(domain.to_string());
                }
            }
            SandboxCliOperation::Config(config_op) => {
                apply_config_op(&mut state, config_op, inherited, defaults)?
            }
        }
    }
    Ok(state)
}

fn validate_sandbox_plan_operations(
    plan: &SandboxCliPlan,
    inherited: Option<&SandboxState>,
    defaults: &SandboxState,
) -> Result<(), String> {
    let mut mode = SandboxValidationMode::from_policy(&defaults.sandbox_policy);
    for op in &plan.operations {
        match op {
            SandboxCliOperation::SetMode(next_mode) => {
                mode = validation_mode_for_mode_arg(*next_mode, inherited);
            }
            SandboxCliOperation::AddWritableRoot(_) => {
                require_workspace_write_mode(
                    mode,
                    "--add-writable-root can only be used while sandbox mode is workspace-write",
                )?;
            }
            SandboxCliOperation::AddAllowedDomain(domain) => {
                if domain.trim().is_empty() {
                    return Err("--add-allowed-domain requires a non-empty value".to_string());
                }
            }
            SandboxCliOperation::Config(config_op) => match config_op {
                SandboxConfigOperation::SetMode(next_mode) => {
                    mode = validation_mode_for_mode_arg(*next_mode, inherited);
                }
                SandboxConfigOperation::SetWorkspaceNetworkAccess(_) => {
                    require_workspace_write_mode(
                        mode,
                        "sandbox_workspace_write.network_access requires workspace-write mode",
                    )?
                }
                SandboxConfigOperation::SetWorkspaceWritableRoots(_) => {
                    require_workspace_write_mode(
                        mode,
                        "sandbox_workspace_write.writable_roots requires workspace-write mode",
                    )?
                }
                SandboxConfigOperation::SetWorkspaceExcludeTmpdirEnvVar(_) => {
                    require_workspace_write_mode(
                        mode,
                        "sandbox_workspace_write.exclude_tmpdir_env_var requires workspace-write mode",
                    )?
                }
                SandboxConfigOperation::SetWorkspaceExcludeSlashTmp(_) => {
                    require_workspace_write_mode(
                        mode,
                        "sandbox_workspace_write.exclude_slash_tmp requires workspace-write mode",
                    )?
                }
                SandboxConfigOperation::SetAllowedDomains(_)
                | SandboxConfigOperation::SetDeniedDomains(_)
                | SandboxConfigOperation::SetAllowLocalBinding(_)
                | SandboxConfigOperation::SetUseLinuxSandboxBwrap(_) => {}
            },
        }
    }
    Ok(())
}

fn validation_mode_for_mode_arg(
    mode: SandboxModeArg,
    inherited: Option<&SandboxState>,
) -> SandboxValidationMode {
    match mode {
        SandboxModeArg::Inherit => inherited
            .map(|state| SandboxValidationMode::from_policy(&state.sandbox_policy))
            .unwrap_or(SandboxValidationMode::UnresolvedInherit),
        SandboxModeArg::ReadOnly => SandboxValidationMode::ReadOnly,
        SandboxModeArg::WorkspaceWrite => SandboxValidationMode::WorkspaceWrite,
        SandboxModeArg::DangerFullAccess => SandboxValidationMode::DangerFullAccess,
    }
}

fn require_workspace_write_mode(mode: SandboxValidationMode, message: &str) -> Result<(), String> {
    if matches!(mode, SandboxValidationMode::WorkspaceWrite) {
        Ok(())
    } else {
        Err(message.to_string())
    }
}

fn apply_mode(
    state: &mut SandboxState,
    mode: SandboxModeArg,
    inherited: Option<&SandboxState>,
    defaults: &SandboxState,
) -> Result<(), String> {
    match mode {
        SandboxModeArg::Inherit => {
            let inherited =
                inherited.ok_or_else(|| MISSING_INHERITED_SANDBOX_STATE_MESSAGE.to_string())?;
            *state = inherited.clone();
        }
        SandboxModeArg::ReadOnly => {
            *state = defaults.clone();
            state.sandbox_policy = SandboxPolicy::ReadOnly;
        }
        SandboxModeArg::WorkspaceWrite => {
            *state = defaults.clone();
            state.sandbox_policy = SandboxPolicy::WorkspaceWrite {
                writable_roots: Vec::new(),
                network_access: false,
                exclude_tmpdir_env_var: false,
                exclude_slash_tmp: false,
            };
        }
        SandboxModeArg::DangerFullAccess => {
            *state = defaults.clone();
            state.sandbox_policy = SandboxPolicy::DangerFullAccess;
        }
    }
    Ok(())
}

fn apply_config_op(
    state: &mut SandboxState,
    op: &SandboxConfigOperation,
    inherited: Option<&SandboxState>,
    defaults: &SandboxState,
) -> Result<(), String> {
    match op {
        SandboxConfigOperation::SetMode(mode) => apply_mode(state, *mode, inherited, defaults),
        SandboxConfigOperation::SetWorkspaceNetworkAccess(network_access) => {
            let SandboxPolicy::WorkspaceWrite {
                network_access: current,
                ..
            } = &mut state.sandbox_policy
            else {
                return Err(
                    "sandbox_workspace_write.network_access requires workspace-write mode"
                        .to_string(),
                );
            };
            *current = *network_access;
            Ok(())
        }
        SandboxConfigOperation::SetWorkspaceWritableRoots(roots) => {
            let SandboxPolicy::WorkspaceWrite {
                writable_roots: current,
                ..
            } = &mut state.sandbox_policy
            else {
                return Err(
                    "sandbox_workspace_write.writable_roots requires workspace-write mode"
                        .to_string(),
                );
            };
            *current = roots.clone();
            Ok(())
        }
        SandboxConfigOperation::SetWorkspaceExcludeTmpdirEnvVar(value) => {
            let SandboxPolicy::WorkspaceWrite {
                exclude_tmpdir_env_var,
                ..
            } = &mut state.sandbox_policy
            else {
                return Err(
                    "sandbox_workspace_write.exclude_tmpdir_env_var requires workspace-write mode"
                        .to_string(),
                );
            };
            *exclude_tmpdir_env_var = *value;
            Ok(())
        }
        SandboxConfigOperation::SetWorkspaceExcludeSlashTmp(value) => {
            let SandboxPolicy::WorkspaceWrite {
                exclude_slash_tmp, ..
            } = &mut state.sandbox_policy
            else {
                return Err(
                    "sandbox_workspace_write.exclude_slash_tmp requires workspace-write mode"
                        .to_string(),
                );
            };
            *exclude_slash_tmp = *value;
            Ok(())
        }
        SandboxConfigOperation::SetAllowedDomains(values) => {
            state.managed_network_policy.allowed_domains = values.clone();
            Ok(())
        }
        SandboxConfigOperation::SetDeniedDomains(values) => {
            state.managed_network_policy.denied_domains = values.clone();
            Ok(())
        }
        SandboxConfigOperation::SetAllowLocalBinding(value) => {
            state.managed_network_policy.allow_local_binding = *value;
            Ok(())
        }
        SandboxConfigOperation::SetUseLinuxSandboxBwrap(value) => {
            state.use_linux_sandbox_bwrap = *value;
            Ok(())
        }
    }
}
