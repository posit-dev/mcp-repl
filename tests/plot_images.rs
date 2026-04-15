#![cfg(unix)]

mod common;

use base64::Engine as _;
use common::{TestResult, spawn_server_with_files, spawn_server_with_files_env_vars};
use regex_lite::Regex;
use rmcp::model::{CallToolResult, RawContent};
use serde::Serialize;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use tempfile::tempdir;
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
    #[serde(skip_serializing_if = "Option::is_none")]
    reference: Option<ReferenceImageSnapshot>,
}

#[derive(Debug, Serialize)]
struct ReferenceImageSnapshot {
    name: String,
    command: String,
    #[serde(rename = "envVar")]
    env_var: String,
    script: Vec<String>,
}

#[derive(Debug, Serialize)]
struct PlotTranscriptSnapshot {
    steps: Vec<PlotStepSnapshot>,
}

#[derive(Debug)]
struct TextEventRow {
    start_line: usize,
    end_line: usize,
    start_byte: usize,
    end_byte: usize,
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

fn events_log_path(text: &str) -> Option<PathBuf> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(/[^]\s]+/events\.log)").expect("events-log regex should compile")
    });
    re.captures(text)
        .and_then(|caps| caps.get(1))
        .map(|path| PathBuf::from(path.as_str()))
}

fn top_level_entry_names(dir: &Path) -> TestResult<Vec<String>> {
    let mut names = fs::read_dir(dir)?
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
        .collect::<Result<Vec<_>, _>>()?;
    names.sort();
    Ok(names)
}

fn relative_file_paths(root: &Path) -> TestResult<Vec<String>> {
    let mut paths = Vec::new();
    collect_relative_file_paths(root, root, &mut paths)?;
    paths.sort();
    Ok(paths)
}

fn collect_relative_file_paths(root: &Path, dir: &Path, out: &mut Vec<String>) -> TestResult<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            collect_relative_file_paths(root, &path, out)?;
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .expect("file should be under root")
            .to_string_lossy()
            .replace('\\', "/");
        out.push(relative);
    }
    Ok(())
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

fn any_backend_unavailable(results: &[&CallToolResult]) -> bool {
    results
        .iter()
        .any(|result| backend_unavailable(&result_text(result)))
}

fn response_snapshot(result: &CallToolResult, reference_name: Option<&str>) -> serde_json::Value {
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

            let Some(object) = item.as_object_mut() else {
                continue;
            };
            if let Some(reference_name) = reference_name {
                object.insert(
                    "data".to_string(),
                    serde_json::Value::String(format!("blake3:<{reference_name}>")),
                );
            } else {
                object.remove("data");
            }
        }
    }
    value
}

fn assert_images_expose_no_meta(result: &CallToolResult, context: &str) {
    let snapshot = response_snapshot(result, None);
    let content = snapshot
        .get("content")
        .and_then(|value| value.as_array())
        .expect("tool result content should be an array");
    for item in content {
        let is_image = item
            .get("type")
            .and_then(|value| value.as_str())
            .is_some_and(|value| value == "image");
        if is_image {
            assert!(
                item.get("_meta").is_none(),
                "expected image results to omit _meta for {context}: {item}"
            );
        }
    }
}

fn step_snapshot(input: &str, result: &CallToolResult) -> PlotStepSnapshot {
    step_snapshot_with_reference(input, result, None)
}

fn step_snapshot_with_reference(
    input: &str,
    result: &CallToolResult,
    reference_name: Option<&str>,
) -> PlotStepSnapshot {
    PlotStepSnapshot {
        tool: "r_repl".to_string(),
        input: input.to_string(),
        response: response_snapshot(result, reference_name),
        reference: reference_name.map(reference_image_snapshot),
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

fn text_occurrences(text: &str, needle: &str) -> usize {
    text.match_indices(needle).count()
}

fn parse_text_event_rows(events: &str) -> Vec<TextEventRow> {
    events
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("T ")?;
            let mut parts = rest.split_whitespace();
            let lines = parts.next()?.strip_prefix("lines=")?;
            let bytes = parts.next()?.strip_prefix("bytes=")?;
            let mut line_parts = lines.split('-');
            let mut byte_parts = bytes.split('-');
            Some(TextEventRow {
                start_line: line_parts.next()?.parse().ok()?,
                end_line: line_parts.next()?.parse().ok()?,
                start_byte: byte_parts.next()?.parse().ok()?,
                end_byte: byte_parts.next()?.parse().ok()?,
            })
        })
        .collect()
}

