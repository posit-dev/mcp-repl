mod backend;
mod debug_logs;
mod debug_repl;
mod diagnostics;
mod event_log;
mod html_to_markdown;
mod input_protocol;
mod install;
mod ipc;
mod output_capture;
mod output_stream;
mod output_timeline;
mod oversized_output;
mod pager;
mod pending_output_tape;
mod r_controls;
mod r_graphics;
mod r_htmd;
mod r_session;
mod sandbox;
mod sandbox_cli;
mod server;
#[cfg(target_os = "windows")]
mod windows_sandbox;
mod worker;
mod worker_process;
mod worker_protocol;

use std::path::PathBuf;

use crate::backend::{Backend, backend_from_env};
use crate::oversized_output::OversizedOutputMode;
use crate::sandbox_cli::{
    SandboxCliOperation, SandboxCliPlan, SandboxModeArg, parse_sandbox_config_override,
};

enum CliCommand {
    RunServer(CliOptions),
    Install(install::InstallOptions),
}

#[derive(Debug, Clone)]
struct CliOptions {
    sandbox_plan: SandboxCliPlan,
    debug_repl: bool,
    backend: Backend,
    debug_dir: Option<PathBuf>,
    oversized_output: OversizedOutputMode,
}

#[derive(Debug, Default)]
struct SandboxCliArgs {
    plan: SandboxCliPlan,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_family = "unix")]
    // The worker and server may still write output to stdout/stderr. If a downstream reader
    // disconnects and closes its read end, future writes can raise SIGPIPE and terminate the
    // process on Unix. Ignore SIGPIPE so we surface broken-pipe errors normally instead of
    // crashing.
    ignore_sigpipe();
    crate::diagnostics::startup_log("main: entry");
    #[cfg(target_os = "linux")]
    if sandbox::invoked_as_codex_linux_sandbox() {
        sandbox::run_linux_sandbox_main();
    }
    #[cfg(target_os = "windows")]
    if sandbox::invoked_as_codex_windows_sandbox() {
        sandbox::run_windows_sandbox_main();
    }

    if worker::is_worker_mode() {
        crate::diagnostics::startup_log("main: worker mode");
        return worker::run();
    }

    match parse_cli_args()? {
        CliCommand::RunServer(options) => {
            debug_logs::initialize(options.debug_dir.clone())?;
            event_log::initialize(
                options.debug_dir.clone(),
                event_log::StartupContext {
                    mode: if options.debug_repl {
                        "debug_repl".to_string()
                    } else {
                        "server".to_string()
                    },
                    backend: match options.backend {
                        Backend::R => "r".to_string(),
                        Backend::Python => "python".to_string(),
                    },
                    debug_repl: options.debug_repl,
                    sandbox_state: None,
                },
            )?;
            if options.debug_repl {
                crate::diagnostics::startup_log("main: debug repl mode");
                return debug_repl::run(
                    options.backend,
                    options.sandbox_plan,
                    options.oversized_output,
                );
            }
            crate::diagnostics::startup_log("main: server mode");
            server::run(
                options.backend,
                options.sandbox_plan,
                options.oversized_output,
            )
            .await
        }
        CliCommand::Install(options) => install::run(options),
    }
}

