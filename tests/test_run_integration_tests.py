import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path
from textwrap import dedent
from unittest.mock import patch


def load_module():
    script_path = Path(__file__).with_name("run_integration_tests.py")
    spec = importlib.util.spec_from_file_location("run_integration_tests", script_path)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


class RunIntegrationTestsCaseTests(unittest.TestCase):
    def setUp(self):
        self.module = load_module()

    def test_r_cases_pin_r_interpreter(self):
        for case_name, case in self.module.CASES.items():
            if not case_name.startswith("r-"):
                continue
            with self.subTest(case=case_name):
                self.assertIn("--interpreter", case.server_args)
                index = case.server_args.index("--interpreter")
                self.assertLess(index + 1, len(case.server_args))
                self.assertEqual(case.server_args[index + 1], "r")

    def test_tool_result_builder_matches_mcp_response_shape(self):
        self.assertEqual(
            self.module.tool_result(
                self.module.text("[1] 2\n"),
                self.module.text("> "),
            ),
            {
                "content": [
                    {"type": "text", "text": "[1] 2\n"},
                    {"type": "text", "text": "> "},
                ],
                "isError": False,
            },
        )

    def test_resolve_binary_path_accepts_extensionless_windows_path(self):
        with tempfile.TemporaryDirectory() as temp_dir:
            binary = Path(temp_dir) / "mcp-repl"
            exe_binary = Path(temp_dir) / "mcp-repl.exe"
            exe_binary.write_text("", encoding="utf-8")

            with patch.object(self.module.sys, "platform", "win32"):
                self.assertEqual(exe_binary, self.module.resolve_binary_path(binary))

    def test_wait_for_busy_response_text_polls_until_marker(self):
        initial = self.module.tool_result(
            self.module.text(
                dedent(
                    """\
                    setup started
                    <<repl status: busy, write_stdin timeout reached; elapsed_ms=1>>"""
                )
            )
        )
        ready = self.module.tool_result(
            self.module.text(
                dedent(
                    """\
                    INTERRUPT_READY
                    <<repl status: busy, write_stdin timeout reached; elapsed_ms=2>>"""
                )
            )
        )

        test_case = self

        class FakeClient:
            def repl(self, input_text, *, timeout_ms=None):
                test_case.assertEqual(input_text, "")
                test_case.assertEqual(timeout_ms, 500)
                return ready

        received = self.module.wait_for_busy_response_text(
            FakeClient(),
            initial,
            "INTERRUPT_READY",
            "interrupt setup repl",
            deadline_seconds=1.0,
        )

        self.assertIs(received, ready)

    def test_wait_for_busy_response_text_fails_if_worker_finishes_before_marker(self):
        finished = self.module.tool_result(self.module.text("> "))

        with self.assertRaisesRegex(
            self.module.SuiteFailure,
            "finished before .* marker",
        ):
            self.module.wait_for_busy_response_text(
                FakeClientWithoutResponses(),
                finished,
                "INTERRUPT_READY",
                "interrupt setup repl",
                deadline_seconds=1.0,
            )

    def test_r_interrupt_restart_prefixes_polls_after_transient_busy_interrupt(self):
        initial_busy = self.module.tool_result(
            self.module.text(
                dedent(
                    """\
                    INTERRUPT_READY
                    <<repl status: busy, write_stdin timeout reached; elapsed_ms=1>>"""
                )
            )
        )
        interrupt_busy = self.module.tool_result(
            self.module.text(
                "<<repl status: busy, write_stdin timeout reached; elapsed_ms=2>>"
            )
        )
        interrupted = self.module.tool_result(
            self.module.text("interrupt received\n"),
            self.module.text("AFTER_INTERRUPT\n"),
            self.module.text("> "),
        )
        test_case = self
        self_module = self.module

        class FakeClient:
            def __init__(self):
                self.responses = [
                    (
                        "x <- 1\n",
                        30000,
                        self_module.tool_result(
                            self_module.text("> x <- 1\n"),
                            self_module.text("> "),
                        ),
                    ),
                    (
                        '\u0004print(exists("x"))\n',
                        30000,
                        self_module.tool_result(
                            self_module.text("[repl] new session started\n"),
                            self_module.text('> print(exists("x"))\n[1] FALSE\n'),
                            self_module.text("> "),
                        ),
                    ),
                    (None, 1000, initial_busy),
                    ('\u0003cat("AFTER_INTERRUPT\\n")', 5000, interrupt_busy),
                    ("", 500, interrupted),
                ]

            def repl(self, input_text, *, timeout_ms=None):
                test_case.assertTrue(self.responses, "unexpected repl call")
                expected_input, expected_timeout, result = self.responses.pop(0)
                if expected_input is None:
                    test_case.assertIn("INTERRUPT_READY", input_text)
                else:
                    test_case.assertEqual(expected_input, input_text)
                test_case.assertEqual(expected_timeout, timeout_ms)
                return result

        client = FakeClient()
        self.module.r_interrupt_restart_prefixes(client)
        self.assertEqual([], client.responses)


class FakeClientWithoutResponses:
    def repl(self, input_text, *, timeout_ms=None):
        raise AssertionError("unexpected poll")


if __name__ == "__main__":
    unittest.main()
