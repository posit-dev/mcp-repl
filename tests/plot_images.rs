#![cfg(unix)]

mod common;

use base64::Engine as _;
use common::{TestResult, spawn_server};
use rmcp::model::{CallToolResult, RawContent};
use serde::Serialize;
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use std::process::Stdio;
use tempfile::tempdir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::{Duration, sleep};

#[derive(Debug)]
struct ImageData {
    mime_type: String,
    bytes: Vec<u8>,
}

#[derive(Debug, Serialize)]
struct PlotStepSnapshot {
    tool: String,
    input: String,
    response: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct PlotTranscriptSnapshot {
    steps: Vec<PlotStepSnapshot>,
}

struct RawMcpServer {
    child: Child,
    stdin: ChildStdin,
    stdout_lines: Lines<BufReader<ChildStdout>>,
    server_pid: Option<u32>,
}

fn result_text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|item| match &item.raw {
            RawContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn overflow_path(text: &str) -> Option<PathBuf> {
    let marker = "full response at ";
    let start = text.find(marker)? + marker.len();
    let end = text[start..]
        .find('\n')
        .map(|idx| start + idx)
        .unwrap_or(text.len());
    Some(PathBuf::from(text[start..end].trim()))
}

fn extract_all_paths(text: &str, marker: &str) -> Vec<PathBuf> {
    text.lines()
        .filter_map(|line| {
            let start = line.find(marker)? + marker.len();
            Some(PathBuf::from(line[start..].trim()))
        })
        .collect()
}

fn backend_unavailable(text: &str) -> bool {
    text.contains("Fatal error: cannot create 'R_TempDir'")
        || text.contains("failed to start R session")
        || text.contains("worker exited with status")
        || text.contains("worker exited with signal")
        || text.contains("unable to initialize the JIT")
        || text.contains(
            "worker protocol error: ipc disconnected while waiting for request completion",
        )
        || text.contains("options(\"defaultPackages\") was not found")
        || text.contains("worker io error: Broken pipe")
}

impl RawMcpServer {
    async fn spawn() -> TestResult<Self> {
        let exe = resolve_server_path()?;
        let mut args = Vec::new();
        if !common::sandbox_exec_available() {
            args.push("--sandbox".to_string());
            args.push("danger-full-access".to_string());
        }

        let mut child = Command::new(exe)
            .args(&args)
            .env_remove("R_PROFILE_USER")
            .env_remove("R_PROFILE_SITE")
            .env_remove("R_ENVIRON")
            .env_remove("R_ENVIRON_USER")
            .env_remove("MCP_CONSOLE_UPDATE_PLOT_IMAGES")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let server_pid = child.id();
        let stdin = child.stdin.take().ok_or("missing child stdin")?;
        let stdout = child.stdout.take().ok_or("missing child stdout")?;
        Ok(Self {
            child,
            stdin,
            stdout_lines: BufReader::new(stdout).lines(),
            server_pid,
        })
    }

    async fn initialize(&mut self) -> TestResult<()> {
        self.write_json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {
                    "name": "plot-images-test",
                    "version": "1.0"
                }
            }
        }))
        .await?;
        let response = self.read_json().await?;
        assert_eq!(response.get("id"), Some(&json!(1)));
        assert!(
            response.get("result").is_some(),
            "expected initialize result, got: {response:?}"
        );
        self.write_json(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }))
        .await?;
        Ok(())
    }

    async fn call_tool(&mut self, request_id: i64, input: &str, timeout_ms: u64) -> TestResult<()> {
        let mut input = input.to_string();
        if !input.ends_with('\n') {
            input.push('\n');
        }
        self.write_json(&json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "tools/call",
            "params": {
                "name": "repl",
                "arguments": {
                    "input": input,
                    "timeout_ms": timeout_ms,
                }
            }
        }))
        .await
    }

    async fn read_call_tool_result(&mut self, request_id: i64) -> TestResult<CallToolResult> {
        let response = self.read_json().await?;
        assert_eq!(
            response.get("id"),
            Some(&json!(request_id)),
            "unexpected response id: {response:?}"
        );
        if let Some(error) = response.get("error") {
            return Err(format!("unexpected MCP error response: {error}").into());
        }
        let result = response
            .get("result")
            .cloned()
            .ok_or_else(|| format!("missing result in MCP response: {response:?}"))?;
        Ok(serde_json::from_value(result)?)
    }

    async fn write_json(&mut self, value: &Value) -> TestResult<()> {
        let mut line = serde_json::to_vec(value)?;
        line.push(b'\n');
        self.stdin.write_all(&line).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn read_json(&mut self) -> TestResult<Value> {
        let line = tokio::time::timeout(Duration::from_secs(30), self.stdout_lines.next_line())
            .await??
            .ok_or("server stdout closed before JSON-RPC response")?;
        Ok(serde_json::from_str(&line)?)
    }

    async fn cancel(mut self) -> TestResult<()> {
        terminate_process_tree(self.server_pid);
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
        Ok(())
    }
}

fn resolve_server_path() -> TestResult<PathBuf> {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_mcp-repl") {
        return Ok(PathBuf::from(path));
    }
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_mcp-console") {
        return Ok(PathBuf::from(path));
    }

    let mut path = std::env::current_exe()?;
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

