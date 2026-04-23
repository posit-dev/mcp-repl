import json
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path


SCRIPT_PATH = Path(__file__).with_name("diff_composition.py")


def run(cmd, cwd):
    return subprocess.run(
        cmd,
        cwd=cwd,
        check=True,
        capture_output=True,
        text=True,
    )


class DiffCompositionCliTests(unittest.TestCase):
    def setUp(self):
        self.temp_dir = tempfile.TemporaryDirectory()
        self.repo = Path(self.temp_dir.name)
        run(["git", "init"], self.repo)
        run(["git", "config", "user.name", "Test User"], self.repo)
        run(["git", "config", "user.email", "test@example.com"], self.repo)

        self.write(
            "src/lib.rs",
            """
            pub fn answer() -> i32 {
                41
            }

            #[cfg(test)]
            mod tests {
                use super::*;

                #[test]
                fn smoke() {
                    assert_eq!(answer(), 41);
                }
            }
            """,
        )
        self.write(
            "tests/integration.rs",
            """
            #[test]
            fn integration() {
                assert_eq!(2 + 2, 4);
            }
            """,
        )
        self.write("docs/guide.md", "# Guide\n")
        self.write("tests/snapshots/example.snap", "old snapshot\n")
        run(["git", "add", "."], self.repo)
        run(["git", "commit", "-m", "base"], self.repo)
        self.base = run(["git", "rev-parse", "HEAD"], self.repo).stdout.strip()

        self.write(
            "src/lib.rs",
            """
            pub fn answer() -> i32 {
                42
            }

            pub fn doubled() -> i32 {
                answer() * 2
            }

            #[cfg(test)]
            mod tests {
                use super::*;

                #[test]
                fn smoke() {
                    assert_eq!(answer(), 42);
                }

                #[test]
                fn doubled_smoke() {
                    assert_eq!(doubled(), 84);
                }
            }
            """,
        )
        self.write(
            "tests/integration.rs",
            """
            #[test]
            fn integration() {
                assert_eq!(2 + 2, 4);
            }

            #[test]
            fn integration_two() {
                assert_eq!(3 + 3, 6);
            }
            """,
        )
        self.write("docs/guide.md", "# Guide\n\nMore detail.\n")
        self.write("tests/snapshots/example.snap", "new snapshot\n")
        run(["git", "add", "."], self.repo)
        run(["git", "commit", "-m", "head"], self.repo)

    def tearDown(self):
        self.temp_dir.cleanup()

    def write(self, relative_path, contents):
        path = self.repo / relative_path
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(textwrap.dedent(contents).lstrip(), encoding="utf-8")

    def test_json_output_splits_diff_into_expected_categories(self):
        result = run(
            [
                sys.executable,
                str(SCRIPT_PATH),
                "--base",
                self.base,
                "--head",
                "HEAD",
                "--format",
                "json",
            ],
            self.repo,
        )
        summary = json.loads(result.stdout)

        self.assertEqual(summary["totals"]["files"], 4)
        self.assertGreater(summary["categories"]["src_runtime"]["churn"], 0)
        self.assertGreater(summary["categories"]["src_inline_tests"]["churn"], 0)
        self.assertGreater(summary["categories"]["tests"]["churn"], 0)
        self.assertGreater(summary["categories"]["docs"]["churn"], 0)
        self.assertGreater(summary["categories"]["snapshots"]["churn"], 0)

        self.assertEqual(
            summary["totals"]["churn"],
            sum(category["churn"] for category in summary["categories"].values()),
        )
        self.assertEqual(
            [entry["path"] for entry in summary["largest_files"]],
            sorted(
                [entry["path"] for entry in summary["largest_files"]],
                key=lambda path: next(
                    item["churn"]
                    for item in summary["largest_files"]
                    if item["path"] == path
                ),
                reverse=True,
            ),
        )

    def test_markdown_output_is_pr_body_friendly(self):
        result = run(
            [
                sys.executable,
                str(SCRIPT_PATH),
                "--base",
                self.base,
                "--head",
                "HEAD",
                "--format",
                "markdown",
            ],
            self.repo,
        )
        text = result.stdout

        self.assertIn("## Diff composition", text)
        self.assertIn("runtime `src/`", text)
        self.assertIn("inline tests inside `src/`", text)
        self.assertIn("tests in `tests/`", text)
        self.assertIn("docs", text)
        self.assertIn("snapshots", text)
        self.assertIn("Largest files:", text)


if __name__ == "__main__":
    unittest.main()
