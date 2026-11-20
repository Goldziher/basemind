"""The job summary lists a nextest JUnit report's slowest tests, longest first."""

import contextlib
import importlib.util
import io
import tempfile
import unittest
from pathlib import Path
from types import ModuleType

REPORT = """<?xml version="1.0" encoding="UTF-8"?>
<testsuites name="nextest-run" tests="4" failures="1" errors="0">
  <testsuite name="basemind::cli_smoke" tests="2">
    <testcase name="scan_then_query" classname="basemind::cli_smoke" time="12.500"/>
    <testcase name="help_lists_verbs" classname="basemind::cli_smoke" time="0.250"/>
  </testsuite>
  <testsuite name="basemind::daemon_integration" tests="2">
    <testcase name="takeover_watchdog" classname="basemind::daemon_integration" time="95.000">
      <failure type="test failure">timed out</failure>
    </testcase>
    <testcase name="a_tie" classname="basemind::daemon_integration" time="12.500"/>
  </testsuite>
</testsuites>
"""


def load_script() -> ModuleType:
    """Load scripts/nextest_slowest.py as a module."""
    root = Path(__file__).resolve().parents[2]
    spec = importlib.util.spec_from_file_location("nextest_slowest", root / "scripts/nextest_slowest.py")
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class SlowestTests(unittest.TestCase):
    def setUp(self) -> None:
        self.module = load_script()
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.dir = Path(tmp.name)
        self.report = self.dir / "junit.xml"
        self.report.write_text(REPORT, encoding="utf-8")

    def run_main(self, argv: list[str]) -> tuple[int, str, str]:
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = self.module.main(argv)
        return code, out.getvalue(), err.getvalue()

    def test_the_top_tests_are_listed_longest_first_ties_by_name(self) -> None:
        code, out, _ = self.run_main([str(self.report), "--top", "3"])
        self.assertEqual(code, 0)
        self.assertEqual(
            out,
            "### Slowest 3 tests\n"
            "\n"
            "| Seconds | Test |\n"
            "| ---: | --- |\n"
            "| 95.0 | `basemind::daemon_integration takeover_watchdog` |\n"
            "| 12.5 | `basemind::cli_smoke scan_then_query` |\n"
            "| 12.5 | `basemind::daemon_integration a_tie` |\n",
        )

    def test_the_default_lists_twenty_and_a_shorter_report_in_full(self) -> None:
        self.assertEqual(self.module.DEFAULT_TOP, 20)
        code, out, _ = self.run_main([str(self.report)])
        self.assertEqual(code, 0)
        self.assertEqual(
            out.splitlines()[4:],
            [
                "| 95.0 | `basemind::daemon_integration takeover_watchdog` |",
                "| 12.5 | `basemind::cli_smoke scan_then_query` |",
                "| 12.5 | `basemind::daemon_integration a_tie` |",
                "| 0.2 | `basemind::cli_smoke help_lists_verbs` |",
            ],
        )
        self.assertEqual(out.splitlines()[0], "### Slowest 4 tests")

    def test_a_report_without_test_cases_fails(self) -> None:
        empty = self.dir / "empty.xml"
        empty.write_text('<testsuites name="nextest-run" tests="0"/>', encoding="utf-8")
        code, out, err = self.run_main([str(empty)])
        self.assertEqual(code, 1)
        self.assertEqual(out, "")
        self.assertEqual(err, f"{empty}: no test cases\n")


if __name__ == "__main__":
    unittest.main()
