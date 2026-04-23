#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path


CATEGORY_ORDER = [
    "src_runtime",
    "src_inline_tests",
    "tests",
    "docs",
    "snapshots",
    "other",
]

CATEGORY_LABELS = {
    "src_runtime": "runtime `src/`",
    "src_inline_tests": "inline tests inside `src/`",
    "tests": "tests in `tests/`",
    "docs": "docs",
    "snapshots": "snapshots",
    "other": "other",
}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Summarize the diff composition between two git revisions, split into "
            "runtime code, inline src tests, tests, docs, and snapshots."
        )
    )
    parser.add_argument("--base", required=True, help="Base git revision")
    parser.add_argument("--head", default="HEAD", help="Head git revision (default: HEAD)")
    parser.add_argument(
        "--repo",
        default=".",
        help="Repository root to inspect (default: current directory)",
    )
    parser.add_argument(
        "--format",
        choices=["text", "markdown", "json"],
        default="text",
        help="Output format (default: text)",
    )
    parser.add_argument(
        "--top",
        type=int,
        default=5,
        help="Number of largest files to report in text/markdown output (default: 5)",
    )
    return parser.parse_args()


def run_git(repo: Path, *args: str) -> str:
    result = subprocess.run(
        ["git", *args],
        cwd=repo,
        check=True,
        capture_output=True,
        text=True,
        errors="replace",
    )
    return result.stdout


def changed_files(repo: Path, base: str, head: str) -> list[str]:
    output = run_git(repo, "diff", "--name-only", f"{base}..{head}")
    return [line for line in output.splitlines() if line]


def file_at(repo: Path, rev: str, path: str) -> list[str]:
    try:
        output = run_git(repo, "show", f"{rev}:{path}")
    except subprocess.CalledProcessError:
        return []
    return output.splitlines()


def test_lines(lines: list[str]) -> set[int]:
    spans: set[int] = set()
    i = 0
    while i < len(lines):
        if re.search(r"\bmod\s+tests\s*\{", lines[i]):
            depth = 0
            opened = False
            for j in range(i, len(lines)):
                for char in lines[j]:
                    if char == "{":
                        depth += 1
                        opened = True
                    elif char == "}":
                        depth -= 1
                spans.add(j + 1)
                if opened and depth == 0:
                    i = j
                    break
        i += 1
    return spans


def classify_path(path: str) -> str:
    if path.startswith("tests/snapshots/"):
        return "snapshots"
    if path.startswith("tests/"):
        return "tests"
    if path.startswith("docs/"):
        return "docs"
    if path.startswith("src/"):
        return "src_runtime"
    return "other"


def classify_src_line(path: str, line_no: int, test_spans: dict[str, set[int]]) -> str:
    if line_no in test_spans.get(path, set()):
        return "src_inline_tests"
    return "src_runtime"


def numstat(repo: Path, base: str, head: str) -> list[tuple[str, int, int]]:
    rows = []
    output = run_git(repo, "diff", "--numstat", f"{base}..{head}")
    for line in output.splitlines():
        parts = line.split("\t")
        if len(parts) != 3:
            continue
        adds, dels, path = parts
        add_count = int(adds) if adds.isdigit() else 0
        del_count = int(dels) if dels.isdigit() else 0
        rows.append((path, add_count, del_count))
    return rows


def category_template() -> dict[str, dict[str, float | int]]:
    return {
        name: {"insertions": 0, "deletions": 0, "churn": 0, "percent": 0.0}
        for name in CATEGORY_ORDER
    }


