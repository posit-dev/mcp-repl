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
        "cargo test --quiet",
        "python3 tests/run_integration_tests.py --binary target/debug/mcp-repl",
        "cargo insta test --check",
        "MCP_REPL_CODEX_BACKEND=mock",
    ] {
        assert!(agents.contains(required), "missing {required} in AGENTS.md");
    }

    assert!(
        !agents.contains("cargo nextest"),
        "AGENTS.md should use cargo test as the Rust test runner"
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
fn worker_sideband_protocol_keeps_images_one_way() {
    let protocol = read(&repo_root().join("docs/worker_sideband_protocol.md"));

    for required in [
        r#"{ "type": "output_text", "stream": <"stdout"|"stderr">, "data_b64": <base64>, "is_continuation": <bool, optional> }"#,
        r#"{ "type": "output_image", "mime_type": <string>, "data_b64": <base64>, "is_update": <bool>, "source": <string|null> }"#,
        r#"{ "type": "worker_ready", "protocol": { "name": "mcp-repl-worker", "version": 6 }, "worker": { "name": <string>, "version": <string> }, "capabilities": { "images": <bool> } }"#,
        r#"{ "type": "input_batch", "input": <string> }"#,
        r#"{ "type": "input_line", "prompt": <string>, "text": <string> }"#,
        r#"{ "type": "input_wait", "prompt": <string> }"#,
        r#"{ "type": "ready" }"#,
        r#"{ "type": "session_end", "reason": <string>, "message": <string, optional> }"#,
        r#"{ "type": "interrupt" }"#,
        "This document defines worker protocol version 6.",
        "The server rejects unsupported",
        "There is no image acknowledgement message.",
        "Worker-owned runtime output must be emitted only on sideband IPC",
        "Workers must not delay unowned raw",
        "Submitted input must not be emitted as `output_text`.",
        "The server may reconstruct `prompt + text` from ordered `input_line` events",
    ] {
        assert!(
            protocol.contains(required),
            "missing {required} in docs/worker_sideband_protocol.md"
        );
    }

    for forbidden in [
        r#"{ "type": "plot_image", "mime_type": <string>, "data": <base64>, "is_update": <bool>, "source": <string|null> }"#,
        r#"{ "type": "output_image", "image_id": <string>, "mime_type": <string>, "data_b64": <base64>, "update": <bool> }"#,
        "`plot_image_ack`",
        r#""sequence": <integer|null>"#,
        r#"{ "type": "readline_start", "prompt": <string> }"#,
        r#"{ "type": "input_batch", "input_id": <integer>, "input": <string> }"#,
        r#"{ "type": "input_line", "input_id": <integer>, "prompt": <string>, "text": <string> }"#,
        r#"{ "type": "input_wait", "input_id": <integer>, "prompt": <string> }"#,
        r#"{ "type": "interrupt", "input_id": <integer> }"#,
        r#"{ "type": "idle", "input_id": <integer>, "prompt": <string> }"#,
        r#"{ "type": "stdin_wait", "input_id": <integer>, "prompt": <string> }"#,
        r#"{ "type": "turn_input", "input_id": <integer>, "input": <string> }"#,
        "version 2 from built-in and migrating workers",
        "Legacy v2 image event retained for built-in workers during migration.",
    ] {
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
fn readme_documents_release_install_contract() {
    let readme = read(&repo_root().join("README.md"));

    for required in [
        "pipx install posit-mcp-repl",
        "uv tool install posit-mcp-repl",
        "uvx --from posit-mcp-repl mcp-repl",
        "https://raw.githubusercontent.com/posit-dev/mcp-repl/main/scripts/install.sh",
        "https://raw.githubusercontent.com/posit-dev/mcp-repl/main/scripts/install.ps1",
        "https://github.com/posit-dev/mcp-repl/releases/latest",
        "https://github.com/posit-dev/mcp-repl/releases/latest/download/mcp-repl-x86_64-unknown-linux-gnu.tar.gz",
        "https://github.com/posit-dev/mcp-repl/releases/latest/download/mcp-repl-aarch64-apple-darwin.tar.gz",
        "https://github.com/posit-dev/mcp-repl/releases/latest/download/mcp-repl-x86_64-pc-windows-msvc.zip",
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

    for forbidden in [
        "rolling `dev` prerelease",
        "Append `--dev`",
        "Download prebuilt dev binaries",
        "releases/download/dev",
    ] {
        assert!(
            !readme.contains(forbidden),
            "did not expect {forbidden} in README.md"
        );
    }
}

#[test]
fn pyproject_defines_pypi_binary_package() {
    let pyproject = read(&repo_root().join("pyproject.toml"));

    for required in [
        "[build-system]",
        "requires = [\"maturin>=1.11,<2\"]",
        "build-backend = \"maturin\"",
        "name = \"posit-mcp-repl\"",
        "dynamic = [\"version\"]",
        "readme = \"README.md\"",
        "license = \"Apache-2.0\"",
        "bindings = \"bin\"",
        "strip = true",
    ] {
        assert!(
            pyproject.contains(required),
            "missing {required} in pyproject.toml"
        );
    }
}

#[test]
fn ci_workflow_defines_tag_release_and_pypi_contract() {
    let workflow = read(&repo_root().join(".github/workflows/ci.yml"));

    for required in [
        "workflow_dispatch:",
        "publish-release:",
        "publish-pypi:",
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
        "PyO3/maturin-action@v1",
        "maturin-version: v1.11.5",
        "Build PyPI wheel",
        "Smoke test PyPI wheel",
        "Upload PyPI wheel artifact",
        "pypi-wheel-${{ matrix.target }}",
        "pattern: pypi-wheel-*",
        "pypa/gh-action-pypi-publish@release/v1",
        "id-token: write",
        "url: https://pypi.org/p/posit-mcp-repl",
        "packages-dir: dist",
        "gh release create \"${RELEASE_TAG}\" dist/*",
        "github.event_name == 'pull_request'",
        "github.event_name == 'push' && github.ref_type == 'tag'",
        "run: python3 tests/run_integration_tests.py --binary target/debug/mcp-repl",
        "run: python tests/run_integration_tests.py --binary target/debug/mcp-repl.exe",
        "npm install -g @openai/codex",
        "npm config get prefix",
        "name: cargo test",
        "run: cargo test --quiet",
        "MCP_REPL_CODEX_BACKEND: mock",
        "^v[0-9]+(\\.[0-9]+){2}(-[0-9A-Za-z-]+(\\.[0-9A-Za-z-]+)*)?$",
        "-F draft=false",
        "-F prerelease=\"${prerelease_flag}\"",
        "--prerelease",
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
        "publish-dev:",
        "group: publish-dev",
        "publish_dev",
        "Force-move dev tag",
        "Create or update dev prerelease",
        "Upload dev assets",
        "gh release upload dev dist/* --clobber",
        "git tag -f dev",
        "git push origin refs/tags/dev --force",
        "releases/tags/dev",
        "workflow_runs_json",
        "refs/heads/main",
        "grep -E '^v[0-9]+(\\.[0-9]+){2}$'",
        "sort -V | tail -n 1",
        "-F make_latest=false",
        "CODEX_VERSION:",
        "openai/codex-action",
        "secrets.OPENAI_API_KEY",
        "codex-x86_64-unknown-linux-musl.tar.gz",
        "codex-aarch64-apple-darwin.tar.gz",
        "codex-x86_64-pc-windows-msvc.exe.zip",
        "https://github.com/openai/codex/releases/latest/download/",
        "Expand-Archive",
        "scripts/public_api_suite.py",
        "cargo-nextest",
        "taiki-e/install-action@nextest",
        "cargo nextest",
        "profile ci",
        ".config/nextest.toml",
        "name: cargo test (windows serial)",
        "run: cargo test -j 1 --quiet -- --test-threads=1",
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
    let testing_docs = read(&root.join("docs/testing.md"));
    let codex_integration = read(&root.join("tests/codex_integration.rs"));
    let claude_integration = read(&root.join("tests/claude_integration.rs"));

    for required in [
        "name: Install Codex CLI",
        "name: Install Codex CLI (windows)",
        "npm install -g @openai/codex",
        "name: cargo test",
        "run: cargo test --quiet -- --test-threads=5",
        "MCP_REPL_CODEX_BACKEND: mock",
    ] {
        assert!(
            workflow.contains(required),
            "missing {required} in .github/workflows/ci.yml"
        );
    }

    for forbidden in [
        "taiki-e/install-action@nextest",
        "cargo nextest",
        "name: cargo test (real codex integrations)",
        "run: cargo test -j 1 --test codex_integration -- --test-threads=1",
        "name: cargo test (windows serial)",
        "run: cargo test -j 1 --quiet -- --test-threads=1",
        "tests/run_rust_tests.py",
    ] {
        assert!(
            !workflow.contains(forbidden),
            "did not expect {forbidden} in .github/workflows/ci.yml"
        );
    }

    assert!(
        !root.join(".config/nextest.toml").exists(),
        "the repo should use cargo test directly rather than nextest config"
    );

    assert_contains_wrapped_text(
        &testing_docs,
        "The Rust suite uses plain `cargo test` as its single runner.",
        "docs/testing.md",
    );
    assert_contains_wrapped_text(
        &testing_docs,
        "CI passes Cargo's `--quiet` flag to keep successful logs compact and caps the Rust test harness at five threads so integration tests do not oversubscribe worker-backed REPL sessions on hosted runners.",
        "docs/testing.md",
    );
    assert_contains_wrapped_text(
        &testing_docs,
        "CI uses the same capped Cargo scheduling on Linux, macOS, and Windows by running `cargo test --quiet -- --test-threads=5` for every matrix target.",
        "docs/testing.md",
    );
    assert_contains_wrapped_text(
        &testing_docs,
        "CI installs Codex before `cargo test` and sets `MCP_REPL_CODEX_BACKEND=mock`, so the Codex integration target runs through the mocked provider as part of the ordinary Rust suite.",
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
        testing_docs.contains("MCP_REPL_CODEX_BACKEND=mock cargo test -j 1 --test codex_integration codex_exec_auto_backend_smoke -- --test-threads=1"),
        "docs/testing.md should show the forced mock Codex backend check"
    );
    assert!(
        testing_docs.contains("MCP_REPL_CODEX_BACKEND=live cargo test -j 1 --test codex_integration codex_exec_auto_backend_smoke -- --test-threads=1"),
        "docs/testing.md should show the forced live Codex backend check"
    );
    assert_contains_wrapped_text(
        &testing_docs,
        "CI runs the Codex integration target as part of `cargo test`; Claude integration remains local because provider authentication is unavailable in CI.",
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
fn release_backfill_workflow_is_removed() {
    assert!(
        !repo_root()
            .join(".github/workflows/release-backfill.yml")
            .exists(),
        "manual release backfill workflow should not exist"
    );
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
