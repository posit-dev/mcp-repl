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

fn claude_session_env_file(home: &Path) -> String {
    home.join(".claude")
        .join("mcp-repl")
        .join("session.env")
        .display()
        .to_string()
}

fn installed_claude_hook_command(home: &Path, command: &str, args: &[&str], hook: &str) -> String {
    let base = std::iter::once(command)
        .chain(args.iter().copied())
        .chain(["claude-hook", hook])
        .map(posix_escape)
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "CLAUDE_ENV_FILE={} {base}",
        posix_escape(&claude_session_env_file(home))
    )
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

    // Claude Code stores MCP config in ~/.claude.json (not ~/.claude/settings.json)
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
    let expected_env_file = claude_session_env_file(temp.path());
    assert_eq!(
        root["mcpServers"]["r"]["env"]["CLAUDE_ENV_FILE"].as_str(),
        Some(expected_env_file.as_str()),
        "expected r server to receive CLAUDE_ENV_FILE"
    );
    assert_eq!(
        root["mcpServers"]["python"]["env"]["CLAUDE_ENV_FILE"].as_str(),
        Some(expected_env_file.as_str()),
        "expected python server to receive CLAUDE_ENV_FILE"
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

    let session_start = claude_hook_entries(&settings_root, "SessionStart");
    for matcher in ["startup", "resume"] {
        let entry = session_start
            .iter()
            .find(|entry| entry["matcher"].as_str() == Some(matcher))
            .expect("expected SessionStart matcher entry");
        let hooks = entry["hooks"].as_array().expect("expected hooks array");
        let commands: Vec<&str> = hooks
            .iter()
            .filter_map(|hook| hook["command"].as_str())
            .filter(|command| command.contains("claude-hook session-start"))
            .collect();
        let expected =
            installed_claude_hook_command(temp.path(), new_command, &[], "session-start");
        assert_eq!(
            commands,
            vec![expected.as_str()],
            "expected one updated SessionStart command for matcher {matcher}"
        );
    }

    let session_end = claude_hook_entries(&settings_root, "SessionEnd");
    let expected_session_end =
        installed_claude_hook_command(temp.path(), new_command, &[], "session-end");
    for matcher in CLAUDE_SESSION_END_MATCHERS {
        let entry = session_end
            .iter()
            .find(|entry| entry["matcher"].as_str() == Some(*matcher))
            .expect("expected SessionEnd matcher entry");
        let hooks = entry["hooks"]
            .as_array()
            .expect("expected SessionEnd hooks array");
        let commands: Vec<&str> = hooks
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

    let stale_session_start = format!("{old_command} claude-hook session-start");
    let stale_session_end = format!("{old_command} claude-hook session-end");
    let all_commands = all_claude_hook_commands(&settings_root);
    assert!(
        !all_commands.contains(&stale_session_start.as_str()),
        "expected stale SessionStart command to be removed"
    );
    assert!(
        !all_commands.contains(&stale_session_end.as_str()),
        "expected stale SessionEnd command to be removed"
    );
    assert!(
        settings_root.get("hooks").is_some(),
        "expected Claude settings to keep hooks under the hooks object"
    );

    Ok(())
}

#[test]
fn install_claude_updates_existing_top_level_hook_commands() -> TestResult<()> {
    let temp = tempfile::tempdir()?;
    let claude_dir = temp.path().join(".claude");
    std::fs::create_dir_all(&claude_dir)?;
    let config_path = temp.path().join(".claude.json");
    let settings_path = claude_dir.join("settings.json");
    let old_command = "/opt/old/mcp-repl";
    let new_command = "/opt/new/mcp-repl";
    let seeded_config = serde_json::json!({
        "mcpServers": {
            "r": {
                "command": old_command,
                "args": ["--sandbox", "workspace-write", "--interpreter", "r"]
            },
            "python": {
                "command": old_command,
                "args": ["--sandbox", "workspace-write", "--interpreter", "python"]
            }
        }
    });
    std::fs::write(&config_path, serde_json::to_string_pretty(&seeded_config)?)?;
    let seeded = serde_json::json!({
        "SessionStart": [
            {
                "matcher": "startup",
                "hooks": [
                    {"type": "command", "command": format!("{old_command} claude-hook session-start")},
                    {"type": "command", "command": "echo keep-me"}
                ]
            }
        ],
        "SessionEnd": [
            {
                "matcher": "clear",
                "hooks": [
                    {"type": "command", "command": format!("{old_command} claude-hook session-end")}
                ]
            }
        ]
    });
    std::fs::write(&settings_path, serde_json::to_string_pretty(&seeded)?)?;

    let exe = common::resolve_test_binary()?;
    let status = Command::new(exe)
        .arg("install")
        .arg("--client")
        .arg("claude")
        .arg("--command")
        .arg(new_command)
        .env("HOME", temp.path())
        .status()?;
    assert!(
        status.success(),
        "install --client claude failed with status {status}"
    );

    let settings_text = std::fs::read_to_string(settings_path)?;
    let settings_root: JsonValue = serde_json::from_str(&settings_text)?;
    let expected_session_start =
        installed_claude_hook_command(temp.path(), new_command, &[], "session-start");
    let stale_session_start = format!("{old_command} claude-hook session-start");
    let session_start = claude_hook_entries(&settings_root, "SessionStart");
    let startup = session_start
        .iter()
        .find(|entry| entry["matcher"].as_str() == Some("startup"))
        .expect("expected startup SessionStart matcher");
    let startup_commands: Vec<&str> = startup["hooks"]
        .as_array()
        .expect("expected SessionStart hooks array")
        .iter()
        .filter_map(|hook| hook["command"].as_str())
        .collect();
    assert!(
        startup_commands.contains(&"echo keep-me"),
        "expected unrelated SessionStart command to remain"
    );
    assert!(
        startup_commands.contains(&expected_session_start.as_str()),
        "expected updated SessionStart command"
    );
    assert!(
        !startup_commands.contains(&stale_session_start.as_str()),
        "expected stale SessionStart command to be removed"
    );

    let expected_session_end =
        installed_claude_hook_command(temp.path(), new_command, &[], "session-end");
    let session_end = claude_hook_entries(&settings_root, "SessionEnd");
    let clear = session_end
        .iter()
        .find(|entry| entry["matcher"].as_str() == Some("clear"))
        .expect("expected clear SessionEnd matcher");
    let session_end_commands: Vec<&str> = clear["hooks"]
        .as_array()
        .expect("expected SessionEnd hooks array")
        .iter()
        .filter_map(|hook| hook["command"].as_str())
        .collect();
    assert_eq!(
        session_end_commands,
        vec![expected_session_end.as_str()],
        "expected updated SessionEnd command"
    );

    Ok(())
}

#[test]
fn install_claude_reinstall_replaces_old_hook_commands_when_server_names_change() -> TestResult<()>
{
    let temp = tempfile::tempdir()?;
    let claude_dir = temp.path().join(".claude");
    std::fs::create_dir_all(&claude_dir)?;
    let config_path = temp.path().join(".claude.json");
    let settings_path = claude_dir.join("settings.json");
    let old_command = "/opt/old/mcp-repl";
    let new_command = "/opt/new/mcp-repl";
    let seeded_config = serde_json::json!({
        "mcpServers": {
            "legacy-r": {
                "command": old_command,
                "args": ["--interpreter", "r"]
            }
        }
    });
    std::fs::write(&config_path, serde_json::to_string_pretty(&seeded_config)?)?;
    let stale_session_start = format!("{old_command} claude-hook session-start");
    let stale_session_end = format!("{old_command} claude-hook session-end");
    let seeded = serde_json::json!({
        "SessionStart": [
            {
                "matcher": "startup",
                "hooks": [
                    {"type": "command", "command": stale_session_start}
                ]
            }
        ],
        "SessionEnd": [
            {
                "matcher": "clear",
                "hooks": [
                    {"type": "command", "command": stale_session_end}
                ]
            }
        ]
    });
    std::fs::write(&settings_path, serde_json::to_string_pretty(&seeded)?)?;

    let exe = common::resolve_test_binary()?;
    let status = Command::new(exe)
        .arg("install")
        .arg("--client")
        .arg("claude")
        .arg("--command")
        .arg(new_command)
        .env("HOME", temp.path())
        .status()?;
    assert!(
        status.success(),
        "install --client claude failed with status {status}"
    );

    let settings_text = std::fs::read_to_string(settings_path)?;
    let settings_root: JsonValue = serde_json::from_str(&settings_text)?;
    let expected_session_start =
        installed_claude_hook_command(temp.path(), new_command, &[], "session-start");
    let expected_session_end =
        installed_claude_hook_command(temp.path(), new_command, &[], "session-end");

    let startup = claude_hook_entries(&settings_root, "SessionStart")
        .iter()
        .find(|entry| entry["matcher"].as_str() == Some("startup"))
        .expect("expected startup SessionStart matcher");
    let startup_commands: Vec<&str> = startup["hooks"]
        .as_array()
        .expect("expected startup hooks array")
        .iter()
        .filter_map(|hook| hook["command"].as_str())
        .collect();
    assert_eq!(
        startup_commands,
        vec![expected_session_start.as_str()],
        "expected stale SessionStart hook from old server name to be replaced"
    );

    let clear = claude_hook_entries(&settings_root, "SessionEnd")
        .iter()
        .find(|entry| entry["matcher"].as_str() == Some("clear"))
        .expect("expected clear SessionEnd matcher");
    let clear_commands: Vec<&str> = clear["hooks"]
        .as_array()
        .expect("expected clear hooks array")
        .iter()
        .filter_map(|hook| hook["command"].as_str())
        .collect();
    assert_eq!(
        clear_commands,
        vec![expected_session_end.as_str()],
        "expected stale SessionEnd hook from old server name to be replaced"
    );

    Ok(())
}

#[test]
fn install_claude_migrates_wrapper_style_hook_settings() -> TestResult<()> {
    let temp = tempfile::tempdir()?;
    let claude_dir = temp.path().join(".claude");
    std::fs::create_dir_all(&claude_dir)?;
    let config_path = temp.path().join(".claude.json");
    let settings_path = claude_dir.join("settings.json");
    let old_command = "/opt/old/mcp-repl";
    let new_command = "/opt/new/mcp-repl";
    let seeded_config = serde_json::json!({
        "mcpServers": {
            "r": {
                "command": old_command,
                "args": ["--interpreter", "r"]
            },
            "python": {
                "command": old_command,
                "args": ["--interpreter", "python"]
            }
        }
    });
    std::fs::write(&config_path, serde_json::to_string_pretty(&seeded_config)?)?;
    let stale_session_start = format!("{old_command} claude-hook session-start");
    let stale_session_end = format!("{old_command} claude-hook session-end");
    let seeded = serde_json::json!({
        "hooks": {
            "SessionStart": [
                {
                    "matcher": "startup",
                    "hooks": [
                        {"type": "command", "command": stale_session_start},
                        {"type": "command", "command": "echo keep-me"}
                    ]
                }
            ],
            "SessionEnd": [
                {
                    "matcher": "clear",
                    "hooks": [
                        {"type": "command", "command": stale_session_end}
                    ]
                }
            ]
        }
    });
    std::fs::write(&settings_path, serde_json::to_string_pretty(&seeded)?)?;

    let exe = common::resolve_test_binary()?;
    let status = Command::new(exe)
        .arg("install")
        .arg("--client")
        .arg("claude")
        .arg("--command")
        .arg(new_command)
        .env("HOME", temp.path())
        .status()?;
    assert!(
        status.success(),
        "install --client claude failed with status {status}"
    );

    let settings_text = std::fs::read_to_string(settings_path)?;
    let settings_root: JsonValue = serde_json::from_str(&settings_text)?;
    let expected_session_start =
        installed_claude_hook_command(temp.path(), new_command, &[], "session-start");
    let expected_session_end =
        installed_claude_hook_command(temp.path(), new_command, &[], "session-end");

    assert!(
        settings_root.get("hooks").is_some(),
        "expected wrapper-style hooks object to remain the canonical hook location"
    );

    let startup = claude_hook_entries(&settings_root, "SessionStart")
        .iter()
        .find(|entry| entry["matcher"].as_str() == Some("startup"))
        .expect("expected startup SessionStart matcher");
    let startup_commands: Vec<&str> = startup["hooks"]
        .as_array()
        .expect("expected startup hooks array")
        .iter()
        .filter_map(|hook| hook["command"].as_str())
        .collect();
    assert!(
        startup_commands.contains(&"echo keep-me"),
        "expected unrelated wrapper SessionStart command to remain"
    );
    assert!(
        startup_commands.contains(&expected_session_start.as_str()),
        "expected updated SessionStart command"
    );
    assert!(
        !startup_commands.contains(&stale_session_start.as_str()),
        "expected stale wrapped SessionStart command to be removed"
    );

    let clear = claude_hook_entries(&settings_root, "SessionEnd")
        .iter()
        .find(|entry| entry["matcher"].as_str() == Some("clear"))
        .expect("expected clear SessionEnd matcher");
    let clear_commands: Vec<&str> = clear["hooks"]
        .as_array()
        .expect("expected clear hooks array")
        .iter()
        .filter_map(|hook| hook["command"].as_str())
        .collect();
    assert_eq!(
        clear_commands,
        vec![expected_session_end.as_str()],
        "expected wrapped SessionEnd command to be replaced"
    );

    Ok(())
}

#[test]
fn install_claude_reinstall_preserves_unrelated_commands_that_only_mention_hook_name()
-> TestResult<()> {
    let temp = tempfile::tempdir()?;
    let claude_dir = temp.path().join(".claude");
    std::fs::create_dir_all(&claude_dir)?;
    let config_path = temp.path().join(".claude.json");
    let settings_path = claude_dir.join("settings.json");
    let old_command = "/opt/old/mcp-repl";
    let new_command = "/opt/new/mcp-repl";
    let seeded_config = serde_json::json!({
        "mcpServers": {
            "r": {
                "command": old_command,
                "args": ["--sandbox", "workspace-write", "--interpreter", "r"]
            },
            "python": {
                "command": old_command,
                "args": ["--sandbox", "workspace-write", "--interpreter", "python"]
            }
        }
    });
    std::fs::write(&config_path, serde_json::to_string_pretty(&seeded_config)?)?;
    let seeded = serde_json::json!({
        "SessionStart": [
            {
                "matcher": "startup",
                "hooks": [
                    {"type": "command", "command": format!("{old_command} claude-hook session-start")},
                    {"type": "command", "command": "echo \"claude-hook session-start\""},
                    {"type": "command", "command": "/opt/custom-tool claude-hook session-start"}
                ]
            }
        ]
    });
    std::fs::write(&settings_path, serde_json::to_string_pretty(&seeded)?)?;

    let exe = common::resolve_test_binary()?;
    let status = Command::new(exe)
        .arg("install")
        .arg("--client")
        .arg("claude")
        .arg("--command")
        .arg(new_command)
        .env("HOME", temp.path())
        .status()?;
    assert!(
        status.success(),
        "install --client claude failed with status {status}"
    );

    let settings_text = std::fs::read_to_string(settings_path)?;
    let settings_root: JsonValue = serde_json::from_str(&settings_text)?;
    let startup = claude_hook_entries(&settings_root, "SessionStart")
        .iter()
        .find(|entry| entry["matcher"].as_str() == Some("startup"))
        .expect("expected startup SessionStart matcher");
    let startup_commands: Vec<&str> = startup["hooks"]
        .as_array()
        .expect("expected SessionStart hooks array")
        .iter()
        .filter_map(|hook| hook["command"].as_str())
        .collect();
    let expected_session_start =
        installed_claude_hook_command(temp.path(), new_command, &[], "session-start");
    let stale_session_start = format!("{old_command} claude-hook session-start");

    assert!(
        startup_commands.contains(&"echo \"claude-hook session-start\""),
        "expected unrelated command mentioning the hook name to remain"
    );
    assert!(
        startup_commands.contains(&"/opt/custom-tool claude-hook session-start"),
        "expected unrelated suffix-matching command to remain"
    );
    assert!(
        startup_commands.contains(&expected_session_start.as_str()),
        "expected updated SessionStart command"
    );
    assert!(
        !startup_commands.contains(&stale_session_start.as_str()),
        "expected stale SessionStart command to be removed"
    );

    Ok(())
}

#[test]
fn install_claude_reinstall_replaces_old_explicit_workspace_write_hook_commands() -> TestResult<()>
{
    let temp = tempfile::tempdir()?;
    let claude_dir = temp.path().join(".claude");
    std::fs::create_dir_all(&claude_dir)?;
    let config_path = temp.path().join(".claude.json");
    let settings_path = claude_dir.join("settings.json");
    let old_command = "/opt/old/mcp-repl";
    let new_command = "/opt/new/mcp-repl";
    let seeded_config = serde_json::json!({
        "mcpServers": {
            "r": {
                "command": old_command,
                "args": ["--sandbox", "workspace-write", "--interpreter", "r"]
            },
            "python": {
                "command": old_command,
                "args": ["--sandbox", "workspace-write", "--interpreter", "python"]
            }
        }
    });
    std::fs::write(&config_path, serde_json::to_string_pretty(&seeded_config)?)?;
    let stale_session_start =
        format!("{old_command} --sandbox workspace-write claude-hook session-start");
    let stale_session_end =
        format!("{old_command} --sandbox workspace-write claude-hook session-end");
    let seeded = serde_json::json!({
        "SessionStart": [
            {
                "matcher": "startup",
                "hooks": [
                    {"type": "command", "command": stale_session_start}
                ]
            }
        ],
        "SessionEnd": [
            {
                "matcher": "clear",
                "hooks": [
                    {"type": "command", "command": stale_session_end}
                ]
            }
        ]
    });
    std::fs::write(&settings_path, serde_json::to_string_pretty(&seeded)?)?;

    let exe = common::resolve_test_binary()?;
    let status = Command::new(exe)
        .arg("install")
        .arg("--client")
        .arg("claude")
        .arg("--command")
        .arg(new_command)
        .arg("--arg")
        .arg("--sandbox")
        .arg("--arg")
        .arg("workspace-write")
        .env("HOME", temp.path())
        .status()?;
    assert!(
        status.success(),
        "install --client claude failed with status {status}"
    );

    let settings_text = std::fs::read_to_string(settings_path)?;
    let settings_root: JsonValue = serde_json::from_str(&settings_text)?;
    let expected_session_start = installed_claude_hook_command(
        temp.path(),
        new_command,
        &["--sandbox", "workspace-write"],
        "session-start",
    );
    let expected_session_end = installed_claude_hook_command(
        temp.path(),
        new_command,
        &["--sandbox", "workspace-write"],
        "session-end",
    );

    let startup = claude_hook_entries(&settings_root, "SessionStart")
        .iter()
        .find(|entry| entry["matcher"].as_str() == Some("startup"))
        .expect("expected startup SessionStart matcher");
    let startup_commands: Vec<&str> = startup["hooks"]
        .as_array()
        .expect("expected SessionStart hooks array")
        .iter()
        .filter_map(|hook| hook["command"].as_str())
        .collect();
    assert_eq!(
        startup_commands,
        vec![expected_session_start.as_str()],
        "expected explicit workspace-write SessionStart hook to be replaced"
    );

    let clear = claude_hook_entries(&settings_root, "SessionEnd")
        .iter()
        .find(|entry| entry["matcher"].as_str() == Some("clear"))
        .expect("expected clear SessionEnd matcher");
    let clear_commands: Vec<&str> = clear["hooks"]
        .as_array()
        .expect("expected SessionEnd hooks array")
        .iter()
        .filter_map(|hook| hook["command"].as_str())
        .collect();
    assert_eq!(
        clear_commands,
        vec![expected_session_end.as_str()],
        "expected explicit workspace-write SessionEnd hook to be replaced"
    );

    Ok(())
}

#[test]
fn install_claude_reinstall_replaces_old_hook_commands_without_interpreter_args() -> TestResult<()>
{
    let temp = tempfile::tempdir()?;
    let claude_dir = temp.path().join(".claude");
    std::fs::create_dir_all(&claude_dir)?;
    let config_path = temp.path().join(".claude.json");
    let settings_path = claude_dir.join("settings.json");
    let old_command = "/opt/old/mcp-repl";
    let new_command = "/opt/new/mcp-repl";
    let seeded_config = serde_json::json!({
        "mcpServers": {
            "r": {
                "command": old_command,
                "args": ["--sandbox", "workspace-write"]
            }
        }
    });
    std::fs::write(&config_path, serde_json::to_string_pretty(&seeded_config)?)?;
    let stale_session_start =
        format!("{old_command} --sandbox workspace-write claude-hook session-start");
    let stale_session_end =
        format!("{old_command} --sandbox workspace-write claude-hook session-end");
    let seeded = serde_json::json!({
        "SessionStart": [
            {
                "matcher": "startup",
                "hooks": [
                    {"type": "command", "command": stale_session_start}
                ]
            }
        ],
        "SessionEnd": [
            {
                "matcher": "clear",
                "hooks": [
                    {"type": "command", "command": stale_session_end}
                ]
            }
        ]
    });
    std::fs::write(&settings_path, serde_json::to_string_pretty(&seeded)?)?;

    let exe = common::resolve_test_binary()?;
    let status = Command::new(&exe)
        .arg("install")
        .arg("--client")
        .arg("claude")
        .arg("--command")
        .arg(new_command)
        .arg("--arg")
        .arg("--sandbox")
        .arg("--arg")
        .arg("workspace-write")
        .env("HOME", temp.path())
        .status()?;
    assert!(
        status.success(),
        "install --client claude failed with status {status}"
    );

    let settings_text = std::fs::read_to_string(settings_path)?;
    let settings_root: JsonValue = serde_json::from_str(&settings_text)?;
    let expected_session_start = installed_claude_hook_command(
        temp.path(),
        new_command,
        &["--sandbox", "workspace-write"],
        "session-start",
    );
    let expected_session_end = installed_claude_hook_command(
        temp.path(),
        new_command,
        &["--sandbox", "workspace-write"],
        "session-end",
    );

    let startup = claude_hook_entries(&settings_root, "SessionStart")
        .iter()
        .find(|entry| entry["matcher"].as_str() == Some("startup"))
        .expect("expected startup SessionStart matcher");
    let startup_commands: Vec<&str> = startup["hooks"]
        .as_array()
        .expect("expected SessionStart hooks array")
        .iter()
        .filter_map(|hook| hook["command"].as_str())
        .collect();
    assert_eq!(
        startup_commands,
        vec![expected_session_start.as_str()],
        "expected stale SessionStart hook without interpreter args to be replaced"
    );

    let clear = claude_hook_entries(&settings_root, "SessionEnd")
        .iter()
        .find(|entry| entry["matcher"].as_str() == Some("clear"))
        .expect("expected clear SessionEnd matcher");
    let clear_commands: Vec<&str> = clear["hooks"]
        .as_array()
        .expect("expected SessionEnd hooks array")
        .iter()
        .filter_map(|hook| hook["command"].as_str())
        .collect();
    assert_eq!(
        clear_commands,
        vec![expected_session_end.as_str()],
        "expected stale SessionEnd hook without interpreter args to be replaced"
    );

    Ok(())
}

#[test]
fn install_claude_reinstall_deduplicates_matching_hook_entries() -> TestResult<()> {
    let temp = tempfile::tempdir()?;
    let claude_dir = temp.path().join(".claude");
    std::fs::create_dir_all(&claude_dir)?;
    let config_path = temp.path().join(".claude.json");
    let settings_path = claude_dir.join("settings.json");
    let old_command = "/opt/old/mcp-repl";
    let new_command = "/opt/new/mcp-repl";
    let seeded_config = serde_json::json!({
        "mcpServers": {
            "r": {
                "command": old_command,
                "args": ["--interpreter", "r"]
            },
            "python": {
                "command": old_command,
                "args": ["--interpreter", "python"]
            }
        }
    });
    std::fs::write(&config_path, serde_json::to_string_pretty(&seeded_config)?)?;
    let stale_session_start = format!("{old_command} claude-hook session-start");
    let stale_session_end = format!("{old_command} claude-hook session-end");
    let seeded = serde_json::json!({
        "SessionStart": [
            {
                "matcher": "startup",
                "hooks": [
                    {"type": "command", "command": stale_session_start},
                    {"type": "command", "command": "echo keep-start-a"}
                ]
            },
            {
                "matcher": "startup",
                "hooks": [
                    {"type": "command", "command": stale_session_start},
                    {"type": "command", "command": "echo keep-start-b"}
                ]
            }
        ],
        "SessionEnd": [
            {
                "matcher": "clear",
                "hooks": [
                    {"type": "command", "command": stale_session_end},
                    {"type": "command", "command": "echo keep-end-a"}
                ]
            },
            {
                "matcher": "clear",
                "hooks": [
                    {"type": "command", "command": stale_session_end},
                    {"type": "command", "command": "echo keep-end-b"}
                ]
            }
        ]
    });
    std::fs::write(&settings_path, serde_json::to_string_pretty(&seeded)?)?;

    let exe = common::resolve_test_binary()?;
    let status = Command::new(exe)
        .arg("install")
        .arg("--client")
        .arg("claude")
        .arg("--command")
        .arg(new_command)
        .env("HOME", temp.path())
        .status()?;
    assert!(
        status.success(),
        "install --client claude failed with status {status}"
    );

    let settings_text = std::fs::read_to_string(settings_path)?;
    let settings_root: JsonValue = serde_json::from_str(&settings_text)?;
    let expected_session_start =
        installed_claude_hook_command(temp.path(), new_command, &[], "session-start");
    let expected_session_end =
        installed_claude_hook_command(temp.path(), new_command, &[], "session-end");

    let startup_entries: Vec<&JsonValue> = claude_hook_entries(&settings_root, "SessionStart")
        .iter()
        .filter(|entry| entry["matcher"].as_str() == Some("startup"))
        .collect();
    assert_eq!(
        startup_entries.len(),
        1,
        "expected duplicate startup entries to be merged"
    );
    let startup_commands: Vec<&str> = startup_entries[0]["hooks"]
        .as_array()
        .expect("expected startup hooks array")
        .iter()
        .filter_map(|hook| hook["command"].as_str())
        .collect();
    assert!(
        startup_commands.contains(&expected_session_start.as_str()),
        "expected updated SessionStart command"
    );
    assert!(
        startup_commands.contains(&"echo keep-start-a"),
        "expected first duplicate's unrelated hook to remain"
    );
    assert!(
        startup_commands.contains(&"echo keep-start-b"),
        "expected second duplicate's unrelated hook to remain"
    );
    assert!(
        !startup_commands.contains(&stale_session_start.as_str()),
        "expected stale SessionStart command to be removed from merged entry"
    );

    let clear_entries: Vec<&JsonValue> = claude_hook_entries(&settings_root, "SessionEnd")
        .iter()
        .filter(|entry| entry["matcher"].as_str() == Some("clear"))
        .collect();
    assert_eq!(
        clear_entries.len(),
        1,
        "expected duplicate clear entries to be merged"
    );
    let clear_commands: Vec<&str> = clear_entries[0]["hooks"]
        .as_array()
        .expect("expected clear hooks array")
        .iter()
        .filter_map(|hook| hook["command"].as_str())
        .collect();
    assert!(
        clear_commands.contains(&expected_session_end.as_str()),
        "expected updated SessionEnd command"
    );
    assert!(
        clear_commands.contains(&"echo keep-end-a"),
        "expected first duplicate's unrelated SessionEnd hook to remain"
    );
    assert!(
        clear_commands.contains(&"echo keep-end-b"),
        "expected second duplicate's unrelated SessionEnd hook to remain"
    );
    assert!(
        !clear_commands.contains(&stale_session_end.as_str()),
        "expected stale SessionEnd command to be removed from merged entry"
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