fn advance_visible_lines(
    text: &str,
    visible_lines: usize,
    has_partial_line: bool,
) -> (usize, usize, bool) {
    assert!(!text.is_empty(), "text event rows should not be empty");
    let newline_count = text.bytes().filter(|byte| *byte == b'\n').count();
    let start_line = if visible_lines == 0 {
        1
    } else if has_partial_line {
        visible_lines
    } else {
        visible_lines.saturating_add(1)
    };
    let next_visible_lines = if has_partial_line {
        visible_lines
            .saturating_add(newline_count)
            .saturating_add(usize::from(!text.ends_with('\n')))
            .saturating_sub(1)
    } else {
        visible_lines
            .saturating_add(newline_count)
            .saturating_add(usize::from(!text.ends_with('\n')))
    };
    (
        start_line,
        next_visible_lines.max(start_line),
        !text.ends_with('\n'),
    )
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

const REFERENCE_IMAGE_DEST_ENV: &str = "MCP_REPL_TEST_PNG_DEST";
const REFERENCE_IMAGE_COMMAND: &str = "Rscript --vanilla -";

fn reference_image_script_lines(name: &str) -> Option<Vec<&'static str>> {
    let plot_lines = match name {
        "base_plot" => vec!["plot(1:10)"],
        "base_plot_update" => vec!["plot(1:10)", "lines(4:8, 4:8)"],
        "grid_plot" => vec![
            "grid::grid.newpage()",
            "grid::grid.lines(x = c(0.1, 0.9), y = c(0.1, 0.9))",
        ],
        "grid_plot_update" => vec![
            "grid::grid.newpage()",
            "grid::grid.lines(x = c(0.1, 0.9), y = c(0.1, 0.9))",
            "grid::grid.lines(x = c(0.1, 0.9), y = c(0.9, 0.1))",
        ],
        _ => return None,
    };

    let mut lines = vec![
        r#"dest <- Sys.getenv("MCP_REPL_TEST_PNG_DEST")"#,
        "stopifnot(nzchar(dest))",
        "grDevices::png(filename = dest, width = 800, height = 600, res = 96)",
    ];
    lines.extend(plot_lines);
    lines.push("grDevices::dev.off()");
    Some(lines)
}

fn reference_image_script(name: &str) -> Option<String> {
    let lines = reference_image_script_lines(name)?;
    Some(format!("{}\n", lines.join("\n")))
}

fn reference_image_snapshot(name: &str) -> ReferenceImageSnapshot {
    let Some(script) = reference_image_script_lines(name) else {
        panic!("no Rscript generator registered for reference image {name}");
    };
    ReferenceImageSnapshot {
        name: name.to_string(),
        command: REFERENCE_IMAGE_COMMAND.to_string(),
        env_var: REFERENCE_IMAGE_DEST_ENV.to_string(),
        script: script.into_iter().map(str::to_string).collect(),
    }
}

fn regenerate_reference_image(name: &str, path: &std::path::Path) {
    let Some(script) = reference_image_script(name) else {
        panic!("no Rscript generator registered for reference image {name}");
    };
    let mut child = Command::new("Rscript")
        .arg("--vanilla")
        .arg("-")
        .env(REFERENCE_IMAGE_DEST_ENV, path)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| panic!("failed to run Rscript for {name}: {err}"));
    let mut stdin = child
        .stdin
        .take()
        .unwrap_or_else(|| panic!("failed to open Rscript stdin for {name}"));
    stdin
        .write_all(script.as_bytes())
        .unwrap_or_else(|err| panic!("failed to write reference script for {name}: {err}"));
    drop(stdin);
    let status = child
        .wait()
        .unwrap_or_else(|err| panic!("failed to wait for Rscript for {name}: {err}"));
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

