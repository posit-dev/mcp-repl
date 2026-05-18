#!/usr/bin/env python3
from __future__ import annotations

import argparse
import subprocess
import sys
from collections.abc import Sequence


LIVE_CLIENT_TARGETS = {
    "claude_integration",
    "codex_approvals_tui",
}

INTEGRATION_TARGETS = (
    "claude_integration",
    "codex_approvals_tui",
    "debug_events_env",
    "debug_events_tool_calls",
    "debug_repl_prompt",
    "docs_contracts",
    "install_dual_backend",
    "install_shell_script",
    "interrupt",
    "manage_session_behavior",
    "mcp_transcripts",
    "oversized_output_cli",
    "pager",
    "pager_flags",
    "pager_hits_seek",
    "pager_page_size",
    "pager_seek",
    "pager_skip",
    "pager_where",
    "plot_images",
    "python_backend",
    "python_help_snapshots",
    "python_plot_images",
    "python_program_selection",
    "r_console_encoding",
    "r_file_show",
    "r_help",
    "r_manuals",
    "r_protocol",
    "r_startup",
    "r_vignettes",
    "refactor_coverage",
    "repl_surface",
    "reticulate_py_help",
    "sandbox",
    "sandbox_state_updates",
    "server_smoke",
    "session_endings",
    "windows_suite_server_lock",
    "worker_ipc_disconnect",
    "write_stdin_batch",
    "write_stdin_behavior",
    "write_stdin_edge_cases",
    "zod_protocol",
)


def parse_args(argv: Sequence[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run explicit Rust integration test targets with nextest."
    )
    parser.add_argument(
        "--profile",
        default="default",
        choices=("default", "ci"),
        help="nextest profile to use",
    )
    parser.add_argument(
        "--build-jobs",
        type=int,
        help="value forwarded to cargo nextest run --build-jobs",
    )
    parser.add_argument(
        "--test-threads",
        type=int,
        help="value forwarded to cargo nextest run --test-threads",
    )
    parser.add_argument(
        "--target",
        action="append",
        choices=INTEGRATION_TARGETS,
        help="integration test target to run; repeat to select multiple targets",
    )
    parser.add_argument(
        "--clippy",
        action="store_true",
        help="lint explicit integration targets instead of running them",
    )
    return parser.parse_args(argv)


def selected_targets(profile: str, requested: Sequence[str] | None) -> list[str]:
    targets = list(requested) if requested is not None else list(INTEGRATION_TARGETS)
    if profile == "ci":
        targets = [target for target in targets if target not in LIVE_CLIENT_TARGETS]
    return targets


def main(argv: Sequence[str]) -> int:
    args = parse_args(argv)

    if args.clippy:
        command = ["cargo", "clippy", "--all-features", "--bin", "mcp-repl", "--tests"]
        targets = selected_targets("default", args.target)
    else:
        command = [
            "cargo",
            "nextest",
            "run",
            "--profile",
            args.profile,
            "--show-progress",
            "none",
            "--bin",
            "mcp-repl",
        ]
        if args.build_jobs is not None:
            command.extend(["--build-jobs", str(args.build_jobs)])
        if args.test_threads is not None:
            command.extend(["--test-threads", str(args.test_threads)])
        targets = selected_targets(args.profile, args.target)

    for target in targets:
        command.extend(["--test", target])
    if args.clippy:
        command.extend(["--", "-D", "warnings"])

    return subprocess.run(command).returncode


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
