use std::collections::BTreeSet;
use std::env;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map as JsonMap, Value as JsonValue};
use toml_edit::{Array, DocumentMut, Item, Table, value};

const CODEX_TOOL_TIMEOUT_SECS: i64 = 1_800;
const CODEX_TOOL_TIMEOUT_COMMENT: &str =
    "\n# mcp-repl handles the primary timeout; this higher Codex timeout is only an outer guard.\n";
const CODEX_SANDBOX_INHERIT_COMMENT: &str = "\n# --sandbox inherit: use sandbox policy updates sent by Codex for this session.\n# If no update is sent, mcp-repl exits with an error.\n";
pub const DEFAULT_R_SERVER_NAME: &str = "r";
pub const DEFAULT_PYTHON_SERVER_NAME: &str = "python";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallInterpreter {
    R,
    Python,
}

impl InstallInterpreter {
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_lowercase().as_str() {
            "r" => Ok(Self::R),
            "python" => Ok(Self::Python),
            _ => Err(format!("invalid interpreter: {raw} (expected r|python)")),
        }
    }

    fn cli_value(self) -> &'static str {
        match self {
            Self::R => "r",
            Self::Python => "python",
        }
    }

    fn default_server_name(self) -> &'static str {
        match self {
            Self::R => DEFAULT_R_SERVER_NAME,
            Self::Python => DEFAULT_PYTHON_SERVER_NAME,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum InstallTarget {
    Codex,
    Claude,
}

impl InstallTarget {
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "codex" => Ok(Self::Codex),
            "claude" => Ok(Self::Claude),
            _ => Err(format!(
                "invalid install target: {raw} (expected codex|claude)"
            )),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }
}

#[derive(Debug, Clone)]
pub struct InstallOptions {
    pub targets: Vec<InstallTarget>,
    pub interpreters: Vec<InstallInterpreter>,
    pub args: Vec<String>,
}

pub fn run(options: InstallOptions) -> Result<(), Box<dyn std::error::Error>> {
    if has_interpreter_config_arg(&options.args) {
        return Err(
            "install does not accept interpreter selection via --arg; use --interpreter r|python instead"
                .into(),
        );
    }
    let command = default_command();
    let targets = resolve_target_roots(&options.targets)?;
    let codex_args = codex_install_args(&options.args);
    let claude_args = claude_install_args(&options.args);
    let interpreters = effective_interpreters(&options.interpreters);
    let server_specs: Vec<(String, InstallInterpreter)> = interpreters
        .iter()
        .map(|interpreter| (interpreter.default_server_name().to_string(), *interpreter))
        .collect();

    for (target, root) in targets {
        match target {
            InstallTarget::Codex => {
                let path = root.join("config.toml");
                for (server_name, interpreter) in &server_specs {
                    let server_args = with_interpreter_arg(&codex_args, *interpreter);
                    upsert_codex_mcp_server(&path, server_name, &command, &server_args)?;
                }
                println!("Updated codex MCP config: {}", path.display());
            }
            InstallTarget::Claude => {
                let config_path = root.join(".claude.json");
                let settings_dir = root.join(".claude");
                let settings_path = settings_dir.join("settings.json");
                for (server_name, interpreter) in &server_specs {
                    let server_args = with_interpreter_arg(&claude_args, *interpreter);
                    upsert_claude_mcp_server(&config_path, server_name, &command, &server_args)?;
                    // Ensure ~/.claude directory exists for settings.json
                    if !settings_dir.is_dir() {
                        fs::create_dir_all(&settings_dir)?;
                    }
                    upsert_claude_settings_permission(&settings_path, server_name)?;
                }
                println!("Updated claude MCP config: {}", config_path.display());
                println!("Updated claude permissions: {}", settings_path.display());
            }
        }
    }

    Ok(())
}

fn default_command() -> String {
    env::current_exe()
        .ok()
        .and_then(|path| path.into_os_string().into_string().ok())
        .unwrap_or_else(|| "mcp-repl".to_string())
}

