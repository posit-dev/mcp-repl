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
        "scripts/install.sh",
        "scripts/install.ps1",
    ] {
        assert_exists(&root.join(required));
    }
}

#[test]
fn readme_documents_dev_binary_download_contract() {
    let readme = read(&repo_root().join("README.md"));

    for required in [
        "https://raw.githubusercontent.com/posit-dev/mcp-repl/main/scripts/install.sh",
        "https://raw.githubusercontent.com/posit-dev/mcp-repl/main/scripts/install.ps1",
        "https://github.com/posit-dev/mcp-repl/releases/latest",
        "https://github.com/posit-dev/mcp-repl/releases/latest/download/mcp-repl-x86_64-unknown-linux-gnu.tar.gz",
        "https://github.com/posit-dev/mcp-repl/releases/latest/download/mcp-repl-aarch64-apple-darwin.tar.gz",
        "https://github.com/posit-dev/mcp-repl/releases/latest/download/mcp-repl-x86_64-pc-windows-msvc.zip",
        "Download prebuilt dev binaries",
        "https://github.com/posit-dev/mcp-repl/releases/download/dev/mcp-repl-x86_64-unknown-linux-gnu.tar.gz",
        "https://github.com/posit-dev/mcp-repl/releases/download/dev/mcp-repl-aarch64-apple-darwin.tar.gz",
        "https://github.com/posit-dev/mcp-repl/releases/download/dev/mcp-repl-x86_64-pc-windows-msvc.zip",
        "binaries do not bundle R or Python",
        "glibc 2.35+",
        "glibc build produced on Ubuntu 22.04",
        "**Windows**: experimental",
    ] {
        assert!(readme.contains(required), "missing {required} in README.md");
    }
}

#[test]
fn ci_workflow_defines_dev_release_contract() {
    let workflow = read(&repo_root().join(".github/workflows/ci.yml"));

    for required in [
        "publish-dev:",
        "publish-release:",
        "tags:",
        "- 'v[0-9]+.[0-9]+.[0-9]+'",
        "- 'v[0-9]+.[0-9]+.[0-9]+-[0-9A-Za-z]*'",
        "- '!v*\\+*'",
        "ubuntu-22.04",
        "macos-15",
        "windows-2022",
        "mcp-repl-x86_64-unknown-linux-gnu.tar.gz",
        "mcp-repl-aarch64-apple-darwin.tar.gz",
        "mcp-repl-x86_64-pc-windows-msvc.zip",
        "SHA256SUMS.txt",
        "gh release upload dev dist/* --clobber",
        "group: publish-dev",
        "gh release create \"${RELEASE_TAG}\" dist/*",
        "github.event_name == 'pull_request'",
        "github.event_name == 'push' && github.ref == 'refs/heads/main'",
        "github.event_name == 'push' && github.ref_type == 'tag'",
        "^v[0-9]+(\\.[0-9]+){2}(-[0-9A-Za-z-]+(\\.[0-9A-Za-z-]+)*)?$",
        "grep -E '^v[0-9]+(\\.[0-9]+){2}$'",
        "sort -V | tail -n 1",
        "-F draft=false",
        "-F prerelease=\"${prerelease_flag}\"",
        "--prerelease",
        "make_latest=\"${latest_flag}\"",
    ] {
        assert!(
            workflow.contains(required),
            "missing {required} in .github/workflows/ci.yml"
        );
    }

    for forbidden in [
        "workflow_dispatch:",
        "stable_tag:",
        "backfill-stable:",
        "- 'v*.*.*'",
        "publish-stable:",
    ] {
        assert!(
            !workflow.contains(forbidden),
            "did not expect {forbidden} in .github/workflows/ci.yml"
        );
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
        "plot_images__multi_panel_plots_emit_single_image.snap",
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
        "plot_images__multi_panel_plots_emit_single_image@transcript.snap",
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
fn multi_panel_plot_snapshots_show_reference_render() {
    let snapshots_dir = repo_root().join("tests/snapshots");
    for name in [
        "plot_images__multi_panel_plots_emit_single_image.snap",
        "plot_images__multi_panel_plots_emit_single_image@macos.snap",
    ] {
        let contents = read(&snapshots_dir.join(name));
        assert!(
            contents.contains("\"data\": \"blake3:<multi_panel_plot>\""),
            "multi-panel plot snapshot should expose the reference placeholder: {name}"
        );
        assert!(
            contents.contains("\"reference\": {"),
            "multi-panel plot snapshot should embed a reference render: {name}"
        );
    }

    for name in [
        "plot_images__multi_panel_plots_emit_single_image@transcript.snap",
        "plot_images__multi_panel_plots_emit_single_image@transcript__macos.snap",
    ] {
        let contents = read(&snapshots_dir.join(name));
        assert!(
            contents.contains("=== reference multi_panel_plot via Rscript --vanilla -"),
            "multi-panel transcript should embed the reference render: {name}"
        );
        assert!(
            contents.contains("blake3:<multi_panel_plot>"),
            "multi-panel transcript should expose the reference placeholder: {name}"
        );
    }
}
