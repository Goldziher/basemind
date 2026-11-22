"""Tests for `summarize.py`: the table and the recall floor that fails the `docs-eval` workflow."""

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).parent
sys.path.insert(0, str(HERE))

import summarize  # noqa: E402


def run(recalls: list[float | None]) -> subprocess.CompletedProcess:
    """`summarize.py` over one task per recall; `None` is a call that failed."""
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        tasks = [{"id": f"t{i}", "gold": ["a.md"], "meta": {}} for i in range(len(recalls))]
        results = [
            {"id": f"t{i}", "ok": r is not None, "score": {"recall": r} if r is not None else None, "tokens": 1}
            for i, r in enumerate(recalls)
        ]
        (tmp / "tasks.jsonl").write_text("".join(json.dumps(t) + "\n" for t in tasks))
        (tmp / "results.ndjson").write_text("".join(json.dumps(r) + "\n" for r in results))
        return subprocess.run(
            [sys.executable, "-I", str(HERE / "summarize.py"), str(tmp / "tasks.jsonl"), str(tmp / "results.ndjson")],
            capture_output=True,
            text=True,
            check=False,
        )


class RecallFloor(unittest.TestCase):
    def test_a_run_at_the_floor_passes_and_still_prints_the_table(self) -> None:
        proc = run([summarize.MIN_RECALL_AT_10] * 4)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("| all | 4 |", proc.stdout)

    def test_a_run_under_the_floor_fails_after_printing_the_table(self) -> None:
        proc = run([0.0] * 4)
        self.assertEqual(proc.returncode, 1)
        self.assertIn("| all | 4 |", proc.stdout)
        self.assertIn("recall@10 0.000 is below the floor", proc.stderr)

    def test_a_failed_call_counts_as_recall_0(self) -> None:
        self.assertEqual(run([1.0, None, None, None]).returncode, 1)

    def test_an_empty_run_fails(self) -> None:
        self.assertEqual(run([]).returncode, 1)


if __name__ == "__main__":
    unittest.main()
