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

fn markdown_links(text: &str) -> Vec<String> {
    let mut links = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 3 < bytes.len() {
        if bytes[i] == b']' && bytes[i + 1] == b'(' {
            let start = i + 2;
            if let Some(end_rel) = text[start..].find(')') {
                links.push(text[start..start + end_rel].to_string());
                i = start + end_rel + 1;
                continue;
            }
        }
        i += 1;
    }
    links
}

fn has_heading(text: &str, heading: &str) -> bool {
    text.lines().any(|line| line.trim() == heading)
}

#[test]
fn agents_points_to_required_entrypoints_and_stays_short() {
    let agents_path = repo_root().join("AGENTS.md");
    let agents = read(&agents_path);

    assert!(
        agents.lines().count() <= 120,
        "AGENTS.md should stay at 120 lines or less"
    );

    for link in [
        "docs/index.md",
        "docs/architecture.md",
        "docs/testing.md",
        "docs/debugging.md",
        "docs/sandbox.md",
        "docs/plans/README.md",
    ] {
        assert!(
            agents.contains(link),
            "AGENTS.md should link to {link}, got:\n{agents}"
        );
    }
}

#[test]
fn docs_index_lists_normative_docs_and_classifies_special_areas() {
    let root = repo_root();
    let docs_index_path = root.join("docs/index.md");
    let docs_index = read(&docs_index_path);

    for relative in [
        "docs/architecture.md",
        "docs/testing.md",
        "docs/debugging.md",
        "docs/sandbox.md",
        "docs/worker_sideband_protocol.md",
        "docs/tool-descriptions/repl_tool.md",
        "docs/tool-descriptions/repl_tool_r.md",
        "docs/tool-descriptions/repl_tool_python.md",
        "docs/tool-descriptions/repl_reset_tool.md",
        "docs/plans/README.md",
    ] {
        let path = root.join(relative);
        assert_exists(&path);
        assert!(
            docs_index.contains(relative),
            "docs/index.md should list {relative}"
        );
    }

    for relative in ["docs/notes/", "docs/futurework/", "eval/inspect_swe/"] {
        assert!(
            docs_index.contains(relative),
            "docs/index.md should classify {relative}"
        );
    }
}

#[test]
fn plan_directories_exist_and_plan_docs_match_template() {
    let root = repo_root();
    let plans_root = root.join("docs/plans");
    let active_dir = plans_root.join("active");
    let completed_dir = plans_root.join("completed");
    let tech_debt_path = plans_root.join("tech-debt.md");

    assert_exists(&plans_root);
    assert_exists(&active_dir);
    assert_exists(&completed_dir);
    assert_exists(&tech_debt_path);

    let readme = read(&plans_root.join("README.md"));
    for heading in ["## When to Write a Plan", "## Template", "## Lifecycle"] {
        assert!(has_heading(&readme, heading), "missing heading {heading}");
    }

    for dir in [active_dir, completed_dir] {
        let mut saw_markdown = false;
        for entry in fs::read_dir(&dir)
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", dir.display()))
        {
            let entry = entry.unwrap_or_else(|err| panic!("failed to read dir entry: {err}"));
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                continue;
            }
            saw_markdown = true;
            let text = read(&path);
            for heading in ["## Summary", "## Status", "## Decision Log"] {
                assert!(
                    has_heading(&text, heading),
                    "{} is missing heading {heading}",
                    path.display()
                );
            }
        }
        assert!(
            saw_markdown,
            "expected at least one markdown file in {}",
            dir.display()
        );
    }
}

#[test]
fn normative_docs_do_not_use_stale_mcp_console_name() {
    let root = repo_root();
    let normative_paths = [
        "AGENTS.md",
        "README.md",
        "docs/index.md",
        "docs/architecture.md",
        "docs/testing.md",
        "docs/debugging.md",
        "docs/sandbox.md",
        "docs/worker_sideband_protocol.md",
        "docs/plans/README.md",
        "docs/plans/tech-debt.md",
        "docs/tool-descriptions/repl_tool.md",
        "docs/tool-descriptions/repl_tool_r.md",
        "docs/tool-descriptions/repl_tool_python.md",
        "docs/tool-descriptions/repl_reset_tool.md",
    ];

    for relative in normative_paths {
        let path = root.join(relative);
        assert_exists(&path);
        let text = read(&path);
        assert!(
            !text.contains("mcp-console"),
            "{} still contains stale mcp-console references",
            path.display()
        );
    }
}

#[test]
fn docs_index_links_resolve_inside_repo() {
    let docs_index_path = repo_root().join("docs/index.md");
    let docs_index = read(&docs_index_path);
    let docs_dir = docs_index_path
        .parent()
        .expect("docs/index.md should have a parent");

    for link in markdown_links(&docs_index) {
        if link.starts_with("http://")
            || link.starts_with("https://")
            || link.starts_with('#')
            || link.starts_with("mailto:")
        {
            continue;
        }

        let normalized = link.strip_prefix("./").unwrap_or(&link);
        let path = docs_dir.join(normalized);
        assert!(path.exists(), "broken link in docs/index.md: {link}");
    }
}