fn resolve_target_roots(
    requested: &[InstallTarget],
) -> Result<Vec<(InstallTarget, PathBuf)>, Box<dyn std::error::Error>> {
    let mut targets: BTreeSet<InstallTarget> = BTreeSet::new();
    if requested.is_empty() {
        let codex_root = default_codex_home()?;
        if codex_root.is_dir() {
            targets.insert(InstallTarget::Codex);
        }
        // For Claude, check if home directory exists (we write directly to ~/.claude.json)
        let home = home_dir()?;
        if home.is_dir() {
            targets.insert(InstallTarget::Claude);
        }
        if targets.is_empty() {
            return Err(
                "no existing agent home found (expected ~/.codex and/or home directory for ~/.claude.json)"
                    .into(),
            );
        }
    } else {
        targets.extend(requested.iter().copied());
    }

    let mut resolved = Vec::with_capacity(targets.len());
    for target in targets {
        let root = match target {
            InstallTarget::Codex => {
                let root = default_codex_home()?;
                if !root.is_dir() {
                    return Err(format!(
                        "{} home does not exist: {} (not creating new directories)",
                        target.label(),
                        root.display()
                    )
                    .into());
                }
                root
            }
            InstallTarget::Claude => {
                // For Claude, we just need the home directory to exist
                let home = home_dir()?;
                if !home.is_dir() {
                    return Err(format!("home directory does not exist: {}", home.display()).into());
                }
                home
            }
        };
        resolved.push((target, root));
    }

    Ok(resolved)
}

fn default_codex_home() -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Some(path) = env::var_os("CODEX_HOME") {
        return Ok(PathBuf::from(path));
    }
    Ok(home_dir()?.join(".codex"))
}

fn home_dir() -> Result<PathBuf, Box<dyn std::error::Error>> {
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
    let needs_separator = !matches!(homepath.to_str(), Some(value) if value.starts_with('\\') || value.starts_with('/'));
    let mut combined = homedrive;
    if needs_separator {
        combined.push("\\");
    }
    combined.push(homepath);
    Some(PathBuf::from(combined))
}

fn has_sandbox_config_arg(args: &[String]) -> bool {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if matches!(
            arg.as_str(),
            "--sandbox" | "--add-writable-root" | "--add-writeable-root" | "--add-allowed-domain"
        ) || arg.starts_with("--sandbox=")
            || arg.starts_with("--add-writable-root=")
            || arg.starts_with("--add-writeable-root=")
            || arg.starts_with("--add-allowed-domain=")
        {
            return true;
        }
        if arg == "--config" {
            if iter
                .next()
                .is_some_and(|value| is_sandbox_config_override(value))
            {
                return true;
            }
            continue;
        }
        if let Some(value) = arg.strip_prefix("--config=")
            && is_sandbox_config_override(value)
        {
            return true;
        }
    }
    false
}

fn is_sandbox_config_override(raw: &str) -> bool {
    let Some((key, _value)) = raw.split_once('=') else {
        return false;
    };
    matches!(
        key.trim(),
        "sandbox_mode"
            | "sandbox_workspace_write.network_access"
            | "sandbox_workspace_write.writable_roots"
            | "sandbox_workspace_write.exclude_tmpdir_env_var"
            | "sandbox_workspace_write.exclude_slash_tmp"
            | "permissions.network.allowed_domains"
            | "permissions.network.denied_domains"
            | "permissions.network.allow_local_binding"
            | "features.use_linux_sandbox_bwrap"
            | "use_linux_sandbox_bwrap"
    )
}

fn has_interpreter_config_arg(args: &[String]) -> bool {
    args.iter().any(|arg| {
        matches!(arg.as_str(), "--interpreter" | "--backend")
            || arg.starts_with("--interpreter=")
            || arg.starts_with("--backend=")
    })
}

fn has_interpreter_value(args: &[String], target: &str) -> bool {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--interpreter" {
            if iter.next().is_some_and(|value| value == target) {
                return true;
            }
            continue;
        }
        if let Some(value) = arg.strip_prefix("--interpreter=")
            && value == target
        {
            return true;
        }
    }
    false
}

fn with_interpreter_arg(base_args: &[String], interpreter: InstallInterpreter) -> Vec<String> {
    if has_interpreter_value(base_args, interpreter.cli_value()) {
        return base_args.to_vec();
    }
    let mut args = base_args.to_vec();
    args.push("--interpreter".to_string());
    args.push(interpreter.cli_value().to_string());
    args
}

