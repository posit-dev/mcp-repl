mod common;

use std::path::Path;
use std::process::Command;

use common::TestResult;
use serde_json::Value as JsonValue;
use toml_edit::DocumentMut;

const CLAUDE_SESSION_END_MATCHERS: &[&str] = &["clear", "prompt_input_exit", "other"];

fn claude_hook_entries<'a>(settings_root: &'a JsonValue, event: &str) -> &'a [JsonValue] {
    settings_root["hooks"][event]
        .as_array()
        .map(Vec::as_slice)
        .expect("expected Claude hooks event array")
}

fn installed_claude_hook_command(_home: &Path, command: &str, args: &[&str], hook: &str) -> String {
    std::iter::once(command)
        .chain(args.iter().copied())
        .chain(["claude-hook", hook])
        .map(posix_escape)
        .collect::<Vec<_>>()
        .join(" ")
}

fn all_claude_hook_commands(settings_root: &JsonValue) -> Vec<&str> {
    settings_root["hooks"]
        .as_object()
        .into_iter()
        .flat_map(|events| events.values())
        .filter_map(JsonValue::as_array)
        .flatten()
        .filter_map(|entry| entry["hooks"].as_array())
        .flatten()
        .filter_map(|hook| hook["command"].as_str())
        .collect()
}

fn posix_escape(raw: &str) -> String {
    if raw.is_empty() {
        return "''".to_string();
    }
    if raw.bytes().all(|byte| {
        matches!(
            byte,
            b'A'..=b'Z'
                | b'a'..=b'z'
                | b'0'..=b'9'
                | b'/'
                | b'.'
                | b'_'
                | b'-'
                | b':'
        )
    }) {
        return raw.to_string();
    }
    format!("'{}'", raw.replace('\'', "'\"'\"'"))
}

