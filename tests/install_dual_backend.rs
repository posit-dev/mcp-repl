mod common;

use std::path::PathBuf;
use std::process::Command;

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

#[test]
fn install_codex_target_defaults_to_r_and_python_servers() -> TestResult<()> {
    let temp = tempfile::tempdir()?;
    let codex_home = temp.path().join("codex-home");
    std::fs::create_dir_all(&codex_home)?;
    let exe = resolve_exe()?;

    let status = Command::new(&exe)
        .arg("install")
        .arg("--client")
        .arg("codex")
        .env("CODEX_HOME", &codex_home)
        .status()?;
    assert!(
        status.success(),
        "install --client codex failed with status {status}"
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

    Ok(())
}

#[test]
fn install_claude_target_defaults_to_r_and_python_servers() -> TestResult<()> {
    let temp = tempfile::tempdir()?;
    let exe = resolve_exe()?;

    let status = Command::new(&exe)
        .arg("install")
        .arg("--client")
        .arg("claude")
        .env("HOME", temp.path())
        .status()?;
    assert!(
        status.success(),
        "install --client claude failed with status {status}"
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
        let status = Command::new(&exe)
            .arg(cmd)
            .env("CODEX_HOME", &codex_home)
            .env("HOME", temp.path())
            .status()?;
        assert!(
            !status.success(),
            "expected `{cmd}` to be rejected, got status {status}"
        );
    }

    Ok(())
}

#[test]
fn install_rejects_empty_client_selector() -> TestResult<()> {
    let temp = tempfile::tempdir()?;
    let codex_home = temp.path().join("codex-home");
    std::fs::create_dir_all(&codex_home)?;
    let exe = resolve_exe()?;

    let status = Command::new(&exe)
        .arg("install")
        .arg("--client")
        .arg(",")
        .env("CODEX_HOME", &codex_home)
        .env("HOME", temp.path())
        .status()?;

    assert!(
        !status.success(),
        "expected install with empty --client selector to fail"
    );

    Ok(())
}

#[test]
fn install_rejects_server_name_flag() -> TestResult<()> {
    let temp = tempfile::tempdir()?;
    let codex_home = temp.path().join("codex-home");
    std::fs::create_dir_all(&codex_home)?;
    let exe = resolve_exe()?;

    let status = Command::new(&exe)
        .arg("install")
        .arg("--client")
        .arg("codex")
        .arg("--server-name")
        .arg("custom")
        .env("CODEX_HOME", &codex_home)
        .status()?;

    assert!(
        !status.success(),
        "expected install with --server-name to fail"
    );

    Ok(())
}

#[test]
fn install_rejects_command_flag() -> TestResult<()> {
    let temp = tempfile::tempdir()?;
    let codex_home = temp.path().join("codex-home");
    std::fs::create_dir_all(&codex_home)?;
    let exe = resolve_exe()?;

    let status = Command::new(&exe)
        .arg("install")
        .arg("--client")
        .arg("codex")
        .arg("--command")
        .arg("/usr/local/bin/mcp-repl")
        .env("CODEX_HOME", &codex_home)
        .status()?;

    assert!(!status.success(), "expected install with --command to fail");

    Ok(())
}

#[test]
fn install_rejects_backend_in_passthrough_args() -> TestResult<()> {
    let temp = tempfile::tempdir()?;
    let codex_home = temp.path().join("codex-home");
    std::fs::create_dir_all(&codex_home)?;
    let exe = resolve_exe()?;

    let status = Command::new(&exe)
        .arg("install")
        .arg("--client")
        .arg("codex")
        .arg("--arg")
        .arg("--backend")
        .arg("--arg")
        .arg("python")
        .env("CODEX_HOME", &codex_home)
        .status()?;

    assert!(
        !status.success(),
        "expected install with --arg --backend to fail"
    );

    Ok(())
}

#[test]
fn install_rejects_positional_target_selector() -> TestResult<()> {
    let temp = tempfile::tempdir()?;
    let codex_home = temp.path().join("codex-home");
    std::fs::create_dir_all(&codex_home)?;
    let exe = resolve_exe()?;

    for target in ["codex", "claude"] {
        let status = Command::new(&exe)
            .arg("install")
            .arg(target)
            .env("CODEX_HOME", &codex_home)
            .env("HOME", temp.path())
            .status()?;

        assert!(
            !status.success(),
            "expected install {target} to fail without --client"
        );
    }

    Ok(())
}

#[test]
fn install_subcommands_are_rejected() -> TestResult<()> {
    let temp = tempfile::tempdir()?;
    let codex_home = temp.path().join("codex-home");
    std::fs::create_dir_all(&codex_home)?;
    let exe = resolve_exe()?;

    let codex_status = Command::new(&exe)
        .arg("install-codex")
        .env("CODEX_HOME", &codex_home)
        .status()?;
    assert!(
        !codex_status.success(),
        "install-codex should fail after subcommand removal"
    );

    let claude_status = Command::new(exe)
        .arg("install-claude")
        .env("HOME", temp.path())
        .status()?;
    assert!(
        !claude_status.success(),
        "install-claude should fail after subcommand removal"
    );

    Ok(())
}