fn effective_interpreters(configured: &[InstallInterpreter]) -> Vec<InstallInterpreter> {
    if configured.is_empty() {
        return vec![InstallInterpreter::R, InstallInterpreter::Python];
    }
    let mut out = Vec::new();
    for interpreter in configured {
        if !out.contains(interpreter) {
            out.push(*interpreter);
        }
    }
    out
}

fn codex_install_args(base_args: &[String]) -> Vec<String> {
    let mut args = base_args.to_vec();
    if !has_sandbox_config_arg(base_args) {
        args.push("--sandbox".to_string());
        args.push("inherit".to_string());
    }
    args
}

fn claude_install_args(base_args: &[String]) -> Vec<String> {
    let mut args = base_args.to_vec();
    if !has_sandbox_config_arg(base_args) {
        args.push("--sandbox".to_string());
        args.push("workspace-write".to_string());
    }
    args
}

fn contains_sandbox_state_value(args: &[String], target: &str) -> bool {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--sandbox" {
            if iter.next().is_some_and(|value| value == target) {
                return true;
            }
            continue;
        }
        if arg
            .strip_prefix("--sandbox=")
            .is_some_and(|value| value == target)
        {
            return true;
        }
    }
    false
}

fn upsert_codex_mcp_server(
    config_path: &Path,
    server_name: &str,
    command: &str,
    args: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut doc = if config_path.is_file() {
        let raw = fs::read_to_string(config_path)?;
        raw.parse::<DocumentMut>()
            .map_err(|err| format!("failed to parse TOML {}: {err}", config_path.display()))?
    } else {
        DocumentMut::new()
    };

    if doc.get("mcp_servers").is_none() {
        doc["mcp_servers"] = Item::Table(Table::new());
    }
    if !doc["mcp_servers"].is_table() {
        return Err("`mcp_servers` must be a TOML table".into());
    }
    normalize_codex_server_item(&mut doc, server_name)?;

    doc["mcp_servers"][server_name]["command"] = value(command);
    doc["mcp_servers"][server_name]["tool_timeout_sec"] = value(CODEX_TOOL_TIMEOUT_SECS);

    let mut toml_args = Array::default();
    for arg in args {
        toml_args.push(arg.as_str());
    }
    format_toml_array_multiline(&mut toml_args);
    doc["mcp_servers"][server_name]["args"] = Item::Value(toml_args.into());
    if let Some(server_table) = doc["mcp_servers"][server_name].as_table_mut()
        && let Some(mut timeout_key) = server_table.key_mut("tool_timeout_sec")
    {
        timeout_key
            .leaf_decor_mut()
            .set_prefix(CODEX_TOOL_TIMEOUT_COMMENT);
    }
    let args_prefix = if contains_sandbox_state_value(args, "inherit") {
        CODEX_SANDBOX_INHERIT_COMMENT
    } else {
        ""
    };
    if let Some(server_table) = doc["mcp_servers"][server_name].as_table_mut()
        && let Some(mut args_key) = server_table.key_mut("args")
    {
        args_key.leaf_decor_mut().set_prefix(args_prefix);
    }

    atomic_write(config_path, &doc.to_string())?;
    Ok(())
}

fn format_toml_array_multiline(array: &mut Array) {
    for (idx, value) in array.iter_mut().enumerate() {
        let decor = value.decor_mut();
        if idx % 2 == 0 {
            decor.set_prefix("\n    ");
        } else {
            decor.set_prefix(" ");
        }
        decor.set_suffix("");
    }
    array.set_trailing_comma(true);
    array.set_trailing("\n");
}

fn normalize_codex_server_item(
    doc: &mut DocumentMut,
    server_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let server_item = &mut doc["mcp_servers"][server_name];
    if server_item.is_none() {
        *server_item = Item::Table(Table::new());
        return Ok(());
    }
    if server_item.is_table() {
        return Ok(());
    }

    let Some(inline) = server_item.as_inline_table() else {
        return Err(
            format!("`mcp_servers.{server_name}` must be a TOML table or inline table").into(),
        );
    };

    let mut table = Table::new();
    for (key, value) in inline.iter() {
        table.insert(key, Item::Value(value.clone()));
    }
    *server_item = Item::Table(table);
    if let Some(mcp_servers) = doc["mcp_servers"].as_table_mut()
        && let Some(mut server_key) = mcp_servers.key_mut(server_name)
    {
        server_key.leaf_decor_mut().clear();
        server_key.dotted_decor_mut().clear();
    }
    Ok(())
}

