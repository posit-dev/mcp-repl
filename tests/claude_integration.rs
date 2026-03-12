mod common;

use common::TestResult;
use serde::Serialize;
use serde_json::{Map as JsonMap, Value as JsonValue};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const SNAPSHOT_NAME: &str = "claude_live_integration";
const CLAUDE_TIMEOUT: Duration = Duration::from_secs(120);
const CLAUDE_MODEL: &str = "haiku";
const CLAUDE_PERMISSION_MODE: &str = "dontAsk";
const TOOL_INPUT: &str = "cat(\"CLAUDE_MCP_OK\\n\")";
const FINAL_RESULT: &str = "DONE";
const CLAUDE_PROMPT: &str = "Use the mcp__r__repl tool exactly once. Send this exact R code: cat(\"CLAUDE_MCP_OK\\n\") Then answer with exactly DONE, with no punctuation or extra text.\n";

#[derive(Debug)]
struct StagedClaudeEnv {
    _temp_dir: tempfile::TempDir,
    workspace: PathBuf,
    home: PathBuf,
    settings_path: PathBuf,
    mcp_config_path: PathBuf,
    child_env: Vec<(String, String)>,
}

#[derive(Debug, Serialize)]
struct ClaudeSnapshot {
    command: String,
    prompt: String,
    init: ClaudeInitSnapshot,
    tool_call: ClaudeToolCallSnapshot,
    tool_result: String,
    final_text: String,
    result: ClaudeResultSnapshot,
}

#[derive(Debug, Serialize)]
struct ClaudeInitSnapshot {
    cwd: String,
    tools: Vec<String>,
    mcp_servers: Vec<ClaudeMcpServerSnapshot>,
    model: String,
    permission_mode: String,
}

#[derive(Debug, Serialize)]
struct ClaudeMcpServerSnapshot {
    name: String,
    status: String,
}

#[derive(Debug, Serialize)]
struct ClaudeToolCallSnapshot {
    name: String,
    input: String,
}

#[derive(Debug, Serialize)]
struct ClaudeResultSnapshot {
    subtype: String,
    is_error: bool,
    result: String,
    stop_reason: String,
}

#[test]
fn claude_live_integration() -> TestResult<()> {
    let Some(snapshot) = run_claude_integration_snapshot()? else {
        return Ok(());
    };

    let rendered = serde_json::to_string_pretty(&snapshot)?;
    insta::assert_snapshot!(SNAPSHOT_NAME, rendered);

    let transcript = render_transcript(&snapshot);
    insta::with_settings!({ snapshot_suffix => "transcript" }, {
        insta::assert_snapshot!(SNAPSHOT_NAME, transcript);
    });

    Ok(())
}

fn run_claude_integration_snapshot() -> TestResult<Option<ClaudeSnapshot>> {
    if !claude_available() {
        eprintln!("claude not found on PATH; skipping");
        return Ok(None);
    }

    let mcp_console = resolve_mcp_console_path()?;
    let Some(staged) = stage_claude_env(&mcp_console)? else {
        return Ok(None);
    };

    let mut cmd = Command::new("claude");
    cmd.env_clear();
    if let Some(path) = env::var_os("PATH") {
        cmd.env("PATH", path);
    }
    if let Some(tmpdir) = env::var_os("TMPDIR") {
        cmd.env("TMPDIR", tmpdir);
    }
    cmd.env("HOME", &staged.home);
    for (key, value) in &staged.child_env {
        cmd.env(key, value);
    }

    cmd.current_dir(&staged.workspace);
    cmd.arg("--disable-slash-commands");
    cmd.arg("--setting-sources");
    cmd.arg("local");
    cmd.arg("--settings");
    cmd.arg(&staged.settings_path);
    cmd.arg("--mcp-config");
    cmd.arg(&staged.mcp_config_path);
    cmd.arg("--strict-mcp-config");
    cmd.arg("-p");
    cmd.arg("--verbose");
    cmd.arg("--model");
    cmd.arg(CLAUDE_MODEL);
    cmd.arg("--no-session-persistence");
    cmd.arg("--permission-mode");
    cmd.arg(CLAUDE_PERMISSION_MODE);
    cmd.arg("--output-format");
    cmd.arg("json");
    cmd.arg("--tools");
    cmd.arg("");

    let output = run_command_with_timeout(cmd, CLAUDE_PROMPT, CLAUDE_TIMEOUT)?;
    let stdout = String::from_utf8(output.stdout)
        .map_err(|err| format!("claude stdout was not valid UTF-8: {err}"))?;
    let stderr = String::from_utf8(output.stderr)
        .map_err(|err| format!("claude stderr was not valid UTF-8: {err}"))?;

    if !output.status.success() {
        return Err(format!(
            "claude run failed with status {status}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            status = output.status
        )
        .into());
    }

    parse_snapshot(stdout.trim(), &staged.workspace)
}

