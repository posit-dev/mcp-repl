#![cfg(unix)]

mod common;

use base64::Engine as _;
use common::{TestResult, spawn_server};
use rmcp::model::{CallToolResult, RawContent};
use serde::Serialize;
use std::fs;
use std::path::PathBuf;
use tempfile::tempdir;

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
async fn overflow_store_reenforces_cap_after_large_plot_batch_finishes() -> TestResult<()> {
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

    let response_path = overflow_path(&text);
    let overflow_paths = extract_all_paths(&text, "full image at ");
    assert!(
        response_path.is_some() || !overflow_paths.is_empty(),
        "expected overflow artifacts in the response, got: {text:?}"
    );

    let overflow_root = response_path
        .as_deref()
        .or_else(|| overflow_paths.first().map(PathBuf::as_path))
        .and_then(std::path::Path::parent)
        .ok_or_else(|| {
            format!(
                "missing overflow root for response_path={response_path:?} overflow_paths={overflow_paths:?}"
            )
        })?;
    let retained_file_count = fs::read_dir(overflow_root)?.count();
    assert_eq!(
        retained_file_count, 64,
        "expected overflow store to enforce the 64-file cap after the response finished"
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

    let images = extract_images(&result);
    assert!(
        !images.is_empty(),
        "expected plot image even after truncation"
    );

    Ok(())
}