#[cfg(target_family = "unix")]
fn ignore_sigpipe() {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

fn parse_cli_args() -> Result<CliCommand, Box<dyn std::error::Error>> {
    parse_cli_args_from(
        std::env::args_os()
            .skip(1)
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect(),
    )
}

fn parse_cli_args_from(args: Vec<String>) -> Result<CliCommand, Box<dyn std::error::Error>> {
    let mut parser = ArgParser { args, index: 0 };
    if parser.peek() == Some("install") {
        parser.next();
        return Ok(CliCommand::Install(parse_install_args(&mut parser)?));
    }

    let mut sandbox_args = SandboxCliArgs::default();
    let mut debug_repl = false;
    let mut debug_dir = None;
    let mut backend = backend_from_env()?;
    let mut oversized_output = OversizedOutputMode::Pager;
    let mut oversized_output_seen = false;
    while let Some(arg) = parser.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            "--sandbox" => {
                let value = parser.next_value("--sandbox")?;
                let mode = SandboxModeArg::parse(&value).map_err(|err| err.to_string())?;
                sandbox_args
                    .plan
                    .operations
                    .push(SandboxCliOperation::SetMode(mode));
            }
            _ if arg.starts_with("--sandbox=") => {
                let value = arg.split_once('=').map(|(_, value)| value).unwrap_or("");
                if value.is_empty() {
                    return Err("missing value for --sandbox".into());
                }
                let mode = SandboxModeArg::parse(value).map_err(|err| err.to_string())?;
                sandbox_args
                    .plan
                    .operations
                    .push(SandboxCliOperation::SetMode(mode));
            }
            "--add-writable-root" | "--add-writeable-root" => {
                let value = parser.next_value("--add-writable-root")?;
                sandbox_args
                    .plan
                    .operations
                    .push(SandboxCliOperation::AddWritableRoot(parse_writable_root(
                        &value,
                    )?));
            }
            _ if arg.starts_with("--add-writable-root=")
                || arg.starts_with("--add-writeable-root=") =>
            {
                let value = arg.split_once('=').map(|(_, value)| value).unwrap_or("");
                if value.is_empty() {
                    return Err("missing value for --add-writable-root".into());
                }
                sandbox_args
                    .plan
                    .operations
                    .push(SandboxCliOperation::AddWritableRoot(parse_writable_root(
                        value,
                    )?));
            }
            "--add-allowed-domain" => {
                let value = parser.next_value("--add-allowed-domain")?;
                let value = value.trim();
                if value.is_empty() {
                    return Err("missing value for --add-allowed-domain".into());
                }
                sandbox_args
                    .plan
                    .operations
                    .push(SandboxCliOperation::AddAllowedDomain(value.to_string()));
            }
            _ if arg.starts_with("--add-allowed-domain=") => {
                let value = arg.split_once('=').map(|(_, value)| value).unwrap_or("");
                if value.trim().is_empty() {
                    return Err("missing value for --add-allowed-domain".into());
                }
                sandbox_args
                    .plan
                    .operations
                    .push(SandboxCliOperation::AddAllowedDomain(
                        value.trim().to_string(),
                    ));
            }
            "--config" => {
                let value = parser.next_value("--config")?;
                let parsed =
                    parse_sandbox_config_override(&value).map_err(|err| err.to_string())?;
                sandbox_args
                    .plan
                    .operations
                    .push(SandboxCliOperation::Config(parsed));
            }
            _ if arg.starts_with("--config=") => {
                let value = arg.split_once('=').map(|(_, value)| value).unwrap_or("");
                if value.trim().is_empty() {
                    return Err("missing value for --config".into());
                }
                let parsed = parse_sandbox_config_override(value).map_err(|err| err.to_string())?;
                sandbox_args
                    .plan
                    .operations
                    .push(SandboxCliOperation::Config(parsed));
            }
            "--debug-repl" => {
                debug_repl = true;
            }
            "--debug-dir" => {
                let value = parser.next_value("--debug-dir")?;
                if value.trim().is_empty() {
                    return Err("missing value for --debug-dir".into());
                }
                debug_dir = Some(PathBuf::from(value));
            }
            _ if arg.starts_with("--debug-dir=") => {
                let value = arg.split_once('=').map(|(_, value)| value).unwrap_or("");
                if value.trim().is_empty() {
                    return Err("missing value for --debug-dir".into());
                }
                debug_dir = Some(PathBuf::from(value));
            }
            "--oversized-output" => {
                if oversized_output_seen {
                    return Err("duplicate --oversized-output".into());
                }
                let value = parser.next_value("--oversized-output")?;
                oversized_output =
                    OversizedOutputMode::parse(&value).map_err(|err| err.to_string())?;
                oversized_output_seen = true;
            }
            _ if arg.starts_with("--oversized-output=") => {
                if oversized_output_seen {
                    return Err("duplicate --oversized-output".into());
                }
                let value = arg
                    .split_once('=')
                    .map(|(_, value)| value)
                    .unwrap_or_default();
                if value.is_empty() {
                    return Err("missing value for --oversized-output".into());
                }
                oversized_output =
                    OversizedOutputMode::parse(value).map_err(|err| err.to_string())?;
                oversized_output_seen = true;
            }
            _ => match parse_backend_arg(&arg, &mut parser)? {
                Some(parsed_backend) => backend = Some(parsed_backend),
                None => return Err(format!("unknown argument: {arg}").into()),
            },
        }
    }

    Ok(CliCommand::RunServer(CliOptions {
        sandbox_plan: sandbox_args.plan,
        debug_repl,
        backend: backend.unwrap_or(Backend::R),
        debug_dir,
        oversized_output,
    }))
}

fn parse_backend_arg(
    arg: &str,
    parser: &mut ArgParser,
) -> Result<Option<Backend>, Box<dyn std::error::Error>> {
    if arg == "--interpreter" {
        let value = parser.next_value("--interpreter")?;
        return Ok(Some(Backend::parse(&value).map_err(|err| err.to_string())?));
    }
    if let Some(value) = arg.strip_prefix("--interpreter=") {
        if value.is_empty() {
            return Err("missing value for --interpreter".into());
        }
        return Ok(Some(Backend::parse(value).map_err(|err| err.to_string())?));
    }
    Ok(None)
}