fn terminate_process_tree(pid: Option<u32>) {
    let Some(pid) = pid else {
        return;
    };
    let pid_str = pid.to_string();
    let _ = std::process::Command::new("pkill")
        .args(["-TERM", "-P", &pid_str])
        .status();
    unsafe {
        let _ = libc::kill(pid as i32, libc::SIGTERM);
    }
    std::thread::sleep(std::time::Duration::from_millis(200));
    let alive = unsafe { libc::kill(pid as i32, 0) == 0 };
    if alive {
        unsafe {
            let _ = libc::kill(pid as i32, libc::SIGKILL);
        }
    }
}

fn any_backend_unavailable(results: &[&CallToolResult]) -> bool {
    results
        .iter()
        .any(|result| backend_unavailable(&result_text(result)))
}

fn response_snapshot(result: &CallToolResult) -> serde_json::Value {
    let mut value = serde_json::to_value(result)
        .unwrap_or_else(|_| serde_json::json!({"error": "failed to serialize response"}));
    if let Some(content) = value
        .get_mut("content")
        .and_then(|content| content.as_array_mut())
    {
        for item in content {
            let is_image = item
                .get("type")
                .and_then(|value| value.as_str())
                .is_some_and(|value| value == "image");
            if !is_image {
                continue;
            }

            if let Some(data) = item.get_mut("data")
                && let Some(encoded) = data.as_str()
            {
                let hash = blake3::hash(encoded.as_bytes()).to_hex().to_string();
                *data = serde_json::Value::String(format!("blake3:{hash}"));
            }
        }
    }
    value
}

fn step_snapshot(input: &str, result: &CallToolResult) -> PlotStepSnapshot {
    PlotStepSnapshot {
        tool: "r_repl".to_string(),
        input: input.to_string(),
        response: response_snapshot(result),
    }
}

fn extract_images(result: &CallToolResult) -> Vec<ImageData> {
    result
        .content
        .iter()
        .filter_map(|content| match &content.raw {
            RawContent::Image(image) => {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(image.data.as_bytes())
                    .ok()?;
                Some(ImageData {
                    mime_type: image.mime_type.clone(),
                    bytes,
                })
            }
            _ => None,
        })
        .collect()
}

fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if bytes.len() < 24 {
        return None;
    }
    if &bytes[..8] != PNG_SIGNATURE {
        return None;
    }
    if &bytes[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    Some((width, height))
}

fn reference_image_script(name: &str, path: &std::path::Path) -> Option<String> {
    let plot_code = match name {
        "base_plot" => "plot(1:10)",
        "base_plot_update" => "plot(1:10); lines(4:8, 4:8)",
        "grid_plot" => "grid::grid.newpage(); grid::grid.lines(x = c(0.1, 0.9), y = c(0.1, 0.9))",
        "grid_plot_update" => {
            "grid::grid.newpage(); grid::grid.lines(x = c(0.1, 0.9), y = c(0.1, 0.9)); grid::grid.lines(x = c(0.1, 0.9), y = c(0.9, 0.1))"
        }
        _ => return None,
    };
    let path = path.display().to_string();
    let path = path.replace('\\', "\\\\").replace('"', "\\\"");
    Some(format!(
        "grDevices::png(filename = \"{path}\", width = 800, height = 600, res = 96); {plot_code}; grDevices::dev.off()"
    ))
}

fn regenerate_reference_image(name: &str, path: &std::path::Path) {
    let Some(script) = reference_image_script(name, path) else {
        panic!("no Rscript generator registered for reference image {name}");
    };
    let status = std::process::Command::new("Rscript")
        .arg("--vanilla")
        .arg("-e")
        .arg(script)
        .status()
        .unwrap_or_else(|err| panic!("failed to run Rscript for {name}: {err}"));
    assert!(
        status.success(),
        "Rscript failed while generating reference image {name}"
    );
}

fn assert_reference_image(name: &str, bytes: &[u8]) {
    let temp_dir = tempdir().expect("failed to create temp dir for reference image");
    let path = temp_dir.path().join(format!("{name}.png"));
    regenerate_reference_image(name, &path);
    let expected = std::fs::read(&path)
        .unwrap_or_else(|err| panic!("failed to read reference image for {name}: {err}"));
    assert_eq!(expected, bytes, "image did not match reference: {name}");
}

fn assert_no_images(result: &CallToolResult, context: &str) {
    let images = extract_images(result);
    assert!(
        images.is_empty(),
        "expected no images for {context}, got {images:?}"
    );
}

fn assert_plot_snapshot(name: &str, snapshot: &PlotTranscriptSnapshot) -> TestResult<()> {
    let serialized = serde_json::to_string_pretty(snapshot)?;
    if cfg!(target_os = "macos") {
        insta::with_settings!({ snapshot_suffix => "macos" }, {
            insta::assert_snapshot!(name, serialized);
        });
    } else {
        insta::assert_snapshot!(name, serialized);
    }
    Ok(())
}

fn assert_plot_snapshot_pair(name: &str, snapshot: &PlotTranscriptSnapshot) -> TestResult<()> {
    assert_plot_snapshot(name, snapshot)?;
    let transcript = render_plot_transcript(snapshot);
    let suffix = if cfg!(target_os = "macos") {
        "transcript__macos"
    } else {
        "transcript"
    };
    insta::with_settings!({ snapshot_suffix => suffix }, {
        insta::assert_snapshot!(name, transcript);
    });
    Ok(())
}