const INLINE_TEXT_BUDGET_CHARS: usize = 3500;
const INLINE_TEXT_HARD_SPILL_THRESHOLD_CHARS: usize = INLINE_TEXT_BUDGET_CHARS * 5 / 4;
const UNDER_HARD_SPILL_TEXT_LEN: usize = INLINE_TEXT_BUDGET_CHARS + 200;
const OVER_HARD_SPILL_TEXT_LEN: usize = INLINE_TEXT_HARD_SPILL_THRESHOLD_CHARS + 200;

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

        if let Some(reference) = &step.reference {
            out.push_str(&format!(
                "=== reference {} via {}\n",
                reference.name, reference.command
            ));
            out.push_str(&format!("=== env {}=<REFERENCE_PNG>\n", reference.env_var));
            out.push_str("=== script\n");
            for line in &reference.script {
                out.push_str(&format!("===   {line}\n"));
            }
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
                let Some(data) = item.get("data").and_then(|value| value.as_str()) else {
                    lines.push(format!("[{mime_type}]"));
                    continue;
                };
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
    let mut session = spawn_server_with_files().await?;
    let mut steps = Vec::new();

    let plot_input = "plot(1:10)";
    let plot_result = session.write_stdin_raw_with(plot_input, Some(30.0)).await?;
    steps.push(step_snapshot_with_reference(
        plot_input,
        &plot_result,
        Some("base_plot"),
    ));

    let update_input = "lines(4:8, 4:8)";
    let update_result = session
        .write_stdin_raw_with(update_input, Some(30.0))
        .await?;
    steps.push(step_snapshot_with_reference(
        update_input,
        &update_result,
        Some("base_plot_update"),
    ));

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

    assert_images_expose_no_meta(&plot_result, "plot(1:10)");
    assert_images_expose_no_meta(&update_result, "lines(4:8, 4:8)");
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
    let mut session = spawn_server_with_files().await?;
    let mut steps = Vec::new();

    let plot_input = "plot(1:10)";
    let first_result = session.write_stdin_raw_with(plot_input, Some(30.0)).await?;
    steps.push(step_snapshot_with_reference(
        plot_input,
        &first_result,
        Some("base_plot"),
    ));
    let second_result = session.write_stdin_raw_with(plot_input, Some(30.0)).await?;
    steps.push(step_snapshot_with_reference(
        plot_input,
        &second_result,
        Some("base_plot"),
    ));

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
    let mut session = spawn_server_with_files().await?;
    let mut steps = Vec::new();

    let plot_input = "par(mfrow = c(2, 1)); plot(1:10); plot(10:1)";
    let plot_result = session.write_stdin_raw_with(plot_input, Some(30.0)).await?;
    steps.push(step_snapshot_with_reference(
        plot_input,
        &plot_result,
        Some("grid_plot"),
    ));

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
    assert_images_expose_no_meta(&plot_result, "multi-panel plot");
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
async fn plots_emit_images_when_paged_output() -> TestResult<()> {
    let mut session = spawn_server_with_files().await?;

    let input = "line <- paste(rep(\"x\", 200), collapse = \"\"); for (i in 1:50) cat(line, \"\\n\"); plot(1:10)";
    let result = session.write_stdin_raw_with(input, Some(30.0)).await?;
    if any_backend_unavailable(&[&result]) {
        eprintln!("plot_images backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    assert_ne!(
        result.is_error,
        Some(true),
        "paged plot reported an error: {}",
        result_text(&result)
    );

    let images = extract_images(&result);
    assert!(
        !images.is_empty(),
        "expected large output to still include plot image content"
    );
    assert!(
        !result_text(&result).contains("--More--"),
        "did not expect pager footer in response"
    );
    assert!(
        !result_text(&result).contains("full output:"),
        "did not expect oversized-output path marker in mixed text+image reply: {}",
        result_text(&result)
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn plots_respect_numeric_size_options() -> TestResult<()> {
    let mut session = spawn_server_with_files().await?;

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
    let mut session = spawn_server_with_files().await?;
    let mut steps = Vec::new();

    let plot_input = "grid::grid.newpage(); grid::grid.lines(x = c(0.1, 0.9), y = c(0.1, 0.9))";
    let plot_result = session.write_stdin_raw_with(plot_input, Some(30.0)).await?;
    steps.push(step_snapshot(plot_input, &plot_result));

    let update_input = "grid::grid.lines(x = c(0.1, 0.9), y = c(0.9, 0.1))";
    let update_result = session
        .write_stdin_raw_with(update_input, Some(30.0))
        .await?;
    steps.push(step_snapshot_with_reference(
        update_input,
        &update_result,
        Some("grid_plot_update"),
    ));

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

    assert_images_expose_no_meta(&plot_result, "grid base plot");
    assert_images_expose_no_meta(&update_result, "grid plot update");
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
    let mut session = spawn_server_with_files().await?;
    let mut steps = Vec::new();

    let plot_input = "grid::grid.newpage(); grid::grid.lines(x = c(0.1, 0.9), y = c(0.1, 0.9))";
    let first_result = session.write_stdin_raw_with(plot_input, Some(30.0)).await?;
    steps.push(step_snapshot_with_reference(
        plot_input,
        &first_result,
        Some("grid_plot"),
    ));
    let second_result = session.write_stdin_raw_with(plot_input, Some(30.0)).await?;
    steps.push(step_snapshot_with_reference(
        plot_input,
        &second_result,
        Some("grid_plot"),
    ));

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
    let mut session = spawn_server_with_files().await?;

    let input = "plot(1:10); lines(2:9, 2:9); lines(2:9, 2:9)";
    let result = session.write_stdin_raw_with(input, Some(30.0)).await?;
    if any_backend_unavailable(&[&result]) {
        eprintln!("plot_images backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

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
async fn plot_emitted_after_large_output() -> TestResult<()> {
    let mut session = spawn_server_with_files().await?;

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

    assert_ne!(
        result.is_error,
        Some(true),
        "truncation plot reported an error: {}",
        result_text(&result)
    );

    let text = result_text(&result);
    assert!(
        text.contains("END"),
        "expected the tail of the large output, got: {text:?}"
    );
    assert!(
        !text.contains("output truncated"),
        "did not expect truncation notice, got: {text:?}"
    );

    let images = extract_images(&result);
    assert!(
        !images.is_empty(),
        "expected plot image even after truncation"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn mixed_plot_reply_with_four_images_and_under_grace_text_stays_inline() -> TestResult<()> {
    let mut session = spawn_server_with_files().await?;

    let input = format!(
        r#"
big <- paste(rep("u", {UNDER_HARD_SPILL_TEXT_LEN}), collapse = "")
cat("UNDER_START\n")
cat(big)
cat("\nUNDER_END\n")
for (i in 1:4) {{
  plot(1:10, main = sprintf("plot%03d", i))
}}
"#
    );
    let result = session.write_stdin_raw_with(&input, Some(60.0)).await?;
    if any_backend_unavailable(&[&result]) {
        eprintln!("plot_images backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    assert_ne!(
        result.is_error,
        Some(true),
        "under-grace mixed plot reply reported an error: {}",
        result_text(&result)
    );

    let text = result_text(&result);
    let images = extract_images(&result);

    assert!(
        text.contains("UNDER_START") && text.contains("UNDER_END"),
        "expected under-grace mixed reply text inline, got: {text:?}"
    );
    assert!(
        events_log_path(&text).is_none(),
        "did not expect mixed output bundle for under-grace text, got: {text:?}"
    );
    assert_eq!(
        images.len(),
        4,
        "expected four inline images when text stays under the hard spill threshold"
    );

    session.cancel().await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn mixed_plot_reply_with_two_images_and_over_grace_text_uses_output_bundle() -> TestResult<()>
{
    let mut session = spawn_server_with_files().await?;

    let input = format!(
        r#"
big <- paste(rep("v", {OVER_HARD_SPILL_TEXT_LEN}), collapse = "")
cat("OVER_START\n")
cat(big)
cat("\nOVER_END\n")
for (i in 1:2) {{
  plot(1:10, main = sprintf("plot%03d", i))
}}
"#
    );
    let result = session.write_stdin_raw_with(&input, Some(60.0)).await?;
    if any_backend_unavailable(&[&result]) {
        eprintln!("plot_images backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    assert_ne!(
        result.is_error,
        Some(true),
        "over-grace mixed plot reply reported an error: {}",
        result_text(&result)
    );

    let text = result_text(&result);
    let events_log = events_log_path(&text).unwrap_or_else(|| {
        panic!("expected output bundle events.log path in over-grace mixed reply, got: {text:?}")
    });
    let bundle_dir = events_log
        .parent()
        .unwrap_or_else(|| panic!("events.log missing parent: {events_log:?}"));
    let transcript = fs::read_to_string(bundle_dir.join("transcript.txt"))?;
    let images = extract_images(&result);

    assert_eq!(
        images.len(),
        2,
        "expected output-bundle mixed reply to keep both endpoint images inline"
    );
    assert!(
        transcript.contains("OVER_START") && transcript.contains("OVER_END"),
        "expected transcript.txt to contain the over-grace worker text, got: {transcript:?}"
    );

    session.cancel().await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn single_image_over_grace_text_does_not_duplicate_pre_image_preview() -> TestResult<()> {
    let mut session = spawn_server_with_files().await?;

    let input = format!(
        r#"
cat("PRE_UNIQUE_START\n")
cat(paste(rep("p", 240), collapse = ""))
cat("\nPRE_UNIQUE_END\n")
plot(1:10, main = "single-plot")
big <- paste(rep("v", {OVER_HARD_SPILL_TEXT_LEN}), collapse = "")
cat("POST_START\n")
cat(big)
cat("\nPOST_END\n")
"#
    );
    let result = session.write_stdin_raw_with(&input, Some(60.0)).await?;
    if any_backend_unavailable(&[&result]) {
        eprintln!("plot_images backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    assert_ne!(
        result.is_error,
        Some(true),
        "single-image output bundle reported an error: {}",
        result_text(&result)
    );

    let text = result_text(&result);
    let images = extract_images(&result);
    let events_log = events_log_path(&text).unwrap_or_else(|| {
        panic!("expected output bundle events.log path in single-image reply, got: {text:?}")
    });
    let bundle_dir = events_log
        .parent()
        .unwrap_or_else(|| panic!("events.log missing parent: {events_log:?}"));
    let transcript = fs::read_to_string(bundle_dir.join("transcript.txt"))?;

    assert_eq!(
        images.len(),
        1,
        "expected single-image reply to keep exactly one inline image"
    );
    assert_eq!(
        text_occurrences(&text, "PRE_UNIQUE_START\n"),
        1,
        "did not expect duplicated pre-image preview text, got: {text:?}"
    );
    assert_eq!(
        text_occurrences(&text, "PRE_UNIQUE_END\n"),
        1,
        "did not expect duplicated pre-image preview tail, got: {text:?}"
    );
    assert!(
        transcript.contains("POST_START") && transcript.contains("POST_END"),
        "expected transcript.txt to contain the over-grace worker text, got: {transcript:?}"
    );

    session.cancel().await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn mixed_plot_replies_output_bundle_and_keep_first_and_last_images() -> TestResult<()> {
    let mut session = spawn_server_with_files().await?;

    let input = r#"
for (i in 1:6) {
  cat(sprintf("warn%03d\n", i))
  plot(1:10, main = sprintf("plot%03d", i))
}
"#;
    let result = session.write_stdin_raw_with(input, Some(60.0)).await?;
    if any_backend_unavailable(&[&result]) {
        eprintln!("plot_images backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert_ne!(
        result.is_error,
        Some(true),
        "mixed plot output bundle reported an error: {}",
        result_text(&result)
    );

    let text = result_text(&result);
    let events_log = events_log_path(&text).unwrap_or_else(|| {
        panic!("expected output bundle events.log path in response, got: {text:?}")
    });
    let bundle_dir = events_log
        .parent()
        .unwrap_or_else(|| panic!("events.log missing parent: {events_log:?}"));
    let transcript = fs::read_to_string(bundle_dir.join("transcript.txt"))?;
    let events = fs::read_to_string(&events_log)?;
    let top_level_images = top_level_entry_names(&bundle_dir.join("images"))?;
    let history_files = relative_file_paths(&bundle_dir.join("images/history"))?;
    let images = extract_images(&result);

    assert_eq!(
        images.len(),
        2,
        "expected output-bundle reply to keep exactly two inline images"
    );
    assert_eq!(
        top_level_images,
        vec![
            "001.png".to_string(),
            "002.png".to_string(),
            "003.png".to_string(),
            "004.png".to_string(),
            "005.png".to_string(),
            "006.png".to_string(),
            "history".to_string(),
        ],
        "expected top-level final image aliases in output bundle"
    );
    assert_eq!(
        history_files,
        vec![
            "001/001.png".to_string(),
            "002/001.png".to_string(),
            "003/001.png".to_string(),
            "004/001.png".to_string(),
            "005/001.png".to_string(),
            "006/001.png".to_string(),
        ],
        "expected image history files grouped under images/history"
    );
    assert_eq!(
        images[0].bytes,
        fs::read(bundle_dir.join("images/001.png"))?,
        "expected first inline image to match first top-level final alias"
    );
    assert_eq!(
        images[1].bytes,
        fs::read(bundle_dir.join("images/006.png"))?,
        "expected second inline image to match last top-level final alias"
    );
    assert!(
        text.contains("events.log"),
        "expected response to teach client about events.log, got: {text:?}"
    );
    assert!(
        events.starts_with("v1\ntext transcript.txt\nimages images/\n"),
        "expected events.log header, got: {events:?}"
    );
    assert!(
        events.contains("T lines="),
        "expected text range entries in events.log, got: {events:?}"
    );
    assert!(
        events.contains("I images/history/001/001.png")
            && events.contains("I images/history/006/001.png"),
        "expected first/last image entries in events.log, got: {events:?}"
    );
    assert!(
        transcript.contains("warn001") && transcript.contains("warn006"),
        "expected transcript.txt to contain worker text, got: {transcript:?}"
    );
    assert!(
        !transcript.contains("images/history/001/001.png"),
        "did not expect transcript.txt to contain image paths, got: {transcript:?}"
    );

    session.cancel().await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn mixed_output_bundle_events_log_keeps_partial_line_ranges_stable() -> TestResult<()> {
    let mut session = spawn_server_with_files().await?;

    let input = r#"
cat("a")
flush.console()
Sys.sleep(0.05)
for (i in 1:5) {
  plot(1:10, main = sprintf("plot%03d", i))
  cat(sprintf("b%03d\n", i))
  flush.console()
}
"#;
    let result = session.write_stdin_raw_with(input, Some(60.0)).await?;
    if any_backend_unavailable(&[&result]) {
        eprintln!("plot_images backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert_ne!(
        result.is_error,
        Some(true),
        "mixed plot output bundle with partial lines reported an error: {}",
        result_text(&result)
    );

    let text = result_text(&result);
    let events_log = events_log_path(&text).unwrap_or_else(|| {
        panic!("expected output bundle events.log path in response, got: {text:?}")
    });
    let bundle_dir = events_log
        .parent()
        .unwrap_or_else(|| panic!("events.log missing parent: {events_log:?}"));
    let transcript = fs::read_to_string(bundle_dir.join("transcript.txt"))?;
    let events = fs::read_to_string(&events_log)?;
    let rows = parse_text_event_rows(&events);

    assert!(
        transcript.contains("b001\nb002\nb003\nb004\nb005\n"),
        "expected transcript.txt to preserve the mixed plot text, got: {transcript:?}"
    );
    assert!(
        rows.len() >= 2,
        "expected multiple text rows in events.log for the mixed output bundle, got: {events:?}"
    );
    let mut visible_lines = 0;
    let mut has_partial_line = false;
    for row in rows {
        let text = transcript
            .get(row.start_byte..row.end_byte)
            .unwrap_or_else(|| panic!("expected valid UTF-8 row slice for {row:?}"));
        let (expected_start, expected_end, next_partial) =
            advance_visible_lines(text, visible_lines, has_partial_line);
        assert_eq!(
            (row.start_line, row.end_line),
            (expected_start, expected_end),
            "expected events.log line span to match transcript slice {text:?}, got: {events:?}"
        );
        visible_lines = expected_end;
        has_partial_line = next_partial;
    }

    session.cancel().await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn timeout_image_output_bundle_backfills_earlier_worker_text() -> TestResult<()> {
    let mut session = spawn_server_with_files().await?;

    let input = r#"
cat("warn000\n")
flush.console()
Sys.sleep(0.25)
for (i in 1:6) {
  cat(sprintf("warn%03d\n", i))
  plot(1:10, main = sprintf("plot%03d", i))
}
"#;
    let first = session.write_stdin_raw_with(input, Some(0.05)).await?;
    let first_text = result_text(&first);
    if any_backend_unavailable(&[&first]) {
        eprintln!("plot_images backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert!(
        events_log_path(&first_text).is_none(),
        "did not expect output bundle on first small timeout reply, got: {first_text:?}"
    );

    let result = session.write_stdin_raw_with("", Some(60.0)).await?;
    let text = result_text(&result);
    if text.contains("<<repl status: busy") {
        eprintln!("plot_images timeout output-bundle poll remained busy; skipping");
        session.cancel().await?;
        return Ok(());
    }
    assert_ne!(
        result.is_error,
        Some(true),
        "timeout image output bundle reported an error: {}",
        text
    );

    let events_log = events_log_path(&text).unwrap_or_else(|| {
        panic!("expected output bundle events.log path in timeout poll, got: {text:?}")
    });
    let bundle_dir = events_log
        .parent()
        .unwrap_or_else(|| panic!("events.log missing parent: {events_log:?}"));
    let transcript = fs::read_to_string(bundle_dir.join("transcript.txt"))?;
    let events = fs::read_to_string(&events_log)?;

    assert!(
        transcript.contains("warn000"),
        "expected transcript.txt to backfill early timeout text, got: {transcript:?}"
    );
    assert!(
        transcript.contains("warn006"),
        "expected transcript.txt to include later text, got: {transcript:?}"
    );
    assert!(
        !transcript.contains("<<repl status: busy"),
        "did not expect timeout marker in transcript.txt, got: {transcript:?}"
    );
    assert!(
        events.contains("I images/history/001/001.png")
            && events.contains("I images/history/006/001.png"),
        "expected events.log to cover the full image set, got: {events:?}"
    );
    assert!(
        !events.contains("<<repl status: busy"),
        "did not expect timeout marker in events.log, got: {events:?}"
    );

    session.cancel().await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn timeout_output_bundle_text_only_poll_does_not_duplicate_prefix_text() -> TestResult<()> {
    let mut session = spawn_server_with_files().await?;

    let input = r#"
cat("HEAD_ONLY\n")
flush.console()
Sys.sleep(0.25)
for (i in 1:6) {
  cat(sprintf("plot%03d\n", i))
  plot(1:10, main = sprintf("plot%03d", i))
}
flush.console()
Sys.sleep(1)
cat("TAIL_ONLY\n")
"#;
    let first = session.write_stdin_raw_with(input, Some(0.05)).await?;
    if any_backend_unavailable(&[&first]) {
        eprintln!("plot_images backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    sleep(Duration::from_millis(600)).await;
    let bundled = session.write_stdin_raw_with("", Some(0.05)).await?;
    let bundled_text = result_text(&bundled);
    if bundled_text.contains("<<repl status: busy") && events_log_path(&bundled_text).is_none() {
        eprintln!(
            "plot_images timeout output-bundle poll did not flush image history yet; skipping"
        );
        session.cancel().await?;
        return Ok(());
    }
    let final_result = session.write_stdin_raw_with("", Some(5.0)).await?;
    let final_text = result_text(&final_result);

    assert_ne!(
        final_result.is_error,
        Some(true),
        "timeout text-only follow-up poll reported an error: {}",
        final_text
    );
    assert!(
        !final_text.contains("<<repl status: busy"),
        "expected timeout text-only follow-up poll to finish, got: {final_text:?}"
    );
    assert!(
        events_log_path(&final_text).is_some(),
        "expected output bundle disclosure in final timeout poll, got: {final_text:?}"
    );
    assert_eq!(
        final_text.matches("TAIL_ONLY\n").count(),
        1,
        "expected trailing timeout text segment to appear once, got: {final_text:?}"
    );
    assert!(
        !final_text.contains("> cat(\"TAIL_ONLY\\n\")"),
        "did not expect the trailing command echo to survive the final timeout poll: {final_text:?}"
    );
    assert!(
        !final_text.contains("[repl] input discarded while worker busy"),
        "did not expect empty-poll completion to inject a busy-discard notice: {final_text:?}"
    );

    session.cancel().await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn timeout_output_bundle_image_only_omission_still_discloses_bundle_path() -> TestResult<()> {
    let temp = tempdir()?;
    let mut session = spawn_server_with_files_env_vars(vec![
        ("TMPDIR".to_string(), temp.path().display().to_string()),
        (
            "MCP_REPL_OUTPUT_BUNDLE_MAX_BYTES".to_string(),
            "12000".to_string(),
        ),
    ])
    .await?;

    let input = r#"
Sys.sleep(0.25)
for (i in 1:6) {
  plot(1:10, main = sprintf("plot%03d", i))
}
flush.console()
Sys.sleep(1)
"#;
    let first = session.write_stdin_raw_with(input, Some(0.05)).await?;
    if any_backend_unavailable(&[&first]) {
        eprintln!("plot_images backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    sleep(Duration::from_millis(600)).await;
    let bundled = session.write_stdin_raw_with("", Some(0.05)).await?;
    let bundled_text = result_text(&bundled);
    if bundled_text.contains("<<repl status: busy") && events_log_path(&bundled_text).is_none() {
        eprintln!("plot_images timeout omission poll did not flush bundle state yet; skipping");
        session.cancel().await?;
        return Ok(());
    }

    assert_ne!(
        bundled.is_error,
        Some(true),
        "image-only timeout omission poll reported an error: {}",
        bundled_text
    );
    assert!(
        bundled_text.contains("later content omitted"),
        "expected omission notice in image-only timeout poll, got: {bundled_text:?}"
    );
    assert!(
        bundled_text.contains("output-0001"),
        "expected omission reply to disclose a bundle path, got: {bundled_text:?}"
    );

    session.cancel().await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn timeout_output_bundle_survives_missing_anchor_image() -> TestResult<()> {
    let temp = tempdir()?;
    let mut session = spawn_server_with_files_env_vars(vec![(
        "TMPDIR".to_string(),
        temp.path().display().to_string(),
    )])
    .await?;

    let input = r#"
Sys.sleep(0.25)
for (i in 1:6) {
  plot(1:10, main = sprintf("plot%03d", i))
}
flush.console()
Sys.sleep(1)
"#;
    let first = session.write_stdin_raw_with(input, Some(0.05)).await?;
    if any_backend_unavailable(&[&first]) {
        eprintln!("plot_images backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    sleep(Duration::from_millis(600)).await;
    let bundled = session.write_stdin_raw_with("", Some(0.05)).await?;
    let bundled_text = result_text(&bundled);
    if bundled_text.contains("<<repl status: busy") && events_log_path(&bundled_text).is_none() {
        eprintln!("plot_images timeout bundle poll did not flush image history yet; skipping");
        session.cancel().await?;
        return Ok(());
    }

    let events_log = events_log_path(&bundled_text).unwrap_or_else(|| {
        panic!("expected output bundle events.log path in timeout poll, got: {bundled_text:?}")
    });
    let bundle_dir = events_log
        .parent()
        .unwrap_or_else(|| panic!("events.log missing parent: {events_log:?}"));
    fs::remove_file(bundle_dir.join("images/001.png"))?;

    let damaged = session.write_stdin_raw_with("", Some(0.05)).await?;
    let damaged_text = result_text(&damaged);
    assert_ne!(
        damaged.is_error,
        Some(true),
        "missing anchor image poll reported an error: {}",
        damaged_text
    );
    assert!(
        damaged_text.contains("events.log"),
        "expected damaged anchor poll to keep disclosing the output bundle, got: {damaged_text:?}"
    );

    let mut settled_text = damaged_text;
    while settled_text.contains("<<repl status: busy") {
        sleep(Duration::from_millis(100)).await;
        let next = session.write_stdin_raw_with("", Some(0.5)).await?;
        settled_text = result_text(&next);
    }

    let follow_up = session.write_stdin_raw_with("1+1", Some(5.0)).await?;
    let follow_up_text = result_text(&follow_up);

    session.cancel().await?;

    assert!(
        follow_up_text.contains("[1] 2"),
        "expected session to stay alive after anchor image deletion, got: {follow_up_text:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn same_reply_plot_updates_bundle_preserves_image_history() -> TestResult<()> {
    let mut session = spawn_server_with_files().await?;

    let input = format!(
        r#"
big <- paste(rep("h", {OVER_HARD_SPILL_TEXT_LEN}), collapse = "")
cat("HISTORY_START\n")
cat(big)
cat("\nHISTORY_END\n")
plot(1:10)
lines(2:9, 2:9)
lines(3:8, 3:8)
"#
    );
    let result = session.write_stdin_raw_with(&input, Some(60.0)).await?;
    if any_backend_unavailable(&[&result]) {
        eprintln!("plot_images backend unavailable in this environment; skipping");
        session.cancel().await?;
        return Ok(());
    }

    assert_ne!(
        result.is_error,
        Some(true),
        "same-reply plot history bundle reported an error: {}",
        result_text(&result)
    );

    let text = result_text(&result);
    let events_log = events_log_path(&text).unwrap_or_else(|| {
        panic!("expected output bundle events.log path in response, got: {text:?}")
    });
    let bundle_dir = events_log
        .parent()
        .unwrap_or_else(|| panic!("events.log missing parent: {events_log:?}"));
    let transcript = fs::read_to_string(bundle_dir.join("transcript.txt"))?;
    let events = fs::read_to_string(&events_log)?;
    let top_level_images = top_level_entry_names(&bundle_dir.join("images"))?;
    let history_files = relative_file_paths(&bundle_dir.join("images/history"))?;
    let images = extract_images(&result);

    assert_eq!(
        images.len(),
        1,
        "expected same-reply updates to stay collapsed inline"
    );
    assert_eq!(
        top_level_images,
        vec!["001.png".to_string(), "history".to_string()],
        "expected one top-level final alias plus history"
    );
    assert_eq!(
        history_files,
        vec![
            "001/001.png".to_string(),
            "001/002.png".to_string(),
            "001/003.png".to_string(),
        ],
        "expected every same-reply image update in bundle history"
    );
    assert_eq!(
        images[0].bytes,
        fs::read(bundle_dir.join("images/001.png"))?,
        "expected inline image to match the final bundled image"
    );
    assert_eq!(
        fs::read(bundle_dir.join("images/001.png"))?,
        fs::read(bundle_dir.join("images/history/001/003.png"))?,
        "expected final alias to match the last history entry"
    );
    assert!(
        transcript.contains("HISTORY_START") && transcript.contains("HISTORY_END"),
        "expected transcript.txt to contain worker text, got: {transcript:?}"
    );
    assert!(
        events.contains("I images/history/001/001.png")
            && events.contains("I images/history/001/003.png"),
        "expected events.log to cover the full same-reply history, got: {events:?}"
    );

    session.cancel().await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn same_reply_plot_updates_stay_inline_and_show_final_state() -> TestResult<()> {
    let mut batch_session = spawn_server_with_files().await?;
    let mut control_session = spawn_server_with_files().await?;

    let steps = [
        "plot(1:10)",
        "lines(2:9, 2:9)",
        "lines(3:8, 3:8)",
        "lines(c(1, 10), c(10, 1))",
    ];
    let batch_input = steps.join("\n");
    let batch = batch_session
        .write_stdin_raw_with(&batch_input, Some(60.0))
        .await?;
    if any_backend_unavailable(&[&batch]) {
        eprintln!("plot_images backend unavailable in this environment; skipping");
        batch_session.cancel().await?;
        control_session.cancel().await?;
        return Ok(());
    }

    let batch_text = result_text(&batch);
    let batch_images = extract_images(&batch);
    assert_eq!(
        batch_images.len(),
        1,
        "expected one inline image for same-reply updates, got: {batch_text:?}"
    );
    assert!(
        events_log_path(&batch_text).is_none(),
        "did not expect output bundle for collapsed same-reply updates, got: {batch_text:?}"
    );

    let mut control = None;
    for step in steps {
        let result = control_session
            .write_stdin_raw_with(step, Some(60.0))
            .await?;
        let text = result_text(&result);
        if backend_unavailable(&text) {
            eprintln!("plot_images backend unavailable in this environment; skipping");
            batch_session.cancel().await?;
            control_session.cancel().await?;
            return Ok(());
        }
        control = Some(result);
    }

    let control = control.expect("control sequence should produce a final image");
    let control_images = extract_images(&control);

    batch_session.cancel().await?;
    control_session.cancel().await?;

    assert_eq!(
        control_images.len(),
        1,
        "expected control sequence to end with one inline image"
    );
    assert_eq!(
        batch_images[0].mime_type, control_images[0].mime_type,
        "expected same mime type for batch and control plot replies"
    );
    assert_eq!(
        batch_images[0].bytes, control_images[0].bytes,
        "expected same-reply updates to expose the final plot state inline"
    );

    Ok(())
}
