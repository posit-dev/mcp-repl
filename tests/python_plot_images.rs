#![cfg(unix)]

mod common;

use base64::Engine as _;
use common::TestResult;
use rmcp::model::{CallToolResult, RawContent};
use serde::Serialize;
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
        tool: "py_repl".to_string(),
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

fn python_plot_preamble() -> &'static str {
    r#"import matplotlib.pyplot as plt; plt.rcParams["figure.dpi"] = 96; plt.rcParams["figure.figsize"] = (8.3333333333, 6.25)"#
}

fn reference_image_script(name: &str, path: &std::path::Path) -> Option<String> {
    let plot_code = match name {
        "base_plot" => "plt.figure(1)\nplt.clf()\nplt.plot(list(range(1, 11)))",
        "base_plot_update" => {
            "plt.figure(1)\nplt.clf()\nplt.plot(list(range(1, 11)))\nplt.plot(list(range(4, 9)), list(range(4, 9)))"
        }
        "grid_plot" => "plt.figure(2)\nplt.clf()\nplt.plot([0.1, 0.9], [0.1, 0.9])",
        "grid_plot_update" => {
            "plt.figure(2)\nplt.clf()\nplt.plot([0.1, 0.9], [0.1, 0.9])\nplt.plot([0.1, 0.9], [0.9, 0.1])"
        }
        _ => return None,
    };
    let path = path.display().to_string();
    let path = path.replace('\\', "\\\\").replace('"', "\\\"");
    Some(format!(
        r#"import matplotlib
matplotlib.use(\"agg\", force=True)
import matplotlib.pyplot as plt
plt.rcParams[\"figure.dpi\"] = 96
plt.rcParams[\"figure.figsize\"] = (8.3333333333, 6.25)
{plot_code}
plt.savefig(r\"\"\"{path}\"\"\", format=\"png\")
"#
    ))
}

fn regenerate_reference_image(name: &str, path: &std::path::Path) {
    let Some(script) = reference_image_script(name, path) else {
        panic!("no python generator registered for reference image {name}");
    };
    let python = common::python_program().unwrap_or("python3");
    let status = std::process::Command::new(python)
        .arg("-c")
        .arg(script)
        .status()
        .unwrap_or_else(|err| panic!("failed to run {python} for {name}: {err}"));
    assert!(
        status.success(),
        "{python} failed while generating reference image {name}"
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
                lines.push("[resource]".to_string());
            }
            Some("resource_link") => {
                lines.push("[resource_link]".to_string());
            }
            _ => {}
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

fn python_plotting_available() -> bool {
    if !common::python_available() {
        eprintln!("python not available; skipping");
        return false;
    }
    let python = common::python_program().unwrap_or("python3");
    std::process::Command::new(python)
        .args([
            "-c",
            "import matplotlib; matplotlib.use('agg', force=True); import matplotlib.pyplot as plt",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn python_plot_tests_enabled() -> bool {
    if std::env::var_os("MCP_REPL_PYTHON_PLOT_TESTS").is_none() {
        eprintln!("python plot tests disabled; set MCP_REPL_PYTHON_PLOT_TESTS=1 to enable");
        return false;
    }
    python_plotting_available()
}

async fn spawn_python_server_with_pager_page_chars(
    page_bytes: u64,
) -> TestResult<common::McpTestSession> {
    common::spawn_server_with_args_env_and_pager_page_chars(
        vec![
            "--interpreter".to_string(),
            "python".to_string(),
            "--sandbox".to_string(),
            "danger-full-access".to_string(),
        ],
        Vec::new(),
        page_bytes,
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn python_plots_emit_images_and_updates() -> TestResult<()> {
    if !python_plot_tests_enabled() {
        return Ok(());
    }
    let mut session = common::spawn_python_server().await?;
    let mut steps = Vec::new();

    let plot_input = format!(
        "{}; plt.figure(1); plt.clf(); plt.plot(list(range(1, 11))); plt.show()",
        python_plot_preamble()
    );
    let plot_result = session
        .write_stdin_raw_with(&plot_input, Some(30.0))
        .await?;
    steps.push(step_snapshot(&plot_input, &plot_result));
    session.cancel().await?;

    let mut session = common::spawn_python_server().await?;
    let update_input = format!(
        "{}; plt.figure(1); plt.plot(list(range(4, 9)), list(range(4, 9))); plt.show()",
        python_plot_preamble()
    );
    let update_result = session
        .write_stdin_raw_with(&update_input, Some(30.0))
        .await?;
    steps.push(step_snapshot(&update_input, &update_result));
    session.cancel().await?;

    let mut session = common::spawn_python_server().await?;
    let noop_input = "1+1";
    let noop_result = session.write_stdin_raw_with(noop_input, Some(30.0)).await?;
    steps.push(step_snapshot(noop_input, &noop_result));
    session.cancel().await?;

    assert_ne!(
        plot_result.is_error,
        Some(true),
        "plot reported an error: {}",
        result_text(&plot_result)
    );
    assert_ne!(
        update_result.is_error,
        Some(true),
        "update reported an error: {}",
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
        "expected base plot to emit image content"
    );
    assert!(
        !update_images.is_empty(),
        "expected update to emit image content"
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
    assert_plot_snapshot_pair("python_plots_emit_images_and_updates", &snapshot)?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_plots_emit_stable_images_for_repeats() -> TestResult<()> {
    if !python_plot_tests_enabled() {
        return Ok(());
    }
    let mut session = common::spawn_python_server().await?;
    let mut steps = Vec::new();

    let plot_input = format!(
        "{}; plt.figure(1); plt.clf(); plt.plot(list(range(1, 11))); plt.show()",
        python_plot_preamble()
    );
    let first_result = session
        .write_stdin_raw_with(&plot_input, Some(30.0))
        .await?;
    steps.push(step_snapshot(&plot_input, &first_result));
    session.cancel().await?;

    let mut session = common::spawn_python_server().await?;
    let second_result = session
        .write_stdin_raw_with(&plot_input, Some(30.0))
        .await?;
    steps.push(step_snapshot(&plot_input, &second_result));
    session.cancel().await?;

    let mut session = common::spawn_python_server().await?;
    let noop_input = "1+1";
    let noop_result = session.write_stdin_raw_with(noop_input, Some(30.0)).await?;
    steps.push(step_snapshot(noop_input, &noop_result));
    session.cancel().await?;

    assert_ne!(
        first_result.is_error,
        Some(true),
        "first plot reported an error: {}",
        result_text(&first_result)
    );
    assert_ne!(
        second_result.is_error,
        Some(true),
        "second plot reported an error: {}",
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
        "expected first plot to emit image content"
    );
    assert!(
        !second_images.is_empty(),
        "expected second plot to emit image content"
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
    assert_plot_snapshot_pair("python_plots_emit_stable_images_for_repeats", &snapshot)?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_multi_panel_plots_emit_single_image() -> TestResult<()> {
    if !python_plot_tests_enabled() {
        return Ok(());
    }
    let mut session = common::spawn_python_server().await?;
    let mut steps = Vec::new();

    let plot_input = format!(
        "{}; plt.figure(1); plt.clf(); fig = plt.gcf(); ax1 = fig.add_subplot(2, 1, 1); ax2 = fig.add_subplot(2, 1, 2); ax1.plot(list(range(1, 11))); ax2.plot(list(range(10, 0, -1))); plt.show()",
        python_plot_preamble()
    );
    let plot_result = session
        .write_stdin_raw_with(&plot_input, Some(30.0))
        .await?;
    steps.push(step_snapshot(&plot_input, &plot_result));

    let noop_input = "1+1";
    let noop_result = session.write_stdin_raw_with(noop_input, Some(30.0)).await?;
    steps.push(step_snapshot(noop_input, &noop_result));
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
    assert_plot_snapshot_pair("python_multi_panel_plots_emit_single_image", &snapshot)?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_plots_emit_images_when_paged_output() -> TestResult<()> {
    if !python_plot_tests_enabled() {
        return Ok(());
    }
    let mut session = spawn_python_server_with_pager_page_chars(200).await?;

    let input = format!(
        "{}; line = 'x' * 200; exec(\"for _ in range(50):\\\\n    print(line)\"); plt.figure(1); plt.clf(); plt.plot(list(range(1, 11))); plt.show()",
        python_plot_preamble()
    );
    let result = session.write_stdin_raw_with(&input, Some(30.0)).await?;
    session.cancel().await?;

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

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_grid_plots_emit_images_and_updates() -> TestResult<()> {
    if !python_plot_tests_enabled() {
        return Ok(());
    }
    let mut session = common::spawn_python_server().await?;
    let mut steps = Vec::new();

    let plot_input = format!(
        "{}; plt.figure(2); plt.clf(); plt.plot([0.1, 0.9], [0.1, 0.9]); plt.show()",
        python_plot_preamble()
    );
    let plot_result = session
        .write_stdin_raw_with(&plot_input, Some(30.0))
        .await?;
    steps.push(step_snapshot(&plot_input, &plot_result));
    session.cancel().await?;

    let mut session = common::spawn_python_server().await?;
    let update_input = format!(
        "{}; plt.figure(2); plt.plot([0.1, 0.9], [0.9, 0.1]); plt.show()",
        python_plot_preamble()
    );
    let update_result = session
        .write_stdin_raw_with(&update_input, Some(30.0))
        .await?;
    steps.push(step_snapshot(&update_input, &update_result));
    session.cancel().await?;

    let mut session = common::spawn_python_server().await?;
    let noop_input = "1+1";
    let noop_result = session.write_stdin_raw_with(noop_input, Some(30.0)).await?;
    steps.push(step_snapshot(noop_input, &noop_result));
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
    assert_plot_snapshot_pair("python_grid_plots_emit_images_and_updates", &snapshot)?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_grid_plots_emit_stable_images_for_repeats() -> TestResult<()> {
    if !python_plot_tests_enabled() {
        return Ok(());
    }
    let mut session = common::spawn_python_server().await?;
    let mut steps = Vec::new();

    let plot_input = format!(
        "{}; plt.figure(2); plt.clf(); plt.plot([0.1, 0.9], [0.1, 0.9]); plt.show()",
        python_plot_preamble()
    );
    let first_result = session
        .write_stdin_raw_with(&plot_input, Some(30.0))
        .await?;
    steps.push(step_snapshot(&plot_input, &first_result));
    session.cancel().await?;

    let mut session = common::spawn_python_server().await?;
    let second_result = session
        .write_stdin_raw_with(&plot_input, Some(30.0))
        .await?;
    steps.push(step_snapshot(&plot_input, &second_result));
    session.cancel().await?;

    let mut session = common::spawn_python_server().await?;
    let noop_input = "1+1";
    let noop_result = session.write_stdin_raw_with(noop_input, Some(30.0)).await?;
    steps.push(step_snapshot(noop_input, &noop_result));
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
    assert_plot_snapshot_pair(
        "python_grid_plots_emit_stable_images_for_repeats",
        &snapshot,
    )?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn python_plot_updates_in_single_request_collapse() -> TestResult<()> {
    if !python_plot_tests_enabled() {
        return Ok(());
    }
    let mut session = common::spawn_python_server().await?;

    let input = format!(
        "{}; plt.figure(1); plt.clf(); plt.plot(list(range(1, 11))); plt.plot(list(range(2, 10)), list(range(2, 10))); plt.plot(list(range(2, 10)), list(range(2, 10))); plt.show()",
        python_plot_preamble()
    );
    let result = session.write_stdin_raw_with(&input, Some(30.0)).await?;
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
async fn python_plot_emitted_after_large_output() -> TestResult<()> {
    if !python_plot_tests_enabled() {
        return Ok(());
    }
    let mut session = spawn_python_server_with_pager_page_chars(5_000_000).await?;

    let input = format!(
        "{}; print('x' * 3000000); print('END'); plt.figure(1); plt.clf(); plt.plot(list(range(1, 11))); plt.show()",
        python_plot_preamble()
    );
    let result = session.write_stdin_raw_with(&input, Some(60.0)).await?;
    session.cancel().await?;

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
