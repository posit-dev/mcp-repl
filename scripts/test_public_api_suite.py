import importlib.util
import sys
import unittest
from pathlib import Path


def load_module():
    script_path = Path(__file__).with_name("public_api_suite.py")
    spec = importlib.util.spec_from_file_location("public_api_suite", script_path)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


class PublicApiSuiteCaseTests(unittest.TestCase):
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


if __name__ == "__main__":
    unittest.main()