def summarize(repo: Path, base: str, head: str) -> dict[str, object]:
    files = changed_files(repo, base, head)
    old_tests = {path: test_lines(file_at(repo, base, path)) for path in files if path.startswith("src/")}
    new_tests = {path: test_lines(file_at(repo, head, path)) for path in files if path.startswith("src/")}
    categories = category_template()

    for path in files:
        diff = run_git(repo, "diff", "--unified=0", f"{base}..{head}", "--", path)
        old_line = None
        new_line = None
        for line in diff.splitlines():
            if line.startswith("@@"):
                match = re.search(r"-(\d+)(?:,\d+)? \+(\d+)(?:,\d+)?", line)
                assert match is not None
                old_line = int(match.group(1))
                new_line = int(match.group(2))
                continue
            if line.startswith("---") or line.startswith("+++") or old_line is None or new_line is None:
                continue
            if line.startswith("+"):
                category = classify_path(path)
                if category == "src_runtime":
                    category = classify_src_line(path, new_line, new_tests)
                categories[category]["insertions"] += 1
                new_line += 1
            elif line.startswith("-"):
                category = classify_path(path)
                if category == "src_runtime":
                    category = classify_src_line(path, old_line, old_tests)
                categories[category]["deletions"] += 1
                old_line += 1

    total_insertions = 0
    total_deletions = 0
    total_churn = 0
    for metrics in categories.values():
        metrics["churn"] = metrics["insertions"] + metrics["deletions"]
        total_insertions += int(metrics["insertions"])
        total_deletions += int(metrics["deletions"])
        total_churn += int(metrics["churn"])

    if total_churn:
        for metrics in categories.values():
            metrics["percent"] = round(100.0 * float(metrics["churn"]) / total_churn, 1)

    largest_files = [
        {
            "path": path,
            "insertions": adds,
            "deletions": dels,
            "churn": adds + dels,
        }
        for path, adds, dels in numstat(repo, base, head)
    ]
    largest_files.sort(key=lambda item: (-item["churn"], item["path"]))

    return {
        "base": base,
        "head": head,
        "totals": {
            "files": len(files),
            "insertions": total_insertions,
            "deletions": total_deletions,
            "churn": total_churn,
        },
        "categories": categories,
        "largest_files": largest_files,
    }


def render_text(summary: dict[str, object], top: int) -> str:
    totals = summary["totals"]
    lines = [
        f"Diff composition against {summary['base']}:",
        (
            f"- {totals['files']} files changed, "
            f"{totals['insertions']} insertions(+), {totals['deletions']} deletions(-)"
        ),
    ]
    for category in CATEGORY_ORDER:
        metrics = summary["categories"][category]
        if not metrics["churn"]:
            continue
        lines.append(
            f"- {CATEGORY_LABELS[category]}: +{metrics['insertions']}/-{metrics['deletions']} "
            f"({metrics['percent']:.1f}% of churn)"
        )
    lines.append("- largest files:")
    for item in summary["largest_files"][:top]:
        lines.append(
            f"  - {item['path']}: +{item['insertions']}/-{item['deletions']}"
        )
    return "\n".join(lines) + "\n"


def render_markdown(summary: dict[str, object], top: int) -> str:
    totals = summary["totals"]
    lines = [
        "## Diff composition",
        "",
        (
            f"Measured against `{summary['base']}`, this PR is "
            f"`{totals['insertions']}` insertions and `{totals['deletions']}` deletions "
            f"across `{totals['files']}` files."
        ),
    ]
    for category in CATEGORY_ORDER:
        metrics = summary["categories"][category]
        if not metrics["churn"]:
            continue
        lines.append(
            f"- {CATEGORY_LABELS[category]}: `+{metrics['insertions']}/-{metrics['deletions']}` "
            f"(`{metrics['percent']:.1f}%` of churn)"
        )
    lines.extend(["", "Largest files:"])
    for item in summary["largest_files"][:top]:
        lines.append(
            f"- `{item['path']}`: `+{item['insertions']}/-{item['deletions']}`"
        )
    lines.append("")
    return "\n".join(lines)


def main() -> int:
    args = parse_args()
    repo = Path(args.repo).resolve()
    summary = summarize(repo, args.base, args.head)

    if args.format == "json":
        json.dump(summary, sys.stdout, indent=2)
        sys.stdout.write("\n")
    elif args.format == "markdown":
        sys.stdout.write(render_markdown(summary, args.top))
    else:
        sys.stdout.write(render_text(summary, args.top))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