fn upsert_claude_mcp_server(
    config_path: &Path,
    server_name: &str,
    command: &str,
    args: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut root = if config_path.is_file() {
        let raw = fs::read_to_string(config_path)?;
        serde_json::from_str::<JsonValue>(&raw).map_err(|err| {
            format!(
                "failed to parse JSON claude config {}: {err}",
                config_path.display()
            )
        })?
    } else {
        JsonValue::Object(JsonMap::new())
    };

    let Some(root_obj) = root.as_object_mut() else {
        return Err("claude config root must be a JSON object".into());
    };
    let mcp_servers = root_obj
        .entry("mcpServers".to_string())
        .or_insert_with(|| JsonValue::Object(JsonMap::new()));
    let Some(mcp_obj) = mcp_servers.as_object_mut() else {
        return Err("claude config `mcpServers` must be a JSON object".into());
    };
    mcp_obj.insert(
        server_name.to_string(),
        JsonValue::Object(JsonMap::from_iter([
            (
                "command".to_string(),
                JsonValue::String(command.to_string()),
            ),
            (
                "args".to_string(),
                JsonValue::Array(args.iter().cloned().map(JsonValue::String).collect()),
            ),
        ])),
    );

    let serialized = serialize_json_pretty_with_paired_args(&root)?;
    atomic_write(config_path, &serialized)?;
    Ok(())
}

fn upsert_claude_settings_permission(
    settings_path: &Path,
    server_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut root = if settings_path.is_file() {
        let raw = fs::read_to_string(settings_path)?;
        serde_json::from_str::<JsonValue>(&raw).map_err(|err| {
            format!(
                "failed to parse JSON claude settings {}: {err}",
                settings_path.display()
            )
        })?
    } else {
        JsonValue::Object(JsonMap::new())
    };

    let Some(root_obj) = root.as_object_mut() else {
        return Err("claude settings root must be a JSON object".into());
    };

    // Add tool permission to auto-approve all tools from this MCP server
    let tool_pattern = format!("mcp__{server_name}__*");
    let permissions = root_obj
        .entry("permissions".to_string())
        .or_insert_with(|| JsonValue::Object(JsonMap::new()));
    let Some(perm_obj) = permissions.as_object_mut() else {
        return Err("claude settings `permissions` must be a JSON object".into());
    };
    let allow_list = perm_obj
        .entry("allow".to_string())
        .or_insert_with(|| JsonValue::Array(Vec::new()));
    let Some(allow_arr) = allow_list.as_array_mut() else {
        return Err("claude settings `permissions.allow` must be an array".into());
    };
    // Add the pattern if not already present
    if !allow_arr.iter().any(|v| v.as_str() == Some(&tool_pattern)) {
        allow_arr.push(JsonValue::String(tool_pattern));
    }

    let serialized = serde_json::to_string_pretty(&root)?;
    atomic_write(settings_path, &format!("{serialized}\n"))?;
    Ok(())
}

fn serialize_json_pretty_with_paired_args(
    value: &JsonValue,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut out = String::new();
    write_json_pretty_value(&mut out, value, 0, None)?;
    out.push('\n');
    Ok(out)
}

