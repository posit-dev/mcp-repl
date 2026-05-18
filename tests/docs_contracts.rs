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

fn normalized_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn assert_contains_wrapped_text(document: &str, required: &str, source: &str) {
    assert!(
        normalized_whitespace(document).contains(&normalized_whitespace(required)),
        "missing {required} in {source}"
    );
}

#[test]
fn agents_is_short_and_points_to_main_docs() {
    let agents = read(&repo_root().join("AGENTS.md"));
    let testing_docs = read(&repo_root().join("docs/testing.md"));
    let tests_readme = read(&repo_root().join("tests/README.md"));
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
        "scripts/diff_composition.py",
        "cargo nextest run --show-progress none",
        "python3 tests/run_integration_tests.py --binary target/debug/mcp-repl",
        "cargo insta test --check",
        "MCP_REPL_CODEX_BACKEND=mock",
    ] {
        assert!(agents.contains(required), "missing {required} in AGENTS.md");
    }

    assert!(
        !agents.contains("cargo nextest run --profile ci --show-progress none"),
        "AGENTS.md should use the local default nextest profile, not the CI filter"
    );
    assert!(
        !agents.contains("tests/run_rust_tests.py"),
        "AGENTS.md should not use the transitional explicit Rust test wrapper"
    );
    assert!(
        !agents.contains("scripts/public_api_suite.py"),
        "AGENTS.md should use tests/run_integration_tests.py"
    );
    for (source, text) in [
        ("AGENTS.md", agents.as_str()),
        ("docs/testing.md", testing_docs.as_str()),
        ("tests/README.md", tests_readme.as_str()),
    ] {
        assert!(
            text.contains("cargo insta test --check"),
            "missing runnable cargo-insta check in {source}"
        );
        assert!(
            !text.contains("cargo insta test --check --unreferenced=reject"),
            "general cargo-insta check should not reject valid platform-specific snapshots in {source}"
        );
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
fn worker_sideband_protocol_keeps_plot_images_one_way() {
    let protocol = read(&repo_root().join("docs/worker_sideband_protocol.md"));

    for required in [
        r#"{ "type": "output_text", "stream": <"stdout"|"stderr">, "data_b64": <base64>, "is_continuation": <bool, optional> }"#,
        r#"{ "type": "plot_image", "mime_type": <string>, "data": <base64>, "is_update": <bool>, "source": <string|null> }"#,
        "There is no plot-image acknowledgement message.",
        "Workers must not delay stdout/stderr output waiting for sideband responses.",
    ] {
        assert!(
            protocol.contains(required),
            "missing {required} in docs/worker_sideband_protocol.md"
        );
    }

    for forbidden in ["`plot_image_ack`", r#""sequence": <integer|null>"#] {
        assert!(
            !protocol.contains(forbidden),
            "did not expect {forbidden} in docs/worker_sideband_protocol.md"
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
        "scripts/diff_composition.py",
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
    ] {
        assert!(readme.contains(required), "missing {required} in README.md");
    }

    for required in [
        "binaries do not bundle R or Python",
        "glibc 2.35+",
        "glibc build produced on Ubuntu 22.04",
        "**Windows**: experimental",
    ] {
        assert_contains_wrapped_text(&readme, required, "README.md");
    }
}

#[test]
fn ci_workflow_defines_dev_release_contract() {
    let workflow = read(&repo_root().join(".github/workflows/ci.yml"));

    for required in [
        "workflow_dispatch:",
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
        "run: python3 tests/run_integration_tests.py --binary target/debug/mcp-repl",
        "run: python tests/run_integration_tests.py --binary target/debug/mcp-repl.exe",
        "npm install -g @openai/codex",
        "npm config get prefix",
        "name: cargo test (real codex integrations)",
        "run: cargo test -j 1 --test codex_approvals_tui -- --test-threads=1",
        "^v[0-9]+(\\.[0-9]+){2}(-[0-9A-Za-z-]+(\\.[0-9A-Za-z-]+)*)?$",
        "grep -E '^v[0-9]+(\\.[0-9]+){2}$'",
        "sort -V | tail -n 1",
        "-F draft=false",
        "-F prerelease=\"${prerelease_flag}\"",
        "--prerelease",
        "-f make_latest=false",
        "-f make_latest=\"${latest_flag}\"",
    ] {
        assert!(
            workflow.contains(required),
            "missing {required} in .github/workflows/ci.yml"
        );
    }

    for forbidden in [
        "stable_tag:",
        "backfill-stable:",
        "- 'v*.*.*'",
        "publish-stable:",
        "-F make_latest=false",
        "-F make_latest=\"${latest_flag}\"",
        "CODEX_VERSION:",
        "openai/codex-action",
        "secrets.OPENAI_API_KEY",
        "codex-x86_64-unknown-linux-musl.tar.gz",
        "codex-aarch64-apple-darwin.tar.gz",
        "codex-x86_64-pc-windows-msvc.exe.zip",
        "https://github.com/openai/codex/releases/latest/download/",
        "Expand-Archive",
        "scripts/public_api_suite.py",
    ] {
        assert!(
            !workflow.contains(forbidden),
            "did not expect {forbidden} in .github/workflows/ci.yml"
        );
    }
}

#[test]
fn ci_runs_codex_integration_with_mock_backend() {
    let root = repo_root();
    let workflow = read(&root.join(".github/workflows/ci.yml"));
    let nextest_config = read(&root.join(".config/nextest.toml"));
    let testing_docs = read(&root.join("docs/testing.md"));
    let codex_integration = read(&root.join("tests/codex_approvals_tui.rs"));
    let claude_integration = read(&root.join("tests/claude_integration.rs"));

    for required in [
        "taiki-e/install-action@nextest",
        "name: cargo nextest (quiet)",
        "run: cargo nextest run --profile ci --show-progress none",
        "name: cargo nextest (quiet, windows serial)",
        "run: cargo nextest run --profile ci --show-progress none --build-jobs 1 --test-threads 1",
        "name: Install Codex CLI",
        "name: Install Codex CLI (windows)",
        "npm install -g @openai/codex",
        "name: cargo test (real codex integrations)",
        "name: cargo test (real codex integrations, windows serial)",
        "run: cargo test -j 1 --test codex_approvals_tui -- --test-threads=1",
    ] {
        assert!(
            workflow.contains(required),
            "missing {required} in .github/workflows/ci.yml"
        );
    }

    for forbidden in [
        "name: cargo test\n        if: matrix.os != 'windows-2022'\n        run: cargo test",
        "name: cargo test (windows serial)\n        if: matrix.os == 'windows-2022'\n        run: cargo test -j 1 -- --test-threads=1",
        "tests/run_rust_tests.py",
    ] {
        assert!(
            !workflow.contains(forbidden),
            "did not expect {forbidden} in .github/workflows/ci.yml"
        );
    }

    for required in [
        "[profile.default]",
        "[profile.ci]",
        "[[profile.default.overrides]]",
        "success-output = \"immediate\"",
        "default-filter = \"not binary(=codex_approvals_tui) and not binary(=claude_integration)\"",
        "status-level = \"fail\"",
        "final-status-level = \"fail\"",
        "success-output = \"never\"",
        "failure-output = \"final\"",
    ] {
        assert!(
            nextest_config.contains(required),
            "missing {required} in .config/nextest.toml"
        );
    }
    assert_eq!(
        nextest_config.matches("default-filter = ").count(),
        1,
        "only the CI profile should filter out real client integrations"
    );
    assert!(
        !nextest_config.contains("repl-integration"),
        "did not expect stale serial nextest configuration repl-integration"
    );
    for required in [
        "[test-groups]",
        "interrupt-integration = { max-threads = 1 }",
        "filter = 'binary(=interrupt)'",
        "test-group = \"interrupt-integration\"",
    ] {
        assert!(
            nextest_config.contains(required),
            "missing {required} in .config/nextest.toml"
        );
    }

    assert_contains_wrapped_text(
        &testing_docs,
        "It opts the interrupt binary into a one-at-a-time group because those tests coordinate through process-local fixtures.",
        "docs/testing.md",
    );

    assert_contains_wrapped_text(
        &testing_docs,
        "The default local profile includes real client integration binaries.",
        "docs/testing.md",
    );
    assert_contains_wrapped_text(
        &testing_docs,
        "The CI profile excludes real client integration binaries from the ordinary Rust suite. CI installs Codex and runs `codex_approvals_tui` separately against a mocked model provider.",
        "docs/testing.md",
    );
    assert_contains_wrapped_text(
        &testing_docs,
        "Codex uses the Spark model (`gpt-5.3-codex-spark`) in its isolated test config. Claude uses `haiku`.",
        "docs/testing.md",
    );
    assert_contains_wrapped_text(
        &testing_docs,
        "The Codex CI integration does not require OpenAI authentication because the test config points Codex at a local mock provider.",
        "docs/testing.md",
    );
    assert_contains_wrapped_text(
        &testing_docs,
        "By default, the Codex integration uses `MCP_REPL_CODEX_BACKEND=auto`: it checks whether Codex is logged in, checks whether `gpt-5.3-codex-spark` is available, and uses that live backend when both checks pass. Otherwise it uses the mocked provider.",
        "docs/testing.md",
    );
    assert_contains_wrapped_text(
        &testing_docs,
        "Set `MCP_REPL_CODEX_BACKEND=live` or `MCP_REPL_CODEX_BACKEND=mock` to force one path.",
        "docs/testing.md",
    );
    assert!(
        testing_docs.contains("MCP_REPL_CODEX_BACKEND=mock cargo test -j 1 --test codex_approvals_tui codex_exec_auto_backend_smoke -- --test-threads=1"),
        "docs/testing.md should show the forced mock Codex backend check"
    );
    assert!(
        testing_docs.contains("MCP_REPL_CODEX_BACKEND=live cargo test -j 1 --test codex_approvals_tui codex_exec_auto_backend_smoke -- --test-threads=1"),
        "docs/testing.md should show the forced live Codex backend check"
    );
    assert_contains_wrapped_text(
        &testing_docs,
        "CI runs the Codex integration binary; Claude integration remains local because provider authentication is unavailable in CI.",
        "docs/testing.md",
    );
    assert!(
        !testing_docs.contains(
            "CI does not run these binaries because provider authentication is unavailable"
        ),
        "docs/testing.md should not claim CI skips the Codex integration binary"
    );
    assert_contains_wrapped_text(
        &testing_docs,
        "If a required client binary is unavailable, the matching integration test prints a skip banner with the reason. Codex backend selection prints a `CODEX` banner showing whether the test selected live Spark or the mocked provider.",
        "docs/testing.md",
    );
    assert!(
        codex_integration.contains(r#"const CODEX_MODEL: &str = "gpt-5.3-codex-spark";"#),
        "Codex integration should use the Spark model"
    );
    assert!(
        codex_integration.contains("requires_openai_auth = false"),
        "Codex integration should use the mocked provider without OpenAI auth"
    );
    assert!(
        codex_integration.contains("MCP_REPL_CODEX_BACKEND"),
        "Codex integration should expose an env var to force live or mock backend selection"
    );
    assert!(
        codex_integration.contains("codex")
            && codex_integration.contains("login")
            && codex_integration.contains("status"),
        "Codex auto backend selection should probe login status"
    );
    assert!(
        codex_integration.contains("debug") && codex_integration.contains("models"),
        "Codex auto backend selection should inspect available models"
    );
    assert!(
        claude_integration.contains(r#"const CLAUDE_MODEL: &str = "haiku";"#),
        "Claude integration should use the fastest/cheapest model"
    );
}

#[test]
fn cargo_test_default_discovers_existing_rust_tests() {
    let root = repo_root();
    let manifest = read(&root.join("Cargo.toml"));
    let testing_docs = read(&root.join("docs/testing.md"));

    assert!(
        !manifest.contains("autotests"),
        "Cargo.toml should not opt Rust integration tests out of default cargo test"
    );
    assert_contains_wrapped_text(
        &testing_docs,
        "Plain `cargo test` remains the full Cargo compatibility path. It must continue to discover the binary unit tests and Rust integration targets.",
        "docs/testing.md",
    );
    assert!(
        !manifest.contains("test = false"),
        "Cargo.toml should not disable Cargo test discovery for existing targets"
    );
    assert_contains_wrapped_text(
        &testing_docs,
        "Do not opt Rust test targets out of Cargo discovery in anticipation of a future Python migration; migrate a scenario only when the Rust coverage is deleted or reduced in the same change that adds equivalent external coverage.",
        "docs/testing.md",
    );
}

#[test]
fn release_backfill_workflow_defines_manual_tag_publish_contract() {
    let workflow = read(&repo_root().join(".github/workflows/release-backfill.yml"));

    for required in [
        "workflow_dispatch:",
        "release_tag:",
        "Existing semver tag to publish, for example v0.1.0",
        "required: true",
        "type: string",
        "ubuntu-22.04",
        "macos-15",
        "windows-2022",
        "ref: ${{ env.RELEASE_TAG }}",
        "^v[0-9]+(\\.[0-9]+){2}(-[0-9A-Za-z-]+(\\.[0-9A-Za-z-]+)*)?$",
        "mcp-repl-x86_64-unknown-linux-gnu.tar.gz",
        "mcp-repl-aarch64-apple-darwin.tar.gz",
        "mcp-repl-x86_64-pc-windows-msvc.zip",
        "SHA256SUMS.txt",
        "gh release upload \"${RELEASE_TAG}\" dist/* --clobber",
        "gh release create \"${RELEASE_TAG}\" dist/*",
        "--generate-notes",
        "--prerelease",
        "-f make_latest=\"${latest_flag}\"",
    ] {
        assert!(
            workflow.contains(required),
            "missing {required} in .github/workflows/release-backfill.yml"
        );
    }

    for forbidden in [
        "branches:",
        "publish-dev:",
        "group: publish-dev",
        "refs/heads/main",
        "github.event_name == 'push'",
        "name: cargo check",
        "name: cargo build\n        run: cargo build",
        "name: cargo clippy",
        "name: cargo test (skip client integrations)",
        "name: cargo test (windows serial, skip client integrations)",
        "name: cargo +nightly fmt",
        "Install nightly rustfmt",
    ] {
        assert!(
            !workflow.contains(forbidden),
            "did not expect {forbidden} in .github/workflows/release-backfill.yml"
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