struct ArgParser {
    args: Vec<String>,
    index: usize,
}

impl ArgParser {
    fn next(&mut self) -> Option<String> {
        let value = self.args.get(self.index)?.clone();
        self.index += 1;
        Some(value)
    }

    fn peek(&self) -> Option<&str> {
        self.args.get(self.index).map(String::as_str)
    }

    fn next_value(&mut self, flag: &str) -> Result<String, Box<dyn std::error::Error>> {
        self.next()
            .ok_or_else(|| format!("missing value for {flag}").into())
    }
}

fn parse_install_args(
    parser: &mut ArgParser,
) -> Result<install::InstallOptions, Box<dyn std::error::Error>> {
    let mut targets = Vec::new();
    let mut args = Vec::new();
    let mut interpreters: Vec<install::InstallInterpreter> = Vec::new();

    while let Some(arg) = parser.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_install_usage();
                std::process::exit(0);
            }
            "--interpreter" => {
                let value = parser.next_value("--interpreter")?;
                parse_install_interpreters_value(&value, &mut interpreters)?;
            }
            _ if arg.starts_with("--interpreter=") => {
                let value = arg.split_once('=').map(|(_, value)| value).unwrap_or("");
                if value.is_empty() {
                    return Err("missing value for --interpreter".into());
                }
                parse_install_interpreters_value(value, &mut interpreters)?;
            }
            "--client" => {
                let value = parser.next_value("--client")?;
                parse_install_targets_value(&value, &mut targets)?;
            }
            _ if arg.starts_with("--client=") => {
                let value = arg.split_once('=').map(|(_, value)| value).unwrap_or("");
                if value.is_empty() {
                    return Err("missing value for --client".into());
                }
                parse_install_targets_value(value, &mut targets)?;
            }
            "--arg" => {
                args.push(parser.next_value("--arg")?);
            }
            _ if arg.starts_with("--arg=") => {
                let value = arg.split_once('=').map(|(_, value)| value).unwrap_or("");
                if value.is_empty() {
                    return Err("missing value for --arg".into());
                }
                args.push(value.to_string());
            }
            _ => {
                if let Some(flag) = arg.strip_prefix('-') {
                    return Err(format!("unknown install option: -{flag}").into());
                }
                return Err(
                    format!("unknown install argument: {arg} (use --client codex|claude)").into(),
                );
            }
        }
    }

    Ok(install::InstallOptions {
        targets,
        interpreters,
        args,
    })
}

fn parse_install_interpreters_value(
    raw: &str,
    interpreters: &mut Vec<install::InstallInterpreter>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut parsed_any = false;
    for part in raw.split(',') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            return Err("empty --interpreter value (expected r|python)".into());
        }
        parsed_any = true;
        interpreters.push(
            install::InstallInterpreter::parse(trimmed)
                .map_err(|err| -> Box<dyn std::error::Error> { err.into() })?,
        );
    }
    if !parsed_any {
        return Err(format!(
            "invalid --interpreter value: {raw} (expected comma-separated list containing r and/or python)"
        )
        .into());
    }
    Ok(())
}

fn parse_install_targets_value(
    raw: &str,
    targets: &mut Vec<install::InstallTarget>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut parsed_any = false;
    for part in raw.split(',') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            return Err("empty --client value (expected codex|claude)".into());
        }
        parsed_any = true;
        targets.push(
            install::InstallTarget::parse(trimmed)
                .map_err(|err| -> Box<dyn std::error::Error> { err.into() })?,
        );
    }
    if !parsed_any {
        return Err(format!(
            "invalid --client value: {raw} (expected comma-separated list containing codex and/or claude)"
        )
        .into());
    }
    Ok(())
}

fn parse_writable_root(raw: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let path = PathBuf::from(raw);
    if !path.is_absolute() {
        return Err(format!("invalid writable root value: {raw} (expected absolute path)").into());
    }
    Ok(path)
}