fn write_json_pretty_value(
    out: &mut String,
    value: &JsonValue,
    indent: usize,
    key: Option<&str>,
) -> Result<(), serde_json::Error> {
    match value {
        JsonValue::Null | JsonValue::Bool(_) | JsonValue::Number(_) | JsonValue::String(_) => {
            out.push_str(&serde_json::to_string(value)?);
        }
        JsonValue::Array(values) => {
            if values.is_empty() {
                out.push_str("[]");
                return Ok(());
            }
            if key == Some("args") && values.iter().all(JsonValue::is_string) {
                out.push('[');
                out.push('\n');
                let mut idx = 0;
                while idx < values.len() {
                    out.push_str(&" ".repeat(indent + 2));
                    out.push_str(&serde_json::to_string(&values[idx])?);
                    if idx + 1 < values.len() {
                        out.push_str(", ");
                        out.push_str(&serde_json::to_string(&values[idx + 1])?);
                    }
                    idx += 2;
                    if idx < values.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                out.push_str(&" ".repeat(indent));
                out.push(']');
                return Ok(());
            }

            out.push('[');
            out.push('\n');
            for (idx, item) in values.iter().enumerate() {
                out.push_str(&" ".repeat(indent + 2));
                write_json_pretty_value(out, item, indent + 2, None)?;
                if idx + 1 < values.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&" ".repeat(indent));
            out.push(']');
        }
        JsonValue::Object(entries) => {
            if entries.is_empty() {
                out.push_str("{}");
                return Ok(());
            }
            out.push('{');
            out.push('\n');
            let len = entries.len();
            for (idx, (entry_key, entry_value)) in entries.iter().enumerate() {
                out.push_str(&" ".repeat(indent + 2));
                write!(out, "{}: ", serde_json::to_string(entry_key)?)
                    .expect("writing to String should not fail");
                write_json_pretty_value(out, entry_value, indent + 2, Some(entry_key.as_str()))?;
                if idx + 1 < len {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&" ".repeat(indent));
            out.push('}');
        }
    }
    Ok(())
}

fn atomic_write(path: &Path, contents: &str) -> Result<(), Box<dyn std::error::Error>> {
    let Some(parent) = path.parent() else {
        return Err(format!("path has no parent: {}", path.display()).into());
    };
    if !parent.exists() {
        return Err(format!("parent directory does not exist: {}", parent.display()).into());
    }

    let tmp_name = format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("config")
    );
    let tmp_path = parent.join(tmp_name);
    fs::write(&tmp_path, contents)?;
    fs::rename(&tmp_path, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_codex_mcp_server_creates_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("config.toml");
        upsert_codex_mcp_server(
            &config,
            "console",
            "/usr/local/bin/mcp-repl",
            &["--interpreter".to_string(), "python".to_string()],
        )
        .expect("upsert codex");
        let text = fs::read_to_string(config).expect("read config");
        let doc = text.parse::<DocumentMut>().expect("parse generated config");
        assert_eq!(
            doc["mcp_servers"]["console"]["command"].as_str(),
            Some("/usr/local/bin/mcp-repl")
        );
        assert_eq!(
            doc["mcp_servers"]["console"]["tool_timeout_sec"].as_integer(),
            Some(1800)
        );
        let args = doc["mcp_servers"]["console"]["args"]
            .as_array()
            .expect("args array");
        assert_eq!(args.len(), 2);
        assert_eq!(args.get(0).and_then(|v| v.as_str()), Some("--interpreter"));
        assert_eq!(args.get(1).and_then(|v| v.as_str()), Some("python"));
        assert!(
            text.contains("args = [\n"),
            "expected args array to be multiline for readability"
        );
        assert!(
            text.contains("\"--interpreter\", \"python\""),
            "expected paired install args to share one line"
        );
        assert!(
            text.contains("mcp-repl handles the primary timeout"),
            "expected generated timeout rationale comment in config"
        );
    }

    #[test]
    fn upsert_codex_mcp_server_handles_existing_inline_table_server_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("config.toml");
        fs::write(
            &config,
            r#"[mcp_servers]
repl = { command = "/usr/local/bin/old-mcp-repl", args = ["--interpreter", "r"] }
"#,
        )
        .expect("seed config");

        upsert_codex_mcp_server(
            &config,
            "repl",
            "/path/to/mcp-repl",
            &[
                "--sandbox".to_string(),
                "workspace-write".to_string(),
                "--config".to_string(),
                "sandbox_workspace_write.network_access=false".to_string(),
            ],
        )
        .expect("upsert codex");

        let text = fs::read_to_string(&config).expect("read config");
        let doc = text.parse::<DocumentMut>().expect("parse generated config");
        assert!(
            !text.contains("repl = {"),
            "server entry should not remain an inline table after normalization"
        );
        assert_eq!(
            doc["mcp_servers"]["repl"]["command"].as_str(),
            Some("/path/to/mcp-repl")
        );
        assert_eq!(
            doc["mcp_servers"]["repl"]["tool_timeout_sec"].as_integer(),
            Some(CODEX_TOOL_TIMEOUT_SECS)
        );
    }

    #[test]
    fn upsert_codex_mcp_server_preserves_other_config_in_messy_existing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("config.toml");
        fs::write(
            &config,
            r#"# Existing config with uneven spacing and comments

[mcp_servers]
  # keep this note
repl={command="/usr/local/bin/old-mcp-repl",args=["--interpreter","r"]}
r = { command = "/usr/local/bin/legacy-repl" }

[workspace]
name="demo"
"#,
        )
        .expect("seed config");

        upsert_codex_mcp_server(
            &config,
            "repl",
            "/path/to/mcp-repl",
            &["--interpreter".to_string(), "r".to_string()],
        )
        .expect("upsert codex");

        let text = fs::read_to_string(&config).expect("read config");
        let doc = text.parse::<DocumentMut>().expect("parse generated config");
        assert!(
            text.contains("# Existing config with uneven spacing and comments"),
            "top-level comments should remain in place"
        );
        assert_eq!(
            doc["workspace"]["name"].as_str(),
            Some("demo"),
            "non-mcp sections should be preserved"
        );
        assert_eq!(
            doc["mcp_servers"]["r"]["command"].as_str(),
            Some("/usr/local/bin/legacy-repl"),
            "other MCP servers should be preserved"
        );
    }

    #[test]
    fn upsert_codex_mcp_server_handles_existing_empty_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("config.toml");
        fs::write(&config, "").expect("seed empty config");

        upsert_codex_mcp_server(
            &config,
            "repl",
            "/path/to/mcp-repl",
            &["--interpreter".to_string(), "r".to_string()],
        )
        .expect("upsert codex");

        let text = fs::read_to_string(&config).expect("read config");
        let doc = text.parse::<DocumentMut>().expect("parse generated config");
        assert_eq!(
            doc["mcp_servers"]["repl"]["command"].as_str(),
            Some("/path/to/mcp-repl")
        );
        assert_eq!(
            doc["mcp_servers"]["repl"]["args"]
                .as_array()
                .expect("args")
                .len(),
            2
        );
    }

    #[test]
    fn upsert_codex_mcp_server_adds_inherit_comment() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("config.toml");
        upsert_codex_mcp_server(
            &config,
            "repl",
            "/path/to/mcp-repl",
            &["--sandbox".to_string(), "inherit".to_string()],
        )
        .expect("upsert codex");

        let text = fs::read_to_string(config).expect("read config");
        assert!(
            text.contains("--sandbox inherit: use sandbox policy updates sent by Codex"),
            "expected inherit comment in codex config"
        );
    }

    #[test]
    fn upsert_codex_mcp_server_clears_inherit_comment_when_not_using_inherit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("config.toml");
        upsert_codex_mcp_server(
            &config,
            "repl",
            "/path/to/mcp-repl",
            &["--sandbox".to_string(), "inherit".to_string()],
        )
        .expect("upsert codex inherit");
        upsert_codex_mcp_server(
            &config,
            "repl",
            "/path/to/mcp-repl",
            &["--sandbox".to_string(), "workspace-write".to_string()],
        )
        .expect("upsert codex workspace-write");

        let text = fs::read_to_string(config).expect("read config");
        assert!(
            !text.contains("--sandbox inherit: use sandbox policy updates sent by Codex"),
            "inherit-only comment should be removed when inherit is no longer configured"
        );
    }

    #[test]
    fn codex_install_args_defaults_to_inherit() {
        let args = codex_install_args(&["--interpreter".to_string(), "python".to_string()]);
        assert_eq!(
            args,
            vec![
                "--interpreter".to_string(),
                "python".to_string(),
                "--sandbox".to_string(),
                "inherit".to_string()
            ]
        );
    }

    #[test]
    fn claude_install_args_defaults_to_workspace_write() {
        let args = claude_install_args(&["--interpreter".to_string(), "python".to_string()]);
        assert_eq!(
            args,
            vec![
                "--interpreter".to_string(),
                "python".to_string(),
                "--sandbox".to_string(),
                "workspace-write".to_string()
            ]
        );
    }

    #[test]
    fn install_args_preserve_explicit_sandbox_config() {
        let base = vec![
            "--sandbox".to_string(),
            "read-only".to_string(),
            "--interpreter".to_string(),
            "python".to_string(),
        ];
        assert_eq!(codex_install_args(&base), base);
        assert_eq!(claude_install_args(&base), base);
    }

    #[test]
    fn install_args_preserve_explicit_sandbox_config_via_config_flag() {
        let base = vec![
            "--config".to_string(),
            "sandbox_mode=read-only".to_string(),
            "--interpreter".to_string(),
            "python".to_string(),
        ];
        assert_eq!(codex_install_args(&base), base);
        assert_eq!(claude_install_args(&base), base);
    }

    #[test]
    fn install_args_preserve_explicit_sandbox_config_via_config_equals() {
        let base = vec![
            "--config=sandbox_workspace_write.network_access=true".to_string(),
            "--interpreter".to_string(),
            "python".to_string(),
        ];
        assert_eq!(codex_install_args(&base), base);
        assert_eq!(claude_install_args(&base), base);
    }

    #[test]
    fn with_interpreter_arg_adds_python_interpreter_when_missing() {
        let args = with_interpreter_arg(
            &["--sandbox".to_string(), "workspace-write".to_string()],
            InstallInterpreter::Python,
        );
        assert_eq!(
            args,
            vec![
                "--sandbox".to_string(),
                "workspace-write".to_string(),
                "--interpreter".to_string(),
                "python".to_string(),
            ]
        );
    }

    #[test]
    fn with_interpreter_arg_preserves_existing_python_interpreter() {
        let args = with_interpreter_arg(
            &[
                "--sandbox".to_string(),
                "workspace-write".to_string(),
                "--interpreter".to_string(),
                "python".to_string(),
            ],
            InstallInterpreter::Python,
        );
        assert_eq!(
            args,
            vec![
                "--sandbox".to_string(),
                "workspace-write".to_string(),
                "--interpreter".to_string(),
                "python".to_string(),
            ]
        );
    }

    #[test]
    fn run_rejects_interpreter_via_arg() {
        let err = run(InstallOptions {
            targets: vec![InstallTarget::Codex],
            interpreters: vec![InstallInterpreter::R],
            args: vec!["--interpreter".to_string(), "python".to_string()],
        })
        .expect_err("expected rejection");
        assert!(
            err.to_string()
                .contains("install does not accept interpreter selection via --arg"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn effective_interpreters_defaults_to_full_grid() {
        assert_eq!(
            effective_interpreters(&[]),
            vec![InstallInterpreter::R, InstallInterpreter::Python]
        );
    }

    #[test]
    fn upsert_claude_mcp_server_does_not_add_sandbox_comment_field() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("settings.json");
        upsert_claude_mcp_server(
            &config,
            "repl",
            "/path/to/mcp-repl",
            &[
                "--interpreter".to_string(),
                "python".to_string(),
                "--sandbox".to_string(),
                "workspace-write".to_string(),
            ],
        )
        .expect("upsert claude");

        let text = fs::read_to_string(config).expect("read config");
        let root: JsonValue = serde_json::from_str(&text).expect("parse json");
        let server = &root["mcpServers"]["repl"];
        assert_eq!(
            server["args"][0].as_str(),
            Some("--interpreter"),
            "expected explicit interpreter arg in claude config"
        );
        assert_eq!(
            server["args"][1].as_str(),
            Some("python"),
            "expected explicit python interpreter in claude config"
        );
        assert_eq!(
            server["args"][2].as_str(),
            Some("--sandbox"),
            "expected explicit sandbox arg in claude config"
        );
        assert_eq!(
            server["args"][3].as_str(),
            Some("workspace-write"),
            "expected workspace-write default in claude config"
        );
        assert!(
            text.contains("\"--interpreter\", \"python\""),
            "expected related interpreter args to share one line"
        );
        assert!(
            text.contains("\"--sandbox\", \"workspace-write\""),
            "expected related sandbox args to share one line"
        );
        assert!(
            server.get("_comment_sandbox_state").is_none(),
            "did not expect sandbox comment field in claude config"
        );
    }

    #[test]
    fn upsert_claude_settings_permission_adds_tool_permission() {
        let dir = tempfile::tempdir().expect("tempdir");
        let settings = dir.path().join("settings.json");
        upsert_claude_settings_permission(&settings, "r").expect("upsert permission");

        let text = fs::read_to_string(&settings).expect("read settings");
        let root: JsonValue = serde_json::from_str(&text).expect("parse json");
        let allow = root["permissions"]["allow"]
            .as_array()
            .expect("permissions.allow array");
        assert!(
            allow.iter().any(|v| v.as_str() == Some("mcp__r__*")),
            "expected mcp__r__* in permissions.allow"
        );
    }

    #[test]
    fn upsert_claude_settings_permission_does_not_duplicate_permission() {
        let dir = tempfile::tempdir().expect("tempdir");
        let settings = dir.path().join("settings.json");
        // First upsert
        upsert_claude_settings_permission(&settings, "r").expect("first upsert");
        // Second upsert
        upsert_claude_settings_permission(&settings, "r").expect("second upsert");

        let text = fs::read_to_string(&settings).expect("read settings");
        let root: JsonValue = serde_json::from_str(&text).expect("parse json");
        let allow = root["permissions"]["allow"]
            .as_array()
            .expect("permissions.allow array");
        let count = allow
            .iter()
            .filter(|v| v.as_str() == Some("mcp__r__*"))
            .count();
        assert_eq!(
            count, 1,
            "expected exactly one mcp__r__* entry, not duplicated"
        );
    }

    #[test]
    fn upsert_claude_settings_permission_preserves_existing_permissions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let settings = dir.path().join("settings.json");
        // Seed with existing permissions
        fs::write(
            &settings,
            r#"{
  "permissions": {
    "allow": ["Bash(cargo test:*)"],
    "deny": ["Bash(rm -rf *)"]
  }
}"#,
        )
        .expect("seed settings");

        upsert_claude_settings_permission(&settings, "r").expect("upsert permission");

        let text = fs::read_to_string(&settings).expect("read settings");
        let root: JsonValue = serde_json::from_str(&text).expect("parse json");
        let allow = root["permissions"]["allow"]
            .as_array()
            .expect("permissions.allow array");
        assert!(
            allow
                .iter()
                .any(|v| v.as_str() == Some("Bash(cargo test:*)")),
            "expected existing permission preserved"
        );
        assert!(
            allow.iter().any(|v| v.as_str() == Some("mcp__r__*")),
            "expected new permission added"
        );
        let deny = root["permissions"]["deny"]
            .as_array()
            .expect("permissions.deny array");
        assert!(
            deny.iter().any(|v| v.as_str() == Some("Bash(rm -rf *)")),
            "expected deny list preserved"
        );
    }

    #[test]
    fn resolve_home_dir_prefers_home() {
        let resolved = resolve_home_dir_from_env(
            Some(OsString::from("/tmp/home")),
            Some(OsString::from("/tmp/userprofile")),
            Some(OsString::from("C:")),
            Some(OsString::from(r"\Users\example_user")),
        );
        assert_eq!(resolved, Some(PathBuf::from("/tmp/home")));
    }

    #[test]
    fn resolve_home_dir_falls_back_to_userprofile() {
        let resolved = resolve_home_dir_from_env(
            None,
            Some(OsString::from(r"C:\Users\example_user")),
            Some(OsString::from("C:")),
            Some(OsString::from(r"\Users\other")),
        );
        assert_eq!(resolved, Some(PathBuf::from(r"C:\Users\example_user")));
    }

    #[test]
    fn resolve_home_dir_uses_homedrive_and_homepath_when_needed() {
        let resolved = resolve_home_dir_from_env(
            None,
            None,
            Some(OsString::from("C:")),
            Some(OsString::from(r"\Users\example_user")),
        )
        .expect("home dir");
        assert_eq!(resolved, PathBuf::from(r"C:\Users\example_user"));
    }

    #[test]
    fn resolve_home_dir_returns_none_when_all_sources_missing() {
        let resolved = resolve_home_dir_from_env(None, None, None, None);
        assert!(resolved.is_none());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn resolve_home_dir_uses_absolute_homepath_without_prefixing_homedrive() {
        let resolved = resolve_home_dir_from_env(
            None,
            None,
            Some(OsString::from("D:")),
            Some(OsString::from(r"C:\Users\example_user")),
        )
        .expect("home dir");
        assert_eq!(resolved, PathBuf::from(r"C:\Users\example_user"));
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn resolve_home_dir_treats_windows_style_homepath_as_relative_on_non_windows() {
        let resolved = resolve_home_dir_from_env(
            None,
            None,
            Some(OsString::from("D:")),
            Some(OsString::from(r"C:\Users\example_user")),
        )
        .expect("home dir");
        assert_eq!(resolved, PathBuf::from(r"D:\C:\Users\example_user"));
    }
}