fn render_plot_transcript(snapshot: &PlotTranscriptSnapshot) -> String {
    let mut out = String::new();
    out.push_str("== transcript ==\n");
    for (index, step) in snapshot.steps.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }

        let is_error = step
            .response
            .get("isError")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);

        if is_error {
            out.push_str(&format!("{}) ! {}\n", index + 1, step.tool));
        } else {
            out.push_str(&format!("{}) {}\n", index + 1, step.tool));
        }

        for line in split_input_lines(&step.input) {
            out.push_str(&format!(">>> {line}\n"));
        }

        for line in plot_response_lines(&step.response) {
            out.push_str(&format!("<<< {line}\n"));
        }
    }

    out.trim_end().to_string()
}

fn split_input_lines(input: &str) -> Vec<String> {
    let trimmed = input.strip_suffix('\n').unwrap_or(input);
    trimmed.split('\n').map(|line| line.to_string()).collect()
}

fn plot_response_lines(response: &serde_json::Value) -> Vec<String> {
    let mut lines = Vec::new();
    let Some(items) = response.get("content").and_then(|value| value.as_array()) else {
        return lines;
    };

    for item in items {
        let item_type = item.get("type").and_then(|value| value.as_str());
        match item_type {
            Some("text") => {
                if let Some(text) = item.get("text").and_then(|value| value.as_str()) {
                    for line in split_text_lines(text) {
                        if is_prompt_line(&line) {
                            continue;
                        }
                        lines.push(line);
                    }
                }
            }
            Some("image") => {
                let mime_type = item
                    .get("mimeType")
                    .and_then(|value| value.as_str())
                    .unwrap_or("image");
                let data = item
                    .get("data")
                    .and_then(|value| value.as_str())
                    .unwrap_or("data");
                lines.push(format!("[{mime_type} {data}]"));
            }
            Some("audio") => {
                let mime_type = item
                    .get("mimeType")
                    .and_then(|value| value.as_str())
                    .unwrap_or("audio");
                let data = item
                    .get("data")
                    .and_then(|value| value.as_str())
                    .unwrap_or("data");
                lines.push(format!("[{mime_type} {data}]"));
            }
            Some("resource") => {
                lines.push(format!("[resource {item}]"));
            }
            Some("resourceLink") => {
                lines.push(format!("[resource_link {item}]"));
            }
            _ => {
                lines.push(format!("[content {item}]"));
            }
        }
    }
    lines
}

fn split_text_lines(text: &str) -> Vec<String> {
    text.split('\n').map(|line| line.to_string()).collect()
}

fn is_prompt_line(line: &str) -> bool {
    if line.is_empty() {
        return false;
    }
    if line.starts_with(' ') || line.starts_with('\t') {
        return false;
    }
    line.starts_with('>') || line.starts_with('+') || line.starts_with("Browse[")
}