fn print_usage() {
    println!(
        "Usage:\n\
mcp-repl [--debug-repl] [--interpreter <r|python>] [--oversized-output <files|pager>] [--sandbox <inherit|read-only|workspace-write|danger-full-access>] [--add-writable-root <abs-path>] [--add-allowed-domain <domain>] [--config <key=value>]...\n\
mcp-repl install [--client <codex|claude>]... [--interpreter <r|python>[,r|python]...]... [--arg <value>]...\n\n\
--debug-repl: run an interactive debug REPL over stdio\n\
--debug-dir: optional base directory for per-startup debug artifacts (env: MCP_REPL_DEBUG_DIR)\n\
--interpreter: choose REPL interpreter (default: r; env MCP_REPL_INTERPRETER)\n\
--oversized-output: choose oversized-output handling (pager: default legacy modal pager; files: spill oversized replies to files)\n\
--sandbox: base sandbox mode (inherit requires client sandbox update)\n\
--add-writable-root / --add-writeable-root: append absolute writable root in argument order\n\
--add-allowed-domain: append allowed domain pattern in argument order\n\
--config: apply advanced ordered sandbox/network override (Codex-compatible keys)\n\
install: update MCP config for codex (~/.codex/config.toml) and claude (~/.claude.json)\n\
install defaults to the full interpreter grid for each selected client (currently r + python)"
    );
}