#[test]
fn install_codex_target_defaults_to_r_and_python_servers() -> TestResult<()> {
    let temp = tempfile::tempdir()?;
    let codex_home = temp.path().join("codex-home");
    std::fs::create_dir_all(&codex_home)?;
    let exe = common::resolve_test_binary()?;

    let status = Command::new(exe)
        .arg("install")
        .arg("--client")
        .arg("codex")
        .arg("--command")
        .arg("/usr/local/bin/mcp-repl")
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

    let py_args = doc["mcp_servers"]["python"]["args"]
        .as_array()
        .expect("expected python args array");
    let has_interpreter_python = py_args.iter().zip(py_args.iter().skip(1)).any(|(a, b)| {
        (a.as_str() == Some("--interpreter") || a.as_str() == Some("--backend"))
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

    Ok(())
}

#[test]
fn install_claude_target_defaults_to_r_and_python_servers() -> TestResult<()> {
    let temp = tempfile::tempdir()?;
    let exe = common::resolve_test_binary()?;

    let status = Command::new(exe)
        .arg("install")
        .arg("--client")
        .arg("claude")
        .arg("--command")
        .arg("/usr/local/bin/mcp-repl")
        .env("HOME", temp.path())
        .status()?;
    assert!(
        status.success(),
        "install --client claude failed with status {status}"
    );

    let config_path = temp.path().join(".claude.json");
    let text = std::fs::read_to_string(config_path)?;
    let root: JsonValue = serde_json::from_str(&text)?;
    let servers = root["mcpServers"]
        .as_object()
        .expect("expected mcpServers object");
    assert!(servers.contains_key("r"), "expected r server");
    assert!(servers.contains_key("python"), "expected python server");

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
    let py_has_interpreter_python = py_args.iter().zip(py_args.iter().skip(1)).any(|(a, b)| {
        (a.as_str() == Some("--interpreter") || a.as_str() == Some("--backend"))
            && b.as_str() == Some("python")
    });
    assert!(
        py_has_interpreter_python,
        "expected python args to include python interpreter selection"
    );
    assert!(
        root["mcpServers"]["r"].get("env").is_none(),
        "expected r server config not to depend on a session env file passthrough"
    );
    assert!(
        root["mcpServers"]["python"].get("env").is_none(),
        "expected python server config not to depend on a session env file passthrough"
    );

    let settings_path = temp.path().join(".claude/settings.json");
    let settings_text = std::fs::read_to_string(settings_path)?;
    let settings_root: JsonValue = serde_json::from_str(&settings_text)?;
    let expected_session_start =
        installed_claude_hook_command(temp.path(), "/usr/local/bin/mcp-repl", &[], "session-start");
    let session_start = claude_hook_entries(&settings_root, "SessionStart");
    assert!(
        session_start.iter().any(|entry| {
            entry["matcher"].as_str() == Some("startup")
                && entry["hooks"].as_array().is_some_and(|hooks| {
                    hooks.iter().any(|hook| {
                        hook["type"].as_str() == Some("command")
                            && hook["command"].as_str() == Some(expected_session_start.as_str())
                    })
                })
        }),
        "expected startup SessionStart hook"
    );
    let session_end = claude_hook_entries(&settings_root, "SessionEnd");
    let expected_session_end =
        installed_claude_hook_command(temp.path(), "/usr/local/bin/mcp-repl", &[], "session-end");
    for matcher in CLAUDE_SESSION_END_MATCHERS {
        assert!(
            session_end.iter().any(|entry| {
                entry["matcher"].as_str() == Some(*matcher)
                    && entry["hooks"].as_array().is_some_and(|hooks| {
                        hooks.iter().any(|hook| {
                            hook["type"].as_str() == Some("command")
                                && hook["command"].as_str() == Some(expected_session_end.as_str())
                        })
                    })
            }),
            "expected {matcher} SessionEnd hook"
        );
    }
    assert!(
        settings_root.get("hooks").is_some(),
        "expected Claude settings to store hooks under the hooks object"
    );

    Ok(())
}

#[test]
fn install_claude_target_does_not_hardcode_shared_session_env_file() -> TestResult<()> {
    let temp = tempfile::tempdir()?;
    let exe = common::resolve_test_binary()?;

    let status = Command::new(exe)
        .arg("install")
        .arg("--client")
        .arg("claude")
        .arg("--command")
        .arg("/usr/local/bin/mcp-repl")
        .env("HOME", temp.path())
        .status()?;
    assert!(
        status.success(),
        "install --client claude failed with status {status}"
    );

    let config_path = temp.path().join(".claude.json");
    let text = std::fs::read_to_string(config_path)?;
    let root: JsonValue = serde_json::from_str(&text)?;
    for server_name in ["r", "python"] {
        let server = root["mcpServers"][server_name]
            .as_object()
            .expect("expected Claude server object");
        assert!(
            server.get("env").is_none(),
            "expected {server_name} server config not to depend on a session env file passthrough"
        );
    }

    let settings_path = temp.path().join(".claude/settings.json");
    let settings_text = std::fs::read_to_string(settings_path)?;
    let settings_root: JsonValue = serde_json::from_str(&settings_text)?;
    let session_start_commands = all_claude_hook_commands(&settings_root)
        .into_iter()
        .filter(|command| command.contains("claude-hook session-start"))
        .collect::<Vec<_>>();
    let session_end_commands = all_claude_hook_commands(&settings_root)
        .into_iter()
        .filter(|command| command.contains("claude-hook session-end"))
        .collect::<Vec<_>>();
    assert!(
        session_start_commands
            .iter()
            .all(|command| !command.contains("CLAUDE_ENV_FILE=")),
        "expected SessionStart hooks not to prefix a shared CLAUDE_ENV_FILE"
    );
    assert!(
        session_end_commands
            .iter()
            .all(|command| !command.contains("CLAUDE_ENV_FILE=")),
        "expected SessionEnd hooks not to prefix a shared CLAUDE_ENV_FILE"
    );

    Ok(())
}

#[test]
fn install_claude_reinstall_with_custom_command_replaces_hook_commands() -> TestResult<()> {
    let temp = tempfile::tempdir()?;
    let exe = common::resolve_test_binary()?;
    let old_command = "/opt/repltool";
    let new_command = "/opt/repltool-v2";

    let first_status = Command::new(&exe)
        .arg("install")
        .arg("--client")
        .arg("claude")
        .arg("--command")
        .arg(old_command)
        .env("HOME", temp.path())
        .status()?;
    assert!(
        first_status.success(),
        "initial install --client claude failed with status {first_status}"
    );

    let second_status = Command::new(&exe)
        .arg("install")
        .arg("--client")
        .arg("claude")
        .arg("--command")
        .arg(new_command)
        .env("HOME", temp.path())
        .status()?;
    assert!(
        second_status.success(),
        "reinstall --client claude failed with status {second_status}"
    );

    let settings_path = temp.path().join(".claude/settings.json");
    let settings_text = std::fs::read_to_string(settings_path)?;
    let settings_root: JsonValue = serde_json::from_str(&settings_text)?;

    let expected_session_start =
        installed_claude_hook_command(temp.path(), new_command, &[], "session-start");
    let expected_session_end =
        installed_claude_hook_command(temp.path(), new_command, &[], "session-end");
    let stale_session_start = format!("{old_command} claude-hook session-start");
    let stale_session_end = format!("{old_command} claude-hook session-end");

    for matcher in ["startup", "resume", "clear", "compact"] {
        let entry = claude_hook_entries(&settings_root, "SessionStart")
            .iter()
            .find(|entry| entry["matcher"].as_str() == Some(matcher))
            .expect("expected SessionStart matcher entry");
        let commands: Vec<&str> = entry["hooks"]
            .as_array()
            .expect("expected hooks array")
            .iter()
            .filter_map(|hook| hook["command"].as_str())
            .filter(|command| command.contains("claude-hook session-start"))
            .collect();
        assert_eq!(
            commands,
            vec![expected_session_start.as_str()],
            "expected one updated SessionStart command for matcher {matcher}"
        );
    }

    for matcher in CLAUDE_SESSION_END_MATCHERS {
        let entry = claude_hook_entries(&settings_root, "SessionEnd")
            .iter()
            .find(|entry| entry["matcher"].as_str() == Some(*matcher))
            .expect("expected SessionEnd matcher entry");
        let commands: Vec<&str> = entry["hooks"]
            .as_array()
            .expect("expected hooks array")
            .iter()
            .filter_map(|hook| hook["command"].as_str())
            .filter(|command| command.contains("claude-hook session-end"))
            .collect();
        assert_eq!(
            commands,
            vec![expected_session_end.as_str()],
            "expected one updated SessionEnd command for matcher {matcher}"
        );
    }

    let all_commands = all_claude_hook_commands(&settings_root);
    assert!(
        !all_commands.contains(&stale_session_start.as_str()),
        "expected stale SessionStart command to be removed"
    );
    assert!(
        !all_commands.contains(&stale_session_end.as_str()),
        "expected stale SessionEnd command to be removed"
    );

    Ok(())
}

#[test]
fn install_claude_ignores_top_level_hook_entries() -> TestResult<()> {
    let temp = tempfile::tempdir()?;
    let claude_dir = temp.path().join(".claude");
    std::fs::create_dir_all(&claude_dir)?;
    let settings_path = claude_dir.join("settings.json");
    std::fs::write(
        &settings_path,
        serde_json::to_string_pretty(&serde_json::json!({
            "SessionStart": [{"matcher": "startup", "hooks": []}]
        }))?,
    )?;
    let exe = common::resolve_test_binary()?;

    let status = Command::new(exe)
        .arg("install")
        .arg("--client")
        .arg("claude")
        .arg("--command")
        .arg("/usr/local/bin/mcp-repl")
        .env("HOME", temp.path())
        .status()?;
    assert!(
        status.success(),
        "install should ignore top-level hook keys instead of failing"
    );

    let settings_text = std::fs::read_to_string(settings_path)?;
    let settings_root: JsonValue = serde_json::from_str(&settings_text)?;
    assert!(
        settings_root["SessionStart"].is_array(),
        "expected top-level SessionStart entries to remain untouched"
    );
    assert!(
        settings_root["hooks"]["SessionStart"].is_array(),
        "expected canonical hooks.SessionStart entries to be written"
    );

    Ok(())
}

#[test]
fn install_claude_ignores_duplicate_hook_matchers() -> TestResult<()> {
    let temp = tempfile::tempdir()?;
    let claude_dir = temp.path().join(".claude");
    std::fs::create_dir_all(&claude_dir)?;
    let settings_path = claude_dir.join("settings.json");
    std::fs::write(
        &settings_path,
        serde_json::to_string_pretty(&serde_json::json!({
            "hooks": {
                "SessionStart": [
                    {"matcher": "startup", "hooks": [{"type": "command", "command": "/opt/old/mcp-repl claude-hook session-start"}]},
                    {"matcher": "startup", "hooks": [{"type": "command", "command": "echo keep-me"}]}
                ]
            }
        }))?,
    )?;
    let exe = common::resolve_test_binary()?;

    let status = Command::new(exe)
        .arg("install")
        .arg("--client")
        .arg("claude")
        .arg("--command")
        .arg("/usr/local/bin/mcp-repl")
        .env("HOME", temp.path())
        .status()?;
    assert!(
        status.success(),
        "install should ignore duplicate hook matchers instead of failing"
    );

    let settings_text = std::fs::read_to_string(settings_path)?;
    let settings_root: JsonValue = serde_json::from_str(&settings_text)?;
    let startup_entries: Vec<&JsonValue> = claude_hook_entries(&settings_root, "SessionStart")
        .iter()
        .filter(|entry| entry["matcher"].as_str() == Some("startup"))
        .collect();
    assert_eq!(
        startup_entries.len(),
        2,
        "expected duplicate startup entries to remain in place"
    );
    let all_startup_commands: Vec<&str> = startup_entries
        .iter()
        .flat_map(|entry| entry["hooks"].as_array().into_iter().flatten())
        .filter_map(|hook| hook["command"].as_str())
        .collect();
    let expected =
        installed_claude_hook_command(temp.path(), "/usr/local/bin/mcp-repl", &[], "session-start");
    assert!(
        all_startup_commands.contains(&expected.as_str()),
        "expected one startup entry to contain the current managed command"
    );
    assert!(
        all_startup_commands.contains(&"echo keep-me"),
        "expected unrelated duplicate-entry commands to remain"
    );
    assert!(
        !all_startup_commands.contains(&"/opt/old/mcp-repl claude-hook session-start"),
        "expected stale managed command to be removed even when duplicate entries remain"
    );

    Ok(())
}

#[test]
fn install_codex_and_install_claude_commands_are_rejected() -> TestResult<()> {
    let temp = tempfile::tempdir()?;
    let codex_home = temp.path().join("codex-home");
    std::fs::create_dir_all(&codex_home)?;
    let exe = common::resolve_test_binary()?;

    for cmd in ["install-codex", "install-claude"] {
        let status = Command::new(&exe)
            .arg(cmd)
            .arg("--command")
            .arg("/usr/local/bin/mcp-repl")
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
    let exe = common::resolve_test_binary()?;

    let status = Command::new(&exe)
        .arg("install")
        .arg("--client")
        .arg(",")
        .arg("--command")
        .arg("/usr/local/bin/mcp-repl")
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
fn install_subcommands_are_rejected() -> TestResult<()> {
    let temp = tempfile::tempdir()?;
    let codex_home = temp.path().join("codex-home");
    std::fs::create_dir_all(&codex_home)?;
    let exe = common::resolve_test_binary()?;

    let codex_status = Command::new(&exe)
        .arg("install-codex")
        .arg("--command")
        .arg("/usr/local/bin/mcp-repl")
        .env("CODEX_HOME", &codex_home)
        .status()?;
    assert!(
        !codex_status.success(),
        "install-codex should fail after subcommand removal"
    );

    let claude_status = Command::new(exe)
        .arg("install-claude")
        .arg("--command")
        .arg("/usr/local/bin/mcp-repl")
        .env("HOME", temp.path())
        .status()?;
    assert!(
        !claude_status.success(),
        "install-claude should fail after subcommand removal"
    );

    Ok(())
}