fn claude_available() -> bool {
    Command::new("claude")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn resolve_mcp_console_path() -> TestResult<PathBuf> {
    if let Ok(path) = env::var("CARGO_BIN_EXE_mcp-repl") {
        return Ok(PathBuf::from(path));
    }
    if let Ok(path) = env::var("CARGO_BIN_EXE_mcp-console") {
        return Ok(PathBuf::from(path));
    }

    let mut path = env::current_exe()?;
    path.pop();
    path.pop();
    for candidate in ["mcp-repl", "mcp-console"] {
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

fn run_command_with_timeout(
    mut cmd: Command,
    stdin: &str,
    timeout: Duration,
) -> TestResult<Output> {
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = cmd.spawn()?;

    if let Some(mut child_stdin) = child.stdin.take() {
        child_stdin.write_all(stdin.as_bytes())?;
    }

    let mut stdout_reader = child
        .stdout
        .take()
        .ok_or_else(|| "failed to capture claude stdout".to_string())?;
    let mut stderr_reader = child
        .stderr
        .take()
        .ok_or_else(|| "failed to capture claude stderr".to_string())?;

    let (stdout_tx, stdout_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_reader.read_to_end(&mut buf);
        let _ = stdout_tx.send(buf);
    });
    let (stderr_tx, stderr_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_reader.read_to_end(&mut buf);
        let _ = stderr_tx.send(buf);
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            #[cfg(unix)]
            unsafe {
                libc::killpg(child.id() as i32, libc::SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "timed out waiting for claude to finish after {}s",
                timeout.as_secs()
            )
            .into());
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    let stdout = stdout_rx
        .recv_timeout(Duration::from_secs(1))
        .map_err(|_| "timed out collecting claude stdout".to_string())?;
    let stderr = stderr_rx
        .recv_timeout(Duration::from_secs(1))
        .map_err(|_| "timed out collecting claude stderr".to_string())?;

    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn stage_claude_env(mcp_console: &Path) -> TestResult<Option<StagedClaudeEnv>> {
    let temp_dir = tempfile::tempdir()?;
    let workspace = temp_dir.path().join("workspace");
    let home = temp_dir.path().join("home");
    fs::create_dir_all(&workspace)?;
    fs::create_dir_all(&home)?;

    let settings_path = temp_dir.path().join("settings.json");
    let mcp_config_path = temp_dir.path().join("mcp.json");

    let mut child_env = Vec::new();
    let mut settings_env = JsonMap::new();

    if let Some(api_key) = nonempty_env("ANTHROPIC_API_KEY") {
        child_env.push(("ANTHROPIC_API_KEY".to_string(), api_key));
    } else {
        let host_settings_env = load_host_claude_settings_env()?;
        let Some(bedrock_env) = stage_bedrock_env(&home, &host_settings_env)? else {
            eprintln!("no supported Claude auth staging available; skipping");
            return Ok(None);
        };
        for (key, value) in bedrock_env {
            child_env.push((key.clone(), value.clone()));
            settings_env.insert(key, JsonValue::String(value));
        }
    }

    let settings_root = JsonValue::Object(JsonMap::from_iter([
        (
            "enabledPlugins".to_string(),
            JsonValue::Object(JsonMap::new()),
        ),
        (
            "permissions".to_string(),
            JsonValue::Object(JsonMap::from_iter([(
                "allow".to_string(),
                JsonValue::Array(vec![JsonValue::String("mcp__r__*".to_string())]),
            )])),
        ),
        ("env".to_string(), JsonValue::Object(settings_env)),
    ]));
    fs::write(
        &settings_path,
        serde_json::to_string_pretty(&settings_root)?,
    )?;

    let mcp_root = JsonValue::Object(JsonMap::from_iter([(
        "mcpServers".to_string(),
        JsonValue::Object(JsonMap::from_iter([(
            "r".to_string(),
            JsonValue::Object(JsonMap::from_iter([
                (
                    "command".to_string(),
                    JsonValue::String(mcp_console.display().to_string()),
                ),
                (
                    "args".to_string(),
                    JsonValue::Array(vec![
                        JsonValue::String("--sandbox".to_string()),
                        JsonValue::String("workspace-write".to_string()),
                        JsonValue::String("--interpreter".to_string()),
                        JsonValue::String("r".to_string()),
                    ]),
                ),
            ])),
        )])),
    )]));
    fs::write(&mcp_config_path, serde_json::to_string_pretty(&mcp_root)?)?;

    Ok(Some(StagedClaudeEnv {
        _temp_dir: temp_dir,
        workspace,
        home,
        settings_path,
        mcp_config_path,
        child_env,
    }))
}

fn stage_bedrock_env(
    temp_home: &Path,
    host_settings_env: &std::collections::BTreeMap<String, String>,
) -> TestResult<Option<std::collections::BTreeMap<String, String>>> {
    if !bedrock_enabled(host_settings_env) {
        return Ok(None);
    }

    let aws_source = host_home_dir()?.join(".aws");
    if !aws_source.is_dir() {
        eprintln!("Bedrock auth selected but ~/.aws is unavailable; skipping");
        return Ok(None);
    }

    copy_dir_all(&aws_source, &temp_home.join(".aws"))?;

    let mut env_vars = std::collections::BTreeMap::new();
    env_vars.insert("CLAUDE_CODE_USE_BEDROCK".to_string(), "1".to_string());
    for key in ["AWS_PROFILE", "AWS_REGION", "ANTHROPIC_DEFAULT_HAIKU_MODEL"] {
        if let Some(value) = current_or_host_env(key, host_settings_env) {
            env_vars.insert(key.to_string(), value);
        }
    }

    Ok(Some(env_vars))
}

fn load_host_claude_settings_env() -> TestResult<std::collections::BTreeMap<String, String>> {
    let settings_path = host_home_dir()?.join(".claude/settings.json");
    if !settings_path.is_file() {
        return Ok(std::collections::BTreeMap::new());
    }

    let raw = match fs::read_to_string(&settings_path) {
        Ok(raw) => raw,
        Err(err) => {
            eprintln!(
                "failed to read host Claude settings {}; skipping host Claude settings: {err}",
                settings_path.display()
            );
            return Ok(std::collections::BTreeMap::new());
        }
    };
    let root: JsonValue = match serde_json::from_str(&raw) {
        Ok(root) => root,
        Err(err) => {
            eprintln!(
                "failed to parse host Claude settings {}; skipping host Claude settings: {err}",
                settings_path.display()
            );
            return Ok(std::collections::BTreeMap::new());
        }
    };

    let env_vars = root
        .get("env")
        .and_then(JsonValue::as_object)
        .map(|env| {
            env.iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|value| (key.clone(), value.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(env_vars)
}

fn parse_snapshot(stdout: &str, workspace: &Path) -> TestResult<Option<ClaudeSnapshot>> {
    let events: Vec<JsonValue> = serde_json::from_str(stdout)
        .map_err(|err| format!("failed to parse Claude JSON output: {err}\nstdout:\n{stdout}"))?;

    let init_event = events
        .iter()
        .find(|event| event.get("type").and_then(JsonValue::as_str) == Some("system"))
        .ok_or_else(|| "missing Claude init event".to_string())?;

    let mut tools = init_event
        .get("tools")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| "Claude init event missing tools".to_string())?
        .iter()
        .filter_map(JsonValue::as_str)
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    tools.sort();

    let mut mcp_servers = init_event
        .get("mcp_servers")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| "Claude init event missing mcp_servers".to_string())?
        .iter()
        .map(|server| ClaudeMcpServerSnapshot {
            name: server
                .get("name")
                .and_then(JsonValue::as_str)
                .unwrap_or("<unknown>")
                .to_string(),
            status: server
                .get("status")
                .and_then(JsonValue::as_str)
                .unwrap_or("<unknown>")
                .to_string(),
        })
        .collect::<Vec<_>>();
    mcp_servers.sort_by(|left, right| left.name.cmp(&right.name));

    let init = ClaudeInitSnapshot {
        cwd: normalize_workspace_path(
            init_event
                .get("cwd")
                .and_then(JsonValue::as_str)
                .unwrap_or(""),
            workspace,
        ),
        tools,
        mcp_servers,
        model: CLAUDE_MODEL.to_string(),
        permission_mode: init_event
            .get("permissionMode")
            .and_then(JsonValue::as_str)
            .unwrap_or(CLAUDE_PERMISSION_MODE)
            .to_string(),
    };

    let tool_call = extract_tool_call(&events)?;
    let tool_result = extract_tool_result(&events)?;
    let final_text = normalize_done_text(&extract_final_text(&events)?);
    let result = extract_result(&events)?;

    if tool_call.name != "mcp__r__repl" {
        return Err(format!("unexpected Claude tool call: {}", tool_call.name).into());
    }
    if tool_call.input != TOOL_INPUT {
        return Err(format!("unexpected Claude tool input: {}", tool_call.input).into());
    }
    if tool_result != "CLAUDE_MCP_OK\n" {
        return Err(format!("unexpected Claude tool result: {tool_result:?}").into());
    }
    if final_text != FINAL_RESULT {
        return Err(format!("unexpected Claude final text: {final_text:?}").into());
    }
    if result.is_error || result.result != FINAL_RESULT {
        return Err(format!("unexpected Claude result event: {result:?}").into());
    }

    Ok(Some(ClaudeSnapshot {
        command: snapshot_command(),
        prompt: CLAUDE_PROMPT.trim_end().to_string(),
        init,
        tool_call,
        tool_result,
        final_text,
        result,
    }))
}

fn extract_tool_call(events: &[JsonValue]) -> TestResult<ClaudeToolCallSnapshot> {
    let mut calls = Vec::new();

    for event in events {
        let Some(message) = event.get("message").and_then(JsonValue::as_object) else {
            continue;
        };
        let Some(contents) = message.get("content").and_then(JsonValue::as_array) else {
            continue;
        };
        for content in contents {
            if content.get("type").and_then(JsonValue::as_str) != Some("tool_use") {
                continue;
            }
            let name = content
                .get("name")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| "Claude tool_use missing name".to_string())?
                .to_string();
            let input = content
                .get("input")
                .and_then(JsonValue::as_object)
                .and_then(|input| input.get("input"))
                .and_then(JsonValue::as_str)
                .ok_or_else(|| "Claude tool_use missing input.input".to_string())?
                .to_string();
            calls.push(ClaudeToolCallSnapshot { name, input });
        }
    }

    match calls.len() {
        1 => Ok(calls.remove(0)),
        0 => Err("Claude output did not contain a tool call".into()),
        n => Err(format!("expected exactly one Claude tool call, found {n}").into()),
    }
}

fn extract_tool_result(events: &[JsonValue]) -> TestResult<String> {
    for event in events {
        if let Some(contents) = event
            .get("tool_use_result")
            .and_then(JsonValue::as_array)
            .or_else(|| {
                event
                    .get("message")
                    .and_then(JsonValue::as_object)
                    .and_then(|message| message.get("content"))
                    .and_then(JsonValue::as_array)
                    .and_then(|contents| {
                        contents.iter().find_map(|content| {
                            content
                                .get("content")
                                .and_then(JsonValue::as_array)
                                .filter(|_| {
                                    content.get("type").and_then(JsonValue::as_str)
                                        == Some("tool_result")
                                })
                        })
                    })
            })
        {
            let text = contents
                .iter()
                .filter_map(|item| item.get("text").and_then(JsonValue::as_str))
                .filter(|text| *text != "> ")
                .collect::<String>();
            if !text.is_empty() {
                return Ok(text);
            }
        }
    }

    Err("Claude output did not contain a tool result".into())
}

fn extract_final_text(events: &[JsonValue]) -> TestResult<String> {
    let mut final_text = None;

    for event in events {
        let Some(message) = event.get("message").and_then(JsonValue::as_object) else {
            continue;
        };
        let Some(contents) = message.get("content").and_then(JsonValue::as_array) else {
            continue;
        };
        let text = contents
            .iter()
            .filter_map(|content| {
                (content.get("type").and_then(JsonValue::as_str) == Some("text"))
                    .then(|| content.get("text").and_then(JsonValue::as_str))
                    .flatten()
            })
            .collect::<String>();
        if !text.is_empty() {
            final_text = Some(text);
        }
    }

    final_text.ok_or_else(|| "Claude output did not contain a final text message".into())
}

fn extract_result(events: &[JsonValue]) -> TestResult<ClaudeResultSnapshot> {
    let result_event = events
        .iter()
        .find(|event| event.get("type").and_then(JsonValue::as_str) == Some("result"))
        .ok_or_else(|| "Claude output missing result event".to_string())?;

    if result_event
        .get("permission_denials")
        .and_then(JsonValue::as_array)
        .is_some_and(|denials| !denials.is_empty())
    {
        return Err("Claude output contained permission denials".into());
    }

    Ok(ClaudeResultSnapshot {
        subtype: result_event
            .get("subtype")
            .and_then(JsonValue::as_str)
            .unwrap_or("<unknown>")
            .to_string(),
        is_error: result_event
            .get("is_error")
            .and_then(JsonValue::as_bool)
            .unwrap_or(true),
        result: normalize_done_text(
            result_event
                .get("result")
                .and_then(JsonValue::as_str)
                .unwrap_or(""),
        ),
        stop_reason: result_event
            .get("stop_reason")
            .and_then(JsonValue::as_str)
            .unwrap_or("<unknown>")
            .to_string(),
    })
}

fn render_transcript(snapshot: &ClaudeSnapshot) -> String {
    let mut out = String::new();
    out.push_str(&snapshot.command);
    out.push('\n');
    out.push_str("stdin:\n");
    for line in snapshot.prompt.lines() {
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
    out.push_str("init:\n");
    out.push_str(&format!("cwd: {}\n", snapshot.init.cwd));
    out.push_str(&format!("tools: {}\n", snapshot.init.tools.join(", ")));
    for server in &snapshot.init.mcp_servers {
        out.push_str(&format!(
            "mcp_server: {} ({})\n",
            server.name, server.status
        ));
    }
    out.push_str(&format!("model: {}\n", snapshot.init.model));
    out.push_str(&format!(
        "permission_mode: {}\n",
        snapshot.init.permission_mode
    ));
    out.push('\n');
    out.push_str(&format!("1) {}\n", snapshot.tool_call.name));
    for line in snapshot.tool_call.input.lines() {
        out.push_str(&format!(">>> {line}\n"));
    }
    for line in snapshot.tool_result.trim_end_matches('\n').lines() {
        out.push_str(&format!("<<< {line}\n"));
    }
    out.push('\n');
    out.push_str(&format!("2) {}\n", snapshot.final_text));
    out.push_str(&format!(
        "result: {} ({})\n",
        snapshot.result.result, snapshot.result.stop_reason
    ));
    out.trim_end().to_string()
}

fn snapshot_command() -> String {
    "$ HOME=<HOME> claude --disable-slash-commands --setting-sources local --settings <SETTINGS_JSON> --mcp-config <MCP_JSON> --strict-mcp-config -p --verbose --model haiku --no-session-persistence --permission-mode dontAsk --output-format json --tools \"\"".to_string()
}

fn normalize_workspace_path(path: &str, workspace: &Path) -> String {
    let workspace_display = workspace.display().to_string();
    let workspace_private = format!("/private{workspace_display}");
    path.replace(&workspace_private, "<WORKSPACE>")
        .replace(&workspace_display, "<WORKSPACE>")
}

fn bedrock_enabled(host_settings_env: &std::collections::BTreeMap<String, String>) -> bool {
    current_or_host_env("CLAUDE_CODE_USE_BEDROCK", host_settings_env)
        .is_some_and(|value| is_truthy(&value))
}

fn current_or_host_env(
    key: &str,
    host_settings_env: &std::collections::BTreeMap<String, String>,
) -> Option<String> {
    nonempty_env(key).or_else(|| host_settings_env.get(key).cloned())
}

fn nonempty_env(key: &str) -> Option<String> {
    env::var(key).ok().filter(|value| !value.is_empty())
}

fn is_truthy(value: &str) -> bool {
    !value.is_empty() && !matches!(value, "0" | "false" | "False" | "FALSE")
}

fn normalize_done_text(text: &str) -> String {
    match text.trim() {
        "DONE." => FINAL_RESULT.to_string(),
        other => other.to_string(),
    }
}

fn host_home_dir() -> TestResult<PathBuf> {
    resolve_home_dir_from_env(
        env::var_os("HOME"),
        env::var_os("USERPROFILE"),
        env::var_os("HOMEDRIVE"),
        env::var_os("HOMEPATH"),
    )
    .ok_or_else(|| {
        "cannot determine home directory (expected HOME, USERPROFILE, or HOMEDRIVE+HOMEPATH)".into()
    })
}

fn resolve_home_dir_from_env(
    home: Option<OsString>,
    userprofile: Option<OsString>,
    homedrive: Option<OsString>,
    homepath: Option<OsString>,
) -> Option<PathBuf> {
    if let Some(home) = home.filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(home));
    }
    if let Some(userprofile) = userprofile.filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(userprofile));
    }

    let homedrive = homedrive.filter(|value| !value.is_empty())?;
    let homepath = homepath.filter(|value| !value.is_empty())?;
    let absolute_homepath = PathBuf::from(&homepath);
    if absolute_homepath.is_absolute() {
        return Some(absolute_homepath);
    }

    let needs_separator = !matches!(
        homepath.to_str(),
        Some(value) if value.starts_with('\\') || value.starts_with('/')
    );
    let mut combined = homedrive;
    if needs_separator {
        combined.push("\\");
    }
    combined.push(homepath);
    Some(PathBuf::from(combined))
}

fn copy_dir_all(source: &Path, dest: &Path) -> TestResult<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let source_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_all(&source_path, &dest_path)?;
        } else if file_type.is_file() {
            fs::copy(&source_path, &dest_path)?;
        }
    }
    Ok(())
}
