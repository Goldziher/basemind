"""The harden baseline comparison flags a regression above the threshold and only on a known machine."""

import contextlib
import importlib.util
import io
import json
import tempfile
import unittest
from pathlib import Path
from types import ModuleType

BASELINES = {
    "threshold_pct": 20,
    "min_abs_ms": 500,
    "machines": {
        "box": {"features": "full", "repos": {"big": {"scan_ms": 10000, "git_build_ms": 2000}}},
    },
}


def load_script() -> ModuleType:
    """Load scripts/harden_baseline.py as a module."""
    root = Path(__file__).resolve().parents[2]
    spec = importlib.util.spec_from_file_location("harden_baseline", root / "scripts/harden_baseline.py")
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class CompareTests(unittest.TestCase):
    """A run is judged against the baseline of its own machine class."""

    def setUp(self) -> None:
        self.module = load_script()
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)
        self.baselines = Path(self.dir.name) / "baselines.json"
        self.baselines.write_text(json.dumps(BASELINES))

    def run_main(self, scan_ms: int, build_ms: int, *extra: str, machine: str = "box") -> tuple[int, str]:
        results = Path(self.dir.name) / "results.ndjson"
        record = {"repo_name": "big", "scan_elapsed_ms": scan_ms, "git_history": {"build_ms": build_ms}}
        results.write_text(json.dumps(record) + "\n")
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            status = self.module.main(
                [str(results), "--baselines", str(self.baselines), "--machine", machine, "--features", "full", *extra]
            )
        return status, out.getvalue()

    def test_a_regression_above_twenty_percent_is_flagged(self) -> None:
        status, out = self.run_main(12500, 2000)
        self.assertIn("REGRESSION big: scan 12500 ms vs baseline 10000 ms (+25%", out)
        self.assertEqual(status, 0)

    def test_strict_fails_on_a_regression(self) -> None:
        status, _ = self.run_main(12500, 2000, "--strict")
        self.assertEqual(status, 1)

    def test_ten_percent_is_not_flagged(self) -> None:
        status, out = self.run_main(11000, 2200, "--strict")
        self.assertNotIn("REGRESSION", out)
        self.assertIn("1 of 1 repositories compared", out)
        self.assertEqual(status, 0)

    def test_the_build_time_is_judged_too(self) -> None:
        _, out = self.run_main(10000, 3000)
        self.assertIn("REGRESSION big: git-history build 3000 ms", out)

    def test_a_sub_floor_excess_is_not_flagged(self) -> None:
        baselines = json.loads(self.baselines.read_text())
        baselines["machines"]["box"]["repos"]["big"] = {"scan_ms": 300, "git_build_ms": 60}
        self.baselines.write_text(json.dumps(baselines))
        status, out = self.run_main(600, 120, "--strict")
        self.assertNotIn("REGRESSION", out)
        self.assertEqual(status, 0)

    def test_an_unknown_machine_prints_a_candidate_and_passes(self) -> None:
        status, out = self.run_main(99999, 99999, "--strict", machine="elsewhere")
        self.assertNotIn("REGRESSION", out)
        self.assertIn("not compared", out)
        self.assertEqual(json.loads(out[out.index("{") :])["elsewhere"]["repos"]["big"]["scan_ms"], 99999)
        self.assertEqual(status, 0)

    def test_the_committed_baselines_cover_the_ci_repositories(self) -> None:
        committed = json.loads(self.module.DEFAULT_BASELINES.read_text())
        repos = committed["machines"]["github-ubuntu-latest"]["repos"]
        self.assertEqual(
            sorted(repos), ["django", "gin", "react", "requests", "ripgrep", "ripgrep-shallow", "tokio", "typescript"]
        )


if __name__ == "__main__":
    unittest.main()
