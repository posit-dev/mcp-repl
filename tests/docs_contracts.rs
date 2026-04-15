use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(path: &Path) -> String {
    fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()))
}

fn assert_exists(path: &Path) {
    assert!(path.exists(), "expected {} to exist", path.display());
}

#[test]
fn agents_is_short_and_points_to_main_docs() {
    let agents = read(&repo_root().join("AGENTS.md"));
    assert!(
        agents.lines().count() <= 120,
        "AGENTS.md should stay at 120 lines or less"
    );

    for required in [
        "docs/index.md",
        "docs/architecture.md",
        "docs/testing.md",
        "docs/debugging.md",
        "docs/sandbox.md",
        "docs/plans/AGENTS.md",
    ] {
        assert!(agents.contains(required), "missing {required} in AGENTS.md");
    }
}

#[test]
fn docs_index_lists_main_docs() {
    let root = repo_root();
    let index = read(&root.join("docs/index.md"));

    for required in [
        "docs/architecture.md",
        "docs/testing.md",
        "docs/debugging.md",
        "docs/sandbox.md",
        "docs/worker_sideband_protocol.md",
        "docs/plans/AGENTS.md",
    ] {
        assert_exists(&root.join(required));
        assert!(
            index.contains(required),
            "missing {required} in docs/index.md"
        );
    }
}

#[test]
fn plans_layout_exists() {
    let root = repo_root();
    for required in [
        "docs/plans/AGENTS.md",
        "docs/plans/active",
        "docs/plans/completed",
        "docs/plans/tech-debt.md",
    ] {
        assert_exists(&root.join(required));
    }
}

#[test]
fn plot_image_snapshots_do_not_expose_mcp_console_meta() {
    let snapshots_dir = repo_root().join("tests/snapshots");
    for entry in fs::read_dir(&snapshots_dir)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", snapshots_dir.display()))
    {
        let entry = entry.unwrap_or_else(|err| panic!("failed to read snapshot entry: {err}"));
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with("plot_images__") || !name.ends_with(".snap") {
            continue;
        }
        let contents = read(&path);
        assert!(
            !contents.contains("\"_meta\""),
            "plot snapshot should not expose _meta: {}",
            path.display()
        );
        assert!(
            !contents.contains("mcpConsole"),
            "plot snapshot should not expose mcpConsole: {}",
            path.display()
        );
    }
}

#[test]
fn plot_reference_snapshots_show_reference_scripts() {
    let snapshots_dir = repo_root().join("tests/snapshots");
    for name in [
        "plot_images__plots_emit_images_and_updates.snap",
        "plot_images__plots_emit_stable_images_for_repeats.snap",
        "plot_images__grid_plots_emit_images_and_updates.snap",
        "plot_images__grid_plots_emit_stable_images_for_repeats.snap",
    ] {
        let contents = read(&snapshots_dir.join(name));
        assert!(
            contents.contains("\"data\": \"blake3:<"),
            "plot snapshot should expose a canonical image placeholder: {name}"
        );
        assert!(
            contents.contains("\"command\": \"Rscript --vanilla -\""),
            "plot snapshot should expose the reference command: {name}"
        );
        assert!(
            contents.contains("\"envVar\": \"MCP_REPL_TEST_PNG_DEST\""),
            "plot snapshot should expose the reference env var: {name}"
        );
        assert!(
            contents
                .contains(r#""grDevices::png(filename = Sys.getenv(\"MCP_REPL_TEST_PNG_DEST\")"#),
            "plot snapshot should expose the reference script body: {name}"
        );
    }

    for name in [
        "plot_images__plots_emit_images_and_updates@transcript.snap",
        "plot_images__plots_emit_stable_images_for_repeats@transcript.snap",
        "plot_images__grid_plots_emit_images_and_updates@transcript.snap",
        "plot_images__grid_plots_emit_stable_images_for_repeats@transcript.snap",
    ] {
        let contents = read(&snapshots_dir.join(name));
        assert!(
            contents.contains("=== reference "),
            "plot transcript snapshot should expose the reference command: {name}"
        );
        assert!(
            contents.contains("=== env MCP_REPL_TEST_PNG_DEST=<REFERENCE_PNG>"),
            "plot transcript snapshot should expose the reference env var: {name}"
        );
        assert!(
            contents.contains(
                r#"===   grDevices::png(filename = Sys.getenv("MCP_REPL_TEST_PNG_DEST")"#
            ),
            "plot transcript snapshot should expose the reference script body: {name}"
        );
    }
}

#[test]
fn grid_plot_snapshots_show_reference_for_initial_and_updated_images() {
    let snapshots_dir = repo_root().join("tests/snapshots");
    for name in [
        "plot_images__grid_plots_emit_images_and_updates.snap",
        "plot_images__grid_plots_emit_images_and_updates@macos.snap",
    ] {
        let contents = read(&snapshots_dir.join(name));
        assert!(
            contents.contains("\"data\": \"blake3:<grid_plot>\""),
            "grid plot snapshot should expose the base plot reference: {name}"
        );
        assert!(
            contents.contains("\"data\": \"blake3:<grid_plot_update>\""),
            "grid plot snapshot should expose the update reference: {name}"
        );
    }

    for name in [
        "plot_images__grid_plots_emit_images_and_updates@transcript.snap",
        "plot_images__grid_plots_emit_images_and_updates@transcript__macos.snap",
    ] {
        let contents = read(&snapshots_dir.join(name));
        assert!(
            contents.contains("=== reference grid_plot via Rscript --vanilla -"),
            "grid plot transcript should expose the base plot reference: {name}"
        );
        assert!(
            contents.contains("=== reference grid_plot_update via Rscript --vanilla -"),
            "grid plot transcript should expose the update reference: {name}"
        );
    }
}

#[test]
fn multi_panel_plot_snapshots_do_not_claim_a_reference_render() {
    let snapshots_dir = repo_root().join("tests/snapshots");
    for name in [
        "plot_images__multi_panel_plots_emit_single_image.snap",
        "plot_images__multi_panel_plots_emit_single_image@macos.snap",
    ] {
        let contents = read(&snapshots_dir.join(name));
        assert!(
            !contents.contains("\"reference\": {"),
            "multi-panel plot snapshot should not embed a reference render: {name}"
        );
        assert!(
            !contents.contains("blake3:<grid_plot>"),
            "multi-panel plot snapshot should not borrow the grid plot placeholder: {name}"
        );
    }

    for name in [
        "plot_images__multi_panel_plots_emit_single_image@transcript.snap",
        "plot_images__multi_panel_plots_emit_single_image@transcript__macos.snap",
    ] {
        let contents = read(&snapshots_dir.join(name));
        assert!(
            !contents.contains("=== reference "),
            "multi-panel transcript should not embed a reference render: {name}"
        );
        assert!(
            !contents.contains("blake3:<grid_plot>"),
            "multi-panel transcript should not borrow the grid plot placeholder: {name}"
        );
    }
}
