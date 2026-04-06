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
