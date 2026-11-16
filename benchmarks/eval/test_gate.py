"""Tests for `gate.py`: the per-tool verdict across corpora, from hand-built reports."""

import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))

import gate  # noqa: E402


class GateSummary(unittest.TestCase):
    def write(self, d: Path, corpus: str, kinds: dict[str, bool | None]) -> None:
        report = {"tokenizer": "o200k", "modes": {}, "gate": {}}
        rows = []
        for kind, verdict in kinds.items():
            report["modes"][kind] = {"errors": 0, "mean_recall": 1.0, "mean_precision": 1.0}
            report["gate"][kind] = {
                "tasks": 10,
                "credited": 10,
                "credited_fraction": 1.0,
                "median_ratio": 2.0 if verdict else 1.0,
                "pass": verdict,
                "reason": "" if verdict else "median 1.00x < 1.5x",
            }
            rows.append({"mode": kind, "tokens": 10, "savings": {"baseline_tokens": 20}})
        (d / f"{corpus}.report.json").write_text(json.dumps(report))
        (d / f"{corpus}.ndjson").write_text("".join(json.dumps(r) + "\n" for r in rows))

    def test_a_tool_fails_when_any_kind_fails_on_any_corpus(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            self.write(d, "a", {"references": True, "callers": True, "implementations": True, "dependents": True})
            self.write(d, "b", {"references": True, "callers": False, "implementations": True, "dependents": True})
            tools = gate.verdicts(gate.load(d))
            self.assertEqual(tools["code_refs"]["verdict"], "FAIL")

    def test_pass_needs_every_kind_on_every_corpus(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            self.write(d, "a", {"blame": True, "blame_symbol": True, "symbols": True})
            self.write(d, "b", {"blame": True, "symbols": True})
            tools = gate.verdicts(gate.load(d))
            self.assertEqual(tools["git_blame"]["verdict"], "no verdict")
            self.assertEqual(tools["git_blame"]["missing"], "blame_symbol@b")
            self.assertEqual(tools["code_symbols"]["verdict"], "PASS")
            self.assertEqual(tools["code_search"]["verdict"], "not run")
            self.write(d, "b", {"blame": None, "blame_symbol": True, "symbols": True})
            self.assertEqual(gate.verdicts(gate.load(d))["git_blame"]["verdict"], "no verdict")


if __name__ == "__main__":
    unittest.main()
