mod common;

use std::path::PathBuf;
use std::process::{Command, Output};

use common::TestResult;
use serde_json::Value as JsonValue;
use toml_edit::DocumentMut;

fn resolve_exe() -> TestResult<PathBuf> {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_mcp-repl") {
        return Ok(PathBuf::from(path));
    }

    let mut path = std::env::current_exe()?;
    path.pop();
    path.pop();
    {
        let candidate = "mcp-repl";
        let mut candidate_path = path.clone();
        candidate_path.push(candidate);
        if cfg!(windows) {
            candidate_path.set_extension("exe");
        }
        if candidate_path.exists() {
            return Ok(candidate_path);
        }
    }
    Err("unable to locate mcp-repl test binary".into())
}

fn assert_command_success(command: &mut Command) -> TestResult<Output> {
    let output = command.output()?;
    assert!(
        output.status.success(),
        "expected command to succeed, got status {} with stdout {:?} and stderr {:?}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "expected command stderr to be empty, got {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output)
}

fn assert_command_failure(command: &mut Command, expected_stderr: &str) -> TestResult<()> {
    let output = command.output()?;
    assert!(
        !output.status.success(),
        "expected command to fail, got status {} with stdout {:?} and stderr {:?}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "expected command stdout to be empty, got {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(
        String::from_utf8(output.stderr)?.trim_end(),
        expected_stderr
    );
    Ok(())
}

#[test]
fn cargo_install_default_binary_surface_is_mcp_repl_only() -> TestResult<()> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let output = Command::new(cargo)
        .arg("metadata")
        .arg("--no-deps")
        .arg("--format-version")
        .arg("1")
        .arg("--manifest-path")
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
        .output()?;
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let metadata: JsonValue = serde_json::from_slice(&output.stdout)?;
    let packages = metadata["packages"]
        .as_array()
        .expect("expected metadata packages");
    let package = packages
        .iter()
        .find(|package| package["name"].as_str() == Some("mcp-repl"))
        .expect("expected mcp-repl package metadata");
    let targets = package["targets"]
        .as_array()
        .expect("expected package targets");
    let bin_names = targets
        .iter()
        .filter(|target| {
            target["kind"]
                .as_array()
                .expect("expected target kind array")
                .iter()
                .any(|kind| kind.as_str() == Some("bin"))
        })
        .map(|target| target["name"].as_str().expect("expected target name"))
        .collect::<Vec<_>>();

    assert_eq!(
        bin_names,
        vec!["mcp-repl"],
        "cargo install should expose only the mcp-repl binary target"
    );

    Ok(())
}

#[test]
fn install_codex_target_defaults_to_r_and_python_servers() -> TestResult<()> {
    let temp = tempfile::tempdir()?;
    let codex_home = temp.path().join("codex-home");
    std::fs::create_dir_all(&codex_home)?;
    let exe = resolve_exe()?;

    let output = assert_command_success(
        Command::new(&exe)
            .arg("install")
            .arg("--client")
            .arg("codex")
            .env("CODEX_HOME", &codex_home),
    )?;
    assert_eq!(
        String::from_utf8(output.stdout)?,
        format!(
            "Updated codex MCP config: {}\nStart a new Codex session to load the updated MCP tools.\n",
            codex_home.join("config.toml").display()
        )
    );

    let config_path = codex_home.join("config.toml");
    let text = std::fs::read_to_string(config_path)?;
    let doc = text.parse::<DocumentMut>()?;

    assert!(
        doc["mcp_servers"]["r"].is_table(),
        "expected mcp_servers.r table"
    );
    assert!(
        doc["mcp_servers"]["python"].is_table(),
        "expected mcp_servers.python table"
    );
    assert_eq!(
        doc["mcp_servers"]["r"]["command"].as_str(),
        Some(exe.to_string_lossy().as_ref()),
        "expected install to register the current executable path"
    );

    let r_args = doc["mcp_servers"]["r"]["args"]
        .as_array()
        .expect("expected r args array");
    let has_sandbox_inherit = r_args
        .iter()
        .zip(r_args.iter().skip(1))
        .any(|(a, b)| a.as_str() == Some("--sandbox") && b.as_str() == Some("inherit"));
    assert!(
        has_sandbox_inherit,
        "expected r args to include `--sandbox inherit`"
    );
    let r_has_files_mode = r_args
        .iter()
        .zip(r_args.iter().skip(1))
        .any(|(a, b)| a.as_str() == Some("--oversized-output") && b.as_str() == Some("files"));
    assert!(
        r_has_files_mode,
        "expected r args to include `--oversized-output files`"
    );

    let py_args = doc["mcp_servers"]["python"]["args"]
        .as_array()
        .expect("expected python args array");
    let has_interpreter_python = py_args.iter().zip(py_args.iter().skip(1)).any(|(a, b)| {
        (a.as_str() == Some("--interpreter") || a.as_str() == Some("--interpreter"))
            && b.as_str() == Some("python")
    });
    assert!(
        has_interpreter_python,
        "expected python args to include python interpreter selection"
    );
    let py_has_sandbox_inherit = py_args
        .iter()
        .zip(py_args.iter().skip(1))
        .any(|(a, b)| a.as_str() == Some("--sandbox") && b.as_str() == Some("inherit"));
    assert!(
        py_has_sandbox_inherit,
        "expected python args to include `--sandbox inherit`"
    );
    let py_has_files_mode = py_args
        .iter()
        .zip(py_args.iter().skip(1))
        .any(|(a, b)| a.as_str() == Some("--oversized-output") && b.as_str() == Some("files"));
    assert!(
        py_has_files_mode,
        "expected python args to include `--oversized-output files`"
    );
    let direct_only = doc["features"]["code_mode"]["direct_only_tool_namespaces"]
        .as_array()
        .expect("expected features.code_mode.direct_only_tool_namespaces array")
        .iter()
        .map(|value| {
            value
                .as_str()
                .expect("expected direct-only namespace string")
                .to_string()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        direct_only,
        vec!["mcp__r".to_string(), "mcp__python".to_string()]
    );

    Ok(())
}

#[test]
fn install_codex_target_merges_code_mode_visibility_and_unfilters_tools() -> TestResult<()> {
    let temp = tempfile::tempdir()?;
    let codex_home = temp.path().join("codex-home");
    std::fs::create_dir_all(&codex_home)?;
    std::fs::write(
        codex_home.join("config.toml"),
        r#"[features]
code_mode = false

[mcp_servers.r]
command = "/usr/local/bin/old-mcp-repl"
enabled = false
enabled_tools = ["other"]
disabled_tools = ["repl", "other", "repl_reset"]

[mcp_servers.notes]
command = "/usr/local/bin/notes"
"#,
    )?;
    let exe = resolve_exe()?;

    let output = assert_command_success(
        Command::new(&exe)
            .arg("install")
            .arg("--client")
            .arg("codex")
            .arg("--interpreter")
            .arg("r")
            .env("CODEX_HOME", &codex_home),
    )?;
    assert_eq!(
        String::from_utf8(output.stdout)?,
        format!(
            "Updated codex MCP config: {}\nStart a new Codex session to load the updated MCP tools.\n",
            codex_home.join("config.toml").display()
        )
    );

    let text = std::fs::read_to_string(codex_home.join("config.toml"))?;
    let doc = text.parse::<DocumentMut>()?;
    assert_eq!(
        doc["features"]["code_mode"]["enabled"].as_bool(),
        Some(false),
        "expected existing features.code_mode boolean to move to enabled"
    );
    let direct_only = doc["features"]["code_mode"]["direct_only_tool_namespaces"]
        .as_array()
        .expect("expected direct-only namespaces")
        .iter()
        .map(|value| {
            value
                .as_str()
                .expect("expected direct-only namespace string")
                .to_string()
        })
        .collect::<Vec<_>>();
    assert_eq!(direct_only, vec!["mcp__r".to_string()]);

    assert_eq!(
        doc["mcp_servers"]["r"]["enabled"].as_bool(),
        Some(true),
        "expected installed Codex MCP server to be enabled"
    );
    let enabled_tools = doc["mcp_servers"]["r"]["enabled_tools"]
        .as_array()
        .expect("expected enabled_tools array")
        .iter()
        .map(|value| {
            value
                .as_str()
                .expect("expected enabled tool string")
                .to_string()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        enabled_tools,
        vec![
            "other".to_string(),
            "repl".to_string(),
            "repl_reset".to_string()
        ]
    );
    let disabled_tools = doc["mcp_servers"]["r"]["disabled_tools"]
        .as_array()
        .expect("expected disabled_tools array")
        .iter()
        .map(|value| {
            value
                .as_str()
                .expect("expected disabled tool string")
                .to_string()
        })
        .collect::<Vec<_>>();
    assert_eq!(disabled_tools, vec!["other".to_string()]);
    assert_eq!(
        doc["mcp_servers"]["notes"]["command"].as_str(),
        Some("/usr/local/bin/notes"),
        "expected unrelated MCP servers to remain"
    );

    Ok(())
}

#[test]
fn install_codex_target_merges_inline_features_table() -> TestResult<()> {
    let temp = tempfile::tempdir()?;
    let codex_home = temp.path().join("codex-home");
    std::fs::create_dir_all(&codex_home)?;
    std::fs::write(
        codex_home.join("config.toml"),
        r#"features = { code_mode = false, use_linux_sandbox_bwrap = false }

[mcp_servers.r]
command = "/usr/local/bin/old-mcp-repl"
"#,
    )?;
    let exe = resolve_exe()?;

    assert_command_success(
        Command::new(&exe)
            .arg("install")
            .arg("--client")
            .arg("codex")
            .arg("--interpreter")
            .arg("r")
            .env("CODEX_HOME", &codex_home),
    )?;

    let text = std::fs::read_to_string(codex_home.join("config.toml"))?;
    let doc = text.parse::<DocumentMut>()?;
    assert_eq!(
        doc["features"]["code_mode"]["enabled"].as_bool(),
        Some(false),
        "expected existing inline features.code_mode boolean to move to enabled"
    );
    assert_eq!(
        doc["features"]["use_linux_sandbox_bwrap"].as_bool(),
        Some(false),
        "expected unrelated inline features to remain"
    );
    let direct_only = doc["features"]["code_mode"]["direct_only_tool_namespaces"]
        .as_array()
        .expect("expected direct-only namespaces")
        .iter()
        .map(|value| {
            value
                .as_str()
                .expect("expected direct-only namespace string")
                .to_string()
        })
        .collect::<Vec<_>>();
    assert_eq!(direct_only, vec!["mcp__r".to_string()]);

    Ok(())
}

#[test]
fn install_codex_target_preserves_existing_direct_only_namespaces() -> TestResult<()> {
    let temp = tempfile::tempdir()?;
    let codex_home = temp.path().join("codex-home");
    std::fs::create_dir_all(&codex_home)?;
    std::fs::write(
        codex_home.join("config.toml"),
        r#"[features.code_mode]
enabled = false
direct_only_tool_namespaces = ["mcp__notes", "mcp__r"]
"#,
    )?;
    let exe = resolve_exe()?;

    assert_command_success(
        Command::new(&exe)
            .arg("install")
            .arg("--client")
            .arg("codex")
            .env("CODEX_HOME", &codex_home),
    )?;

    let text = std::fs::read_to_string(codex_home.join("config.toml"))?;
    let doc = text.parse::<DocumentMut>()?;
    assert_eq!(
        doc["features"]["code_mode"]["enabled"].as_bool(),
        Some(false)
    );
    let direct_only = doc["features"]["code_mode"]["direct_only_tool_namespaces"]
        .as_array()
        .expect("expected direct-only namespaces")
        .iter()
        .map(|value| {
            value
                .as_str()
                .expect("expected direct-only namespace string")
                .to_string()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        direct_only,
        vec![
            "mcp__notes".to_string(),
            "mcp__r".to_string(),
            "mcp__python".to_string()
        ]
    );

    Ok(())
}

#[test]
fn install_claude_target_defaults_to_r_and_python_servers() -> TestResult<()> {
    let temp = tempfile::tempdir()?;
    let exe = resolve_exe()?;

    let output = assert_command_success(
        Command::new(&exe)
            .arg("install")
            .arg("--client")
            .arg("claude")
            .env("HOME", temp.path()),
    )?;
    assert_eq!(
        String::from_utf8(output.stdout)?,
        format!(
            "Updated claude MCP config: {}\nUpdated claude permissions: {}\n",
            temp.path().join(".claude.json").display(),
            temp.path().join(".claude").join("settings.json").display()
        )
    );

    // Claude Code stores MCP config in ~/.claude.json (not ~/.claude/settings.json)
    let config_path = temp.path().join(".claude.json");
    let text = std::fs::read_to_string(config_path)?;
    let root: JsonValue = serde_json::from_str(&text)?;
    let servers = root["mcpServers"]
        .as_object()
        .expect("expected mcpServers object");
    assert!(servers.contains_key("r"), "expected r server");
    assert!(servers.contains_key("python"), "expected python server");
    assert_eq!(
        root["mcpServers"]["r"]["command"].as_str(),
        Some(exe.to_string_lossy().as_ref()),
        "expected install to register the current executable path"
    );

    let r_args = root["mcpServers"]["r"]["args"]
        .as_array()
        .expect("expected r args array");
    let r_has_workspace_write = r_args
        .iter()
        .zip(r_args.iter().skip(1))
        .any(|(a, b)| a.as_str() == Some("--sandbox") && b.as_str() == Some("workspace-write"));
    assert!(
        r_has_workspace_write,
        "expected r args to include `--sandbox workspace-write`"
    );
    let r_has_files_mode = r_args
        .iter()
        .zip(r_args.iter().skip(1))
        .any(|(a, b)| a.as_str() == Some("--oversized-output") && b.as_str() == Some("files"));
    assert!(
        r_has_files_mode,
        "expected r args to include `--oversized-output files`"
    );

    let py_args = root["mcpServers"]["python"]["args"]
        .as_array()
        .expect("expected python args array");
    let py_has_workspace_write = py_args
        .iter()
        .zip(py_args.iter().skip(1))
        .any(|(a, b)| a.as_str() == Some("--sandbox") && b.as_str() == Some("workspace-write"));
    assert!(
        py_has_workspace_write,
        "expected python args to include `--sandbox workspace-write`"
    );
    let py_has_files_mode = py_args
        .iter()
        .zip(py_args.iter().skip(1))
        .any(|(a, b)| a.as_str() == Some("--oversized-output") && b.as_str() == Some("files"));
    assert!(
        py_has_files_mode,
        "expected python args to include `--oversized-output files`"
    );
    let py_has_interpreter_python = py_args.iter().zip(py_args.iter().skip(1)).any(|(a, b)| {
        (a.as_str() == Some("--interpreter") || a.as_str() == Some("--interpreter"))
            && b.as_str() == Some("python")
    });
    assert!(
        py_has_interpreter_python,
        "expected python args to include python interpreter selection"
    );

    Ok(())
}

#[test]
fn install_codex_and_install_claude_commands_are_rejected() -> TestResult<()> {
    let temp = tempfile::tempdir()?;
    let codex_home = temp.path().join("codex-home");
    std::fs::create_dir_all(&codex_home)?;
    let exe = resolve_exe()?;

    for cmd in ["install-codex", "install-claude"] {
        assert_command_failure(
            Command::new(&exe)
                .arg(cmd)
                .env("CODEX_HOME", &codex_home)
                .env("HOME", temp.path()),
            &format!("Error: \"unknown argument: {cmd}\""),
        )?;
    }

    Ok(())
}

#[test]
fn install_rejects_empty_client_selector() -> TestResult<()> {
    let temp = tempfile::tempdir()?;
    let codex_home = temp.path().join("codex-home");
    std::fs::create_dir_all(&codex_home)?;
    let exe = resolve_exe()?;

    assert_command_failure(
        Command::new(&exe)
            .arg("install")
            .arg("--client")
            .arg(",")
            .env("CODEX_HOME", &codex_home)
            .env("HOME", temp.path()),
        "Error: \"empty --client value (expected codex|claude)\"",
    )?;

    Ok(())
}

#[test]
fn install_rejects_server_name_flag() -> TestResult<()> {
    let temp = tempfile::tempdir()?;
    let codex_home = temp.path().join("codex-home");
    std::fs::create_dir_all(&codex_home)?;
    let exe = resolve_exe()?;

    assert_command_failure(
        Command::new(&exe)
            .arg("install")
            .arg("--client")
            .arg("codex")
            .arg("--server-name")
            .arg("custom")
            .env("CODEX_HOME", &codex_home),
        "Error: \"unknown install option: --server-name\"",
    )?;

    Ok(())
}

#[test]
fn install_rejects_command_flag() -> TestResult<()> {
    let temp = tempfile::tempdir()?;
    let codex_home = temp.path().join("codex-home");
    std::fs::create_dir_all(&codex_home)?;
    let exe = resolve_exe()?;

    assert_command_failure(
        Command::new(&exe)
            .arg("install")
            .arg("--client")
            .arg("codex")
            .arg("--command")
            .arg("/usr/local/bin/mcp-repl")
            .env("CODEX_HOME", &codex_home),
        "Error: \"unknown install option: --command\"",
    )?;

    Ok(())
}

#[test]
fn install_rejects_backend_in_passthrough_args() -> TestResult<()> {
    let temp = tempfile::tempdir()?;
    let codex_home = temp.path().join("codex-home");
    std::fs::create_dir_all(&codex_home)?;
    let exe = resolve_exe()?;

    assert_command_failure(
        Command::new(&exe)
            .arg("install")
            .arg("--client")
            .arg("codex")
            .arg("--arg")
            .arg("--backend")
            .arg("--arg")
            .arg("python")
            .env("CODEX_HOME", &codex_home),
        "Error: \"install does not accept interpreter selection via --arg; use --interpreter r|python instead\"",
    )?;

    Ok(())
}

#[test]
fn install_rejects_positional_target_selector() -> TestResult<()> {
    let temp = tempfile::tempdir()?;
    let codex_home = temp.path().join("codex-home");
    std::fs::create_dir_all(&codex_home)?;
    let exe = resolve_exe()?;

    for target in ["codex", "claude"] {
        assert_command_failure(
            Command::new(&exe)
                .arg("install")
                .arg(target)
                .env("CODEX_HOME", &codex_home)
                .env("HOME", temp.path()),
            &format!("Error: \"unknown install argument: {target} (use --client codex|claude)\""),
        )?;
    }

    Ok(())
}

#[test]
fn install_subcommands_are_rejected() -> TestResult<()> {
    let temp = tempfile::tempdir()?;
    let codex_home = temp.path().join("codex-home");
    std::fs::create_dir_all(&codex_home)?;
    let exe = resolve_exe()?;

    assert_command_failure(
        Command::new(&exe)
            .arg("install-codex")
            .env("CODEX_HOME", &codex_home),
        "Error: \"unknown argument: install-codex\"",
    )?;

    assert_command_failure(
        Command::new(exe)
            .arg("install-claude")
            .env("HOME", temp.path()),
        "Error: \"unknown argument: install-claude\"",
    )?;

    Ok(())
}