fn print_install_usage() {
    println!(
        "Usage:\n\
mcp-repl install [--client <codex|claude>]... [--interpreter <r|python>[,r|python]...]... [--arg <value>]...\n\n\
If no target is specified for `install`, available targets are used:\n\
- codex: $CODEX_HOME or ~/.codex (must exist)\n\
- claude: ~/.claude.json (created if needed)\n\
Missing ~/.codex directory is not created.\n\
If no --interpreter is specified, install uses the full interpreter grid for each selected client."
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::{SandboxPolicy, SandboxState};
    use crate::sandbox_cli::{SandboxConfigOperation, resolve_effective_sandbox_state};

    #[test]
    fn parse_backend_arg_accepts_interpreter_flag_forms() {
        let mut parser = ArgParser {
            args: vec!["python".to_string()],
            index: 0,
        };
        let parsed = parse_backend_arg("--interpreter", &mut parser).expect("parse flag");
        assert_eq!(parsed, Some(Backend::Python));

        let mut parser = ArgParser {
            args: Vec::new(),
            index: 0,
        };
        let parsed = parse_backend_arg("--interpreter=python", &mut parser).expect("parse flag");
        assert_eq!(parsed, Some(Backend::Python));
    }

    #[test]
    fn parse_cli_args_defaults_oversized_output_to_pager() {
        let command = parse_cli_args_from(Vec::new()).expect("parse cli args");
        let CliCommand::RunServer(options) = command else {
            panic!("expected server command");
        };
        assert_eq!(options.oversized_output, OversizedOutputMode::Pager);
    }

    #[test]
    fn parse_cli_args_accepts_explicit_oversized_output_files() {
        let command =
            parse_cli_args_from(vec!["--oversized-output".to_string(), "files".to_string()])
                .expect("parse cli args");
        let CliCommand::RunServer(options) = command else {
            panic!("expected server command");
        };
        assert_eq!(options.oversized_output, OversizedOutputMode::Files);
    }

    #[test]
    fn parse_install_args_defaults_to_no_interpreters() {
        let mut parser = ArgParser {
            args: Vec::new(),
            index: 0,
        };
        let parsed = parse_install_args(&mut parser).expect("parse install args");
        assert!(parsed.interpreters.is_empty());
    }

    #[test]
    fn parse_install_args_accepts_repeatable_interpreters() {
        let mut parser = ArgParser {
            args: vec![
                "--interpreter".to_string(),
                "r".to_string(),
                "--interpreter".to_string(),
                "python".to_string(),
            ],
            index: 0,
        };
        let parsed = parse_install_args(&mut parser).expect("parse install args");
        assert_eq!(
            parsed.interpreters,
            vec![
                install::InstallInterpreter::R,
                install::InstallInterpreter::Python
            ]
        );
    }

    #[test]
    fn parse_install_args_accepts_comma_separated_interpreters() {
        let mut parser = ArgParser {
            args: vec!["--interpreter=python,r".to_string()],
            index: 0,
        };
        let parsed = parse_install_args(&mut parser).expect("parse install args");
        assert_eq!(
            parsed.interpreters,
            vec![
                install::InstallInterpreter::Python,
                install::InstallInterpreter::R
            ]
        );
    }

    #[test]
    fn parse_install_args_accepts_client_flag() {
        let mut parser = ArgParser {
            args: vec!["--client".to_string(), "codex,claude".to_string()],
            index: 0,
        };
        let parsed = parse_install_args(&mut parser).expect("parse install args");
        assert_eq!(
            parsed.targets,
            vec![
                install::InstallTarget::Codex,
                install::InstallTarget::Claude
            ]
        );
    }

    #[test]
    fn parse_install_args_rejects_command_flag() {
        let mut parser = ArgParser {
            args: vec!["--command".to_string(), "/path/to/mcp-repl".to_string()],
            index: 0,
        };
        let err = parse_install_args(&mut parser).expect_err("reject --command");
        assert!(
            err.to_string()
                .contains("unknown install option: --command"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn parse_install_args_rejects_positional_client_target() {
        let mut parser = ArgParser {
            args: vec!["codex".to_string()],
            index: 0,
        };
        let err = parse_install_args(&mut parser).expect_err("reject positional target");
        assert!(
            err.to_string().contains("unknown install argument: codex"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn parse_install_interpreters_value_rejects_empty_values() {
        let mut interpreters = Vec::new();
        let err = parse_install_interpreters_value("", &mut interpreters).expect_err("empty value");
        assert!(
            err.to_string().contains("empty --interpreter value"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn parse_install_targets_value_rejects_empty_values() {
        let mut targets = Vec::new();
        let err = parse_install_targets_value(",", &mut targets).expect_err("empty value");
        assert!(
            err.to_string().contains("empty --client value"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn parse_sandbox_mode_accepts_inherit() {
        let mode = SandboxModeArg::parse("inherit").expect("sandbox mode");
        assert!(matches!(mode, SandboxModeArg::Inherit));
    }

    #[test]
    fn parse_config_override_supports_codex_bwrap_alias() {
        let op =
            parse_sandbox_config_override("use_linux_sandbox_bwrap=true").expect("config override");
        assert!(matches!(
            op,
            SandboxConfigOperation::SetUseLinuxSandboxBwrap(true)
        ));
    }

    #[test]
    fn parse_config_override_supports_allowed_domains() {
        let op = parse_sandbox_config_override(
            "permissions.network.allowed_domains=[\"pypi.org\",\"files.pythonhosted.org\"]",
        )
        .expect("config override");
        assert!(matches!(
            op,
            SandboxConfigOperation::SetAllowedDomains(values)
                if values == vec!["pypi.org".to_string(), "files.pythonhosted.org".to_string()]
        ));
    }

    #[test]
    fn ordered_layering_last_argument_wins() {
        let plan = SandboxCliPlan {
            operations: vec![
                SandboxCliOperation::SetMode(SandboxModeArg::WorkspaceWrite),
                SandboxCliOperation::Config(
                    parse_sandbox_config_override("sandbox_workspace_write.network_access=false")
                        .expect("config override"),
                ),
                SandboxCliOperation::Config(
                    parse_sandbox_config_override("sandbox_workspace_write.network_access=true")
                        .expect("config override"),
                ),
            ],
        };

        let inherited = SandboxState::default();
        let resolved = resolve_effective_sandbox_state(&plan, Some(&inherited))
            .expect("effective sandbox state");
        match resolved.sandbox_policy {
            SandboxPolicy::WorkspaceWrite { network_access, .. } => assert!(network_access),
            other => panic!("expected workspace-write policy, got {other:?}"),
        }
    }

    #[test]
    fn empty_plan_uses_inherited_state_when_available() {
        let plan = SandboxCliPlan::default();
        let inherited = SandboxState {
            sandbox_policy: SandboxPolicy::DangerFullAccess,
            ..SandboxState::default()
        };
        let resolved = resolve_effective_sandbox_state(&plan, Some(&inherited))
            .expect("effective sandbox state");
        assert_eq!(resolved.sandbox_policy, SandboxPolicy::DangerFullAccess);
    }

    #[test]
    fn inherit_without_client_update_errors() {
        let plan = SandboxCliPlan {
            operations: vec![SandboxCliOperation::SetMode(SandboxModeArg::Inherit)],
        };
        let err = resolve_effective_sandbox_state(&plan, None).expect_err("missing inherit update");
        assert!(
            err.contains("--sandbox inherit requested but no client sandbox state was provided"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn inherit_mode_copies_managed_network_policy() {
        let plan = SandboxCliPlan {
            operations: vec![SandboxCliOperation::SetMode(SandboxModeArg::Inherit)],
        };
        let mut inherited = SandboxState::default();
        inherited.managed_network_policy.allowed_domains =
            vec!["example.com".to_string(), "*.example.org".to_string()];
        inherited.managed_network_policy.denied_domains = vec!["blocked.example".to_string()];
        inherited.managed_network_policy.allow_local_binding = true;

        let resolved = resolve_effective_sandbox_state(&plan, Some(&inherited))
            .expect("effective sandbox state");

        assert_eq!(
            resolved.managed_network_policy, inherited.managed_network_policy,
            "inherit mode should copy managed network policy from client state"
        );
    }
}
