import importlib.util
import sys
import unittest
from pathlib import Path


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

    def test_wait_for_busy_response_text_polls_until_marker(self):
        initial = self.module.tool_result(
            self.module.text(
                "setup started\n"
                "<<repl status: busy, write_stdin timeout reached; elapsed_ms=1>>"
            )
        )
        ready = self.module.tool_result(
            self.module.text(
                "INTERRUPT_READY\n"
                "<<repl status: busy, write_stdin timeout reached; elapsed_ms=2>>"
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


class FakeClientWithoutResponses:
    def repl(self, input_text, *, timeout_ms=None):
        raise AssertionError("unexpected poll")


if __name__ == "__main__":
    unittest.main()