#[tokio::test(flavor = "multi_thread")]
async fn plots_emit_images_and_updates() -> TestResult<()> {
    let mut session = spawn_server().await?;
    let mut steps = Vec::new();

    let plot_input = "plot(1:10)";
    let plot_result = session.write_stdin_raw_with(plot_input, Some(30.0)).await?;
    steps.push(step_snapshot(plot_input, &plot_result));

    let update_input = "lines(4:8, 4:8)";
    let update_result = session
        .write_stdin_raw_with(update_input, Some(30.0))
        .await?;
    steps.push(step_snapshot(update_input, &update_result));

    let noop_input = "1+1";
    let noop_result = session.write_stdin_raw_with(noop_input, Some(30.0)).await?;
    steps.push(step_snapshot(noop_input, &noop_result));
    if any_backend_unavailable(&[&plot_result, &update_result, &noop_result]) {
        eprintln!("plot_images backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;

    assert_ne!(
        plot_result.is_error,
        Some(true),
        "plot(1:10) reported an error: {}",
        result_text(&plot_result)
    );
    assert_ne!(
        update_result.is_error,
        Some(true),
        "lines(4:8, 4:8) reported an error: {}",
        result_text(&update_result)
    );
    assert_ne!(
        noop_result.is_error,
        Some(true),
        "1+1 reported an error: {}",
        result_text(&noop_result)
    );

    let plot_images = extract_images(&plot_result);
    let update_images = extract_images(&update_result);

    assert!(
        !plot_images.is_empty(),
        "expected plot(1:10) to emit image content"
    );
    assert!(
        !update_images.is_empty(),
        "expected lines(4:8, 4:8) to emit image content"
    );
    assert_eq!(plot_images[0].mime_type, "image/png");
    assert_eq!(update_images[0].mime_type, "image/png");
    assert_ne!(
        plot_images[0].bytes, update_images[0].bytes,
        "expected updated plot image to differ from initial plot"
    );
    assert_reference_image("base_plot", &plot_images[0].bytes);
    assert_reference_image("base_plot_update", &update_images[0].bytes);
    assert_no_images(&noop_result, "base 1+1");

    let snapshot = PlotTranscriptSnapshot { steps };
    assert_plot_snapshot_pair("plots_emit_images_and_updates", &snapshot)?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn plots_emit_stable_images_for_repeats() -> TestResult<()> {
    let mut session = spawn_server().await?;
    let mut steps = Vec::new();

    let plot_input = "plot(1:10)";
    let first_result = session.write_stdin_raw_with(plot_input, Some(30.0)).await?;
    steps.push(step_snapshot(plot_input, &first_result));
    let second_result = session.write_stdin_raw_with(plot_input, Some(30.0)).await?;
    steps.push(step_snapshot(plot_input, &second_result));

    let noop_input = "1+1";
    let noop_result = session.write_stdin_raw_with(noop_input, Some(30.0)).await?;
    steps.push(step_snapshot(noop_input, &noop_result));
    if any_backend_unavailable(&[&first_result, &second_result, &noop_result]) {
        eprintln!("plot_images backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;

    assert_ne!(
        first_result.is_error,
        Some(true),
        "first plot(1:10) reported an error: {}",
        result_text(&first_result)
    );
    assert_ne!(
        second_result.is_error,
        Some(true),
        "second plot(1:10) reported an error: {}",
        result_text(&second_result)
    );
    assert_ne!(
        noop_result.is_error,
        Some(true),
        "1+1 reported an error: {}",
        result_text(&noop_result)
    );

    let first_images = extract_images(&first_result);
    let second_images = extract_images(&second_result);

    assert!(
        !first_images.is_empty(),
        "expected first plot(1:10) to emit image content"
    );
    assert!(
        !second_images.is_empty(),
        "expected second plot(1:10) to emit image content"
    );
    assert_eq!(first_images[0].mime_type, "image/png");
    assert_eq!(second_images[0].mime_type, "image/png");
    assert_eq!(
        first_images[0].bytes, second_images[0].bytes,
        "expected repeated plot to produce identical image"
    );
    assert_reference_image("base_plot", &first_images[0].bytes);
    assert_no_images(&noop_result, "base repeat 1+1");

    let snapshot = PlotTranscriptSnapshot { steps };
    assert_plot_snapshot_pair("plots_emit_stable_images_for_repeats", &snapshot)?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn multi_panel_plots_emit_single_image() -> TestResult<()> {
    let mut session = spawn_server().await?;
    let mut steps = Vec::new();

    let plot_input = "par(mfrow = c(2, 1)); plot(1:10); plot(10:1)";
    let plot_result = session.write_stdin_raw_with(plot_input, Some(30.0)).await?;
    steps.push(step_snapshot(plot_input, &plot_result));

    let noop_input = "1+1";
    let noop_result = session.write_stdin_raw_with(noop_input, Some(30.0)).await?;
    steps.push(step_snapshot(noop_input, &noop_result));
    if any_backend_unavailable(&[&plot_result, &noop_result]) {
        eprintln!("plot_images backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;

    assert_ne!(
        plot_result.is_error,
        Some(true),
        "multi-panel plot reported an error: {}",
        result_text(&plot_result)
    );
    assert_ne!(
        noop_result.is_error,
        Some(true),
        "1+1 reported an error: {}",
        result_text(&noop_result)
    );

    let plot_images = extract_images(&plot_result);
    assert_eq!(
        plot_images.len(),
        1,
        "expected multi-panel plot to emit a single image update"
    );

    let snapshot = PlotTranscriptSnapshot { steps };
    assert_plot_snapshot_pair("multi_panel_plots_emit_single_image", &snapshot)?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn plots_emit_images_with_large_text_output() -> TestResult<()> {
    let mut session = spawn_server().await?;

    let input = "line <- paste(rep(\"x\", 200), collapse = \"\"); for (i in 1:50) cat(line, \"\\n\"); plot(1:10)";
    let result = session.write_stdin_raw_with(input, Some(30.0)).await?;
    if any_backend_unavailable(&[&result]) {
        eprintln!("plot_images backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;

    assert_ne!(
        result.is_error,
        Some(true),
        "large-output plot reported an error: {}",
        result_text(&result)
    );

    let images = extract_images(&result);
    assert!(
        !images.is_empty(),
        "expected large output to still include plot image content"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn text_overflow_file_preserves_inline_plot_positions() -> TestResult<()> {
    let mut session = spawn_server().await?;

    let input = r#"
cat(paste(rep("x", 12000), collapse = ""))
cat("\nINLINE_OVERFLOW_BEFORE_PLOT\n")
plot(1:10)
cat("INLINE_OVERFLOW_AFTER_PLOT\n")
"#;
    let result = session.write_stdin_raw_with(input, Some(60.0)).await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("plot_images backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    assert_ne!(
        result.is_error,
        Some(true),
        "overflowing plot reported an error: {text}"
    );

    let images = extract_images(&result);
    assert_eq!(images.len(), 1, "expected one inline plot image");
    let image_content_idx = result
        .content
        .iter()
        .position(|content| matches!(content.raw, RawContent::Image(_)))
        .ok_or_else(|| "missing inline image content".to_string())?;
    assert_eq!(
        image_content_idx,
        result.content.len() - 1,
        "expected the inline image to remain the final content item"
    );

    let response_path = overflow_path(&text)
        .ok_or_else(|| format!("expected overflow path in result text, got: {text:?}"))?;
    let overflow_text = fs::read_to_string(&response_path)?;
    let before_idx = overflow_text
        .find("INLINE_OVERFLOW_BEFORE_PLOT")
        .ok_or_else(|| format!("missing pre-plot marker in overflow file: {overflow_text:?}"))?;
    let image_idx = overflow_text.find("image-01.png").ok_or_else(|| {
        format!("missing persisted inline image path in overflow file: {overflow_text:?}")
    })?;
    let after_idx = overflow_text
        .find("INLINE_OVERFLOW_AFTER_PLOT")
        .ok_or_else(|| format!("missing post-plot marker in overflow file: {overflow_text:?}"))?;
    assert!(
        before_idx < after_idx && after_idx < image_idx,
        "expected inline image reference to keep the same trailing position as the public response: {overflow_text:?}"
    );

    let image_path = extract_all_paths(&overflow_text, "full image at ")
        .into_iter()
        .next()
        .ok_or_else(|| format!("missing persisted inline image file path: {overflow_text:?}"))?;
    assert!(
        image_path.exists(),
        "expected persisted inline image file to exist: {image_path:?}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn large_plot_batch_keeps_all_advertised_overflow_paths_alive() -> TestResult<()> {
    let mut session = spawn_server().await?;

    let input = "\
options(console.plot.width = 2, console.plot.height = 2, console.plot.dpi = 10); \
for (i in 1:80) plot(i:(i + 9))";
    let result = session.write_stdin_raw_with(input, Some(120.0)).await?;
    let text = result_text(&result);
    if backend_unavailable(&text) {
        eprintln!("plot_images backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    assert_ne!(
        result.is_error,
        Some(true),
        "large plot batch reported an error: {text}"
    );

    let inline_images = extract_images(&result);
    assert_eq!(inline_images.len(), 4, "expected four inline images");

    let response_path = overflow_path(&text)
        .ok_or_else(|| format!("expected overflow response path, got: {text:?}"))?;
    assert!(
        response_path.exists(),
        "expected overflow response file to exist: {response_path:?}"
    );
    let overflow_text = fs::read_to_string(&response_path)?;
    let overflow_paths = extract_all_paths(&overflow_text, "full image at ");
    assert!(
        !overflow_paths.is_empty(),
        "expected overflow image notices in persisted response file, got: {overflow_text:?}"
    );
    for path in &overflow_paths {
        assert!(
            path.exists(),
            "expected advertised overflow path to exist: {path:?}"
        );
    }

    let overflow_root = response_path
        .parent()
        .ok_or_else(|| format!("overflow response path missing parent: {response_path:?}"))?;
    let retained_file_count = fs::read_dir(overflow_root)?.count();
    assert!(
        retained_file_count > 64,
        "expected current response files to remain live even when they exceed the nominal cap, got {retained_file_count}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn persisted_full_response_does_not_outlive_its_image_artifacts() -> TestResult<()> {
    let mut session = spawn_server().await?;

    let first_input = "\
cat(paste(rep('x', 12000), collapse = '')); \
for (i in 1:7) plot(i:(i + 9))";
    let first_result = session
        .write_stdin_raw_with(first_input, Some(120.0))
        .await?;
    let first_text = result_text(&first_result);
    if backend_unavailable(&first_text) {
        eprintln!("plot_images backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let response_path = overflow_path(&first_text)
        .ok_or_else(|| format!("expected overflow response path, got: {first_text:?}"))?;
    assert!(
        response_path.exists(),
        "expected first overflow response file to exist: {response_path:?}"
    );
    let first_overflow_text = fs::read_to_string(&response_path)?;
    let first_image_paths = extract_all_paths(&first_overflow_text, "full image at ");
    assert!(
        !first_image_paths.is_empty(),
        "expected persisted overflow response to reference image artifacts: {first_overflow_text:?}"
    );

    let second_input = "\
options(console.plot.width = 2, console.plot.height = 2, console.plot.dpi = 10); \
for (i in 8:71) plot(i:(i + 9))";
    let second_result = session
        .write_stdin_raw_with(second_input, Some(120.0))
        .await?;
    let second_text = result_text(&second_result);
    assert_ne!(
        second_result.is_error,
        Some(true),
        "follow-up plot batch reported an error: {second_text}"
    );

    if response_path.exists() {
        let overflow_text = fs::read_to_string(&response_path)?;
        let image_paths = extract_all_paths(&overflow_text, "full image at ");
        for path in image_paths {
            assert!(
                path.exists(),
                "expected persisted full response to keep only live image links, but {path:?} was already evicted while {response_path:?} still exists"
            );
        }
    }

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn overflow_cleanup_retries_after_delete_failure() -> TestResult<()> {
    let mut session = spawn_server().await?;

    let first_result = session
        .write_stdin_raw_with("for (i in 1:7) plot(i:(i + 9))", Some(60.0))
        .await?;
    let first_text = result_text(&first_result);
    if backend_unavailable(&first_text) {
        eprintln!("plot_images backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let blocked_path = extract_all_paths(&first_text, "full image at ")
        .into_iter()
        .next()
        .ok_or_else(|| format!("expected at least one overflow image path, got: {first_text:?}"))?;
    assert!(
        blocked_path.exists(),
        "expected initial overflow path to exist: {blocked_path:?}"
    );

    fs::remove_file(&blocked_path)?;
    fs::create_dir(&blocked_path)?;

    let second_input = "\
options(console.plot.width = 2, console.plot.height = 2, console.plot.dpi = 10); \
for (i in 1:80) plot(i:(i + 9))";
    let second_result = session
        .write_stdin_raw_with(second_input, Some(120.0))
        .await?;
    let second_text = result_text(&second_result);
    assert_ne!(
        second_result.is_error,
        Some(true),
        "second overflow batch reported an error: {second_text}"
    );
    assert!(
        blocked_path.is_dir(),
        "expected blocked overflow path to still be present as a directory after delete failure: {blocked_path:?}"
    );

    fs::remove_dir(&blocked_path)?;
    fs::write(&blocked_path, b"stale-overflow-artifact")?;

    let third_result = session
        .write_stdin_raw_with("for (i in 81:85) plot(i:(i + 9))", Some(60.0))
        .await?;
    let third_text = result_text(&third_result);
    assert_ne!(
        third_result.is_error,
        Some(true),
        "third overflow batch reported an error: {third_text}"
    );

    let overflow_root = blocked_path
        .parent()
        .ok_or_else(|| format!("blocked overflow path missing parent: {blocked_path:?}"))?;
    assert!(
        !blocked_path.exists(),
        "expected cleanup to retry and delete the previously blocked overflow path: {blocked_path:?}"
    );
    let retained_file_count = fs::read_dir(overflow_root)?.count();
    assert!(
        retained_file_count <= 64,
        "expected retrying cleanup to bring the on-disk overflow file count back within the configured cap, got {retained_file_count}"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn plots_respect_numeric_size_options() -> TestResult<()> {
    let mut session = spawn_server().await?;

    let input = "options(console.plot.width = 4, console.plot.height = 3, console.plot.dpi = 100); plot(1:10)";
    let result = session.write_stdin_raw_with(input, Some(30.0)).await?;
    if any_backend_unavailable(&[&result]) {
        eprintln!("plot_images backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert_ne!(
        result.is_error,
        Some(true),
        "numeric size plot reported an error: {}",
        result_text(&result)
    );
    let images = extract_images(&result);
    assert!(
        !images.is_empty(),
        "expected numeric size plot to emit image content"
    );
    let (width, height) =
        png_dimensions(&images[0].bytes).expect("plot did not return a valid png");
    assert_eq!(
        (width, height),
        (400, 300),
        "expected 4x3 inches at 100 dpi to render at 400x300"
    );

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn grid_plots_emit_images_and_updates() -> TestResult<()> {
    let mut session = spawn_server().await?;
    let mut steps = Vec::new();

    let plot_input = "grid::grid.newpage(); grid::grid.lines(x = c(0.1, 0.9), y = c(0.1, 0.9))";
    let plot_result = session.write_stdin_raw_with(plot_input, Some(30.0)).await?;
    steps.push(step_snapshot(plot_input, &plot_result));

    let update_input = "grid::grid.lines(x = c(0.1, 0.9), y = c(0.9, 0.1))";
    let update_result = session
        .write_stdin_raw_with(update_input, Some(30.0))
        .await?;
    steps.push(step_snapshot(update_input, &update_result));

    let noop_input = "1+1";
    let noop_result = session.write_stdin_raw_with(noop_input, Some(30.0)).await?;
    steps.push(step_snapshot(noop_input, &noop_result));
    if any_backend_unavailable(&[&plot_result, &update_result, &noop_result]) {
        eprintln!("plot_images backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;

    assert_ne!(
        plot_result.is_error,
        Some(true),
        "grid plot reported an error: {}",
        result_text(&plot_result)
    );
    assert_ne!(
        update_result.is_error,
        Some(true),
        "grid update reported an error: {}",
        result_text(&update_result)
    );
    assert_ne!(
        noop_result.is_error,
        Some(true),
        "1+1 reported an error: {}",
        result_text(&noop_result)
    );

    let plot_images = extract_images(&plot_result);
    let update_images = extract_images(&update_result);

    assert!(
        !plot_images.is_empty(),
        "expected grid plot to emit image content"
    );
    assert!(
        !update_images.is_empty(),
        "expected grid update to emit image content"
    );
    assert_eq!(plot_images[0].mime_type, "image/png");
    assert_eq!(update_images[0].mime_type, "image/png");
    assert_ne!(
        plot_images[0].bytes, update_images[0].bytes,
        "expected updated grid plot image to differ from initial plot"
    );
    assert_reference_image("grid_plot", &plot_images[0].bytes);
    assert_reference_image("grid_plot_update", &update_images[0].bytes);
    assert_no_images(&noop_result, "grid 1+1");

    let snapshot = PlotTranscriptSnapshot { steps };
    assert_plot_snapshot_pair("grid_plots_emit_images_and_updates", &snapshot)?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn grid_plots_emit_stable_images_for_repeats() -> TestResult<()> {
    let mut session = spawn_server().await?;
    let mut steps = Vec::new();

    let plot_input = "grid::grid.newpage(); grid::grid.lines(x = c(0.1, 0.9), y = c(0.1, 0.9))";
    let first_result = session.write_stdin_raw_with(plot_input, Some(30.0)).await?;
    steps.push(step_snapshot(plot_input, &first_result));
    let second_result = session.write_stdin_raw_with(plot_input, Some(30.0)).await?;
    steps.push(step_snapshot(plot_input, &second_result));

    let noop_input = "1+1";
    let noop_result = session.write_stdin_raw_with(noop_input, Some(30.0)).await?;
    steps.push(step_snapshot(noop_input, &noop_result));
    if any_backend_unavailable(&[&first_result, &second_result, &noop_result]) {
        eprintln!("plot_images backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;

    assert_ne!(
        first_result.is_error,
        Some(true),
        "first grid plot reported an error: {}",
        result_text(&first_result)
    );
    assert_ne!(
        second_result.is_error,
        Some(true),
        "second grid plot reported an error: {}",
        result_text(&second_result)
    );
    assert_ne!(
        noop_result.is_error,
        Some(true),
        "1+1 reported an error: {}",
        result_text(&noop_result)
    );

    let first_images = extract_images(&first_result);
    let second_images = extract_images(&second_result);

    assert!(
        !first_images.is_empty(),
        "expected first grid plot to emit image content"
    );
    assert!(
        !second_images.is_empty(),
        "expected second grid plot to emit image content"
    );
    assert_eq!(first_images[0].mime_type, "image/png");
    assert_eq!(second_images[0].mime_type, "image/png");
    assert_eq!(
        first_images[0].bytes, second_images[0].bytes,
        "expected repeated grid plot to produce identical image"
    );
    assert_reference_image("grid_plot", &first_images[0].bytes);
    assert_no_images(&noop_result, "grid repeat 1+1");

    let snapshot = PlotTranscriptSnapshot { steps };
    assert_plot_snapshot_pair("grid_plots_emit_stable_images_for_repeats", &snapshot)?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn plot_updates_in_single_request_collapse() -> TestResult<()> {
    let mut session = spawn_server().await?;

    let input = "plot(1:10); lines(2:9, 2:9); lines(2:9, 2:9)";
    let result = session.write_stdin_raw_with(input, Some(30.0)).await?;
    if any_backend_unavailable(&[&result]) {
        eprintln!("plot_images backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;

    assert_ne!(
        result.is_error,
        Some(true),
        "plot updates reported an error: {}",
        result_text(&result)
    );

    let images = extract_images(&result);
    assert_eq!(
        images.len(),
        1,
        "expected a single collapsed image update, got {images:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn plot_emitted_after_truncation() -> TestResult<()> {
    let mut session = spawn_server().await?;

    let input = r#"
cat(paste(rep("x", 3000000), collapse = ""))
cat("\nEND\n")
plot(1:10)
"#;
    let result = session.write_stdin_raw_with(input, Some(60.0)).await?;
    if any_backend_unavailable(&[&result]) {
        eprintln!("plot_images backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    session.cancel().await?;

    assert_ne!(
        result.is_error,
        Some(true),
        "truncation plot reported an error: {}",
        result_text(&result)
    );

    let text = result_text(&result);
    assert!(
        text.contains("output truncated"),
        "expected truncation notice, got: {text:?}"
    );
    assert!(
        overflow_path(&text).is_none(),
        "worker-truncated output must not advertise a recoverable full response file: {text:?}"
    );

    let images = extract_images(&result);
    assert!(
        !images.is_empty(),
        "expected plot image even after truncation"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_overflow_requests_keep_first_response_paths_live_until_delivery()
-> TestResult<()> {
    let session = spawn_server().await?;

    let first_input = "\
options(console.plot.width = 12, console.plot.height = 9, console.plot.dpi = 150); \
Sys.sleep(0.5); \
for (i in 1:7) plot(i:(i + 99))";
    let second_input = |start: i32, end: i32| {
        format!(
            "options(console.plot.width = 2, console.plot.height = 2, console.plot.dpi = 10); for (i in {start}:{end}) plot(i:(i + 9))"
        )
    };

    let (first_result, second_result_a, second_result_b, second_result_c) = {
        let first_request =
            tokio::spawn(session.write_stdin_raw_owned_with(first_input, Some(120.0)));
        sleep(Duration::from_millis(50)).await;
        let second_request_a =
            tokio::spawn(session.write_stdin_raw_owned_with(second_input(8, 74), Some(120.0)));
        let second_request_b =
            tokio::spawn(session.write_stdin_raw_owned_with(second_input(75, 141), Some(120.0)));
        let second_request_c =
            tokio::spawn(session.write_stdin_raw_owned_with(second_input(142, 208), Some(120.0)));
        let first_result = first_request.await??;
        let second_result_a = second_request_a.await??;
        let second_result_b = second_request_b.await??;
        let second_result_c = second_request_c.await??;
        (
            first_result,
            second_result_a,
            second_result_b,
            second_result_c,
        )
    };

    let first_text = result_text(&first_result);
    let second_text_a = result_text(&second_result_a);
    let second_text_b = result_text(&second_result_b);
    let second_text_c = result_text(&second_result_c);
    if any_backend_unavailable(&[
        &first_result,
        &second_result_a,
        &second_result_b,
        &second_result_c,
    ]) {
        eprintln!("plot_images backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    assert_ne!(
        first_result.is_error,
        Some(true),
        "first concurrent overflow batch reported an error: {first_text}"
    );
    assert_ne!(
        second_result_a.is_error,
        Some(true),
        "second concurrent overflow batch A reported an error: {second_text_a}"
    );
    assert_ne!(
        second_result_b.is_error,
        Some(true),
        "second concurrent overflow batch B reported an error: {second_text_b}"
    );
    assert_ne!(
        second_result_c.is_error,
        Some(true),
        "second concurrent overflow batch C reported an error: {second_text_c}"
    );

    let advertised_paths = extract_all_paths(&first_text, "full image at ");
    assert_eq!(
        advertised_paths.len(),
        3,
        "expected three overflow image paths in the first result, got: {first_text:?}"
    );
    for path in advertised_paths {
        assert!(
            path.exists(),
            "expected advertised overflow path from the first concurrent response to still exist when delivered: {path:?}"
        );
    }

    session.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn raw_transport_keeps_overflow_paths_live_until_first_reply_is_read() -> TestResult<()> {
    let mut server = RawMcpServer::spawn().await?;
    server.initialize().await?;

    let first_input = "\
options(console.plot.width = 14, console.plot.height = 10, console.plot.dpi = 160); \
for (i in 1:7) plot(i:(i + 99))";
    let second_input = "\
options(console.plot.width = 2, console.plot.height = 2, console.plot.dpi = 10); \
for (i in 8:74) plot(i:(i + 9))";

    server.call_tool(2, first_input, 120_000).await?;
    sleep(Duration::from_millis(1200)).await;
    server.call_tool(3, second_input, 120_000).await?;
    sleep(Duration::from_millis(300)).await;

    let first_result = server.read_call_tool_result(2).await?;
    let first_text = result_text(&first_result);
    if backend_unavailable(&first_text) {
        eprintln!("plot_images backend unavailable in this environment; skipping");
        server.cancel().await?;
        return Ok(());
    }

    assert_ne!(
        first_result.is_error,
        Some(true),
        "first raw transport overflow batch reported an error: {first_text}"
    );

    let overflow_paths = extract_all_paths(&first_text, "full image at ");
    assert_eq!(
        overflow_paths.len(),
        3,
        "expected three overflow image paths in the first raw reply, got: {first_text:?}"
    );
    for path in overflow_paths {
        assert!(
            path.exists(),
            "expected raw transport to keep first-reply overflow path live until that reply is read: {path:?}"
        );
    }

    server.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn raw_transport_keeps_written_but_unread_overflow_paths_live() -> TestResult<()> {
    let mut server = RawMcpServer::spawn().await?;
    server.initialize().await?;

    let first_input = "\
options(console.plot.width = 2, console.plot.height = 2, console.plot.dpi = 10); \
for (i in 1:7) plot(i:(i + 9))";
    let second_input = "\
options(console.plot.width = 2, console.plot.height = 2, console.plot.dpi = 10); \
for (i in 8:90) plot(i:(i + 9))";

    server.call_tool(2, first_input, 120_000).await?;
    sleep(Duration::from_millis(1500)).await;
    server.call_tool(3, second_input, 120_000).await?;
    sleep(Duration::from_millis(800)).await;

    let first_result = server.read_call_tool_result(2).await?;
    let first_text = result_text(&first_result);
    if backend_unavailable(&first_text) {
        eprintln!("plot_images backend unavailable in this environment; skipping");
        server.cancel().await?;
        return Ok(());
    }

    assert_ne!(
        first_result.is_error,
        Some(true),
        "first written-but-unread raw transport overflow batch reported an error: {first_text}"
    );

    let overflow_paths = extract_all_paths(&first_text, "full image at ");
    assert_eq!(
        overflow_paths.len(),
        3,
        "expected three overflow image paths in the first raw reply, got: {first_text:?}"
    );
    for path in overflow_paths {
        assert!(
            path.exists(),
            "expected raw transport to keep a written-but-unread reply's overflow path live: {path:?}"
        );
    }

    server.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn truncated_preview_keeps_only_complete_overflow_image_paths() -> TestResult<()> {
    let mut session = spawn_server().await?;

    for plot_count in 120..=128 {
        let input = format!(
            "options(console.plot.width = 2, console.plot.height = 2, console.plot.dpi = 10); for (i in 1:{plot_count}) plot(i:(i + 9))"
        );
        let result = session.write_stdin_raw_with(input, Some(120.0)).await?;
        let text = result_text(&result);
        if backend_unavailable(&text) {
            eprintln!("plot_images backend unavailable in this environment; skipping");
            session.cancel().await?;
            return Ok(());
        }

        assert_ne!(
            result.is_error,
            Some(true),
            "large plot batch for plot_count={plot_count} reported an error: {text}"
        );
        assert!(
            text.contains("output truncated"),
            "expected a truncated inline preview for plot_count={plot_count}, got: {text:?}"
        );
        assert!(
            overflow_path(&text).is_some(),
            "expected a persisted full response path for plot_count={plot_count}, got: {text:?}"
        );

        let visible_paths = extract_all_paths(&text, "full image at ");
        assert!(
            !visible_paths.is_empty(),
            "expected visible overflow image notices for plot_count={plot_count}, got: {text:?}"
        );
        for path in visible_paths {
            assert!(
                path.exists(),
                "expected every visible overflow image notice to point at a live file for plot_count={plot_count}, but got {path:?} from {text:?}"
            );
        }
    }

    session.cancel().await?;
    Ok(())
}
