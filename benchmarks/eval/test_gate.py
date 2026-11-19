"""Tests for `gate.py`: the per-tool verdict across corpora, from hand-built reports."""

import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))

import gate  # noqa: E402


class GateSummary(unittest.TestCase):
    def write(self, d: Path, corpus: str, kinds: dict[str, bool | None], tool: dict[str, str] | None = None) -> None:
        report = {
            "tokenizer": "o200k",
            "build": {"version": "0.34.0", "build_id": "abc123def456", "features": ["tokenizer", "code-intel-js"]},
            "modes": {},
            "gate": {},
        }
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
                "tool": (tool or {}).get(kind, gate.TOOL_OF.get(kind, "?")),
                "job": "locating",
                "baseline_failed": 0,
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

    def test_a_failing_analysis_kind_fails_its_tool(self) -> None:
        owners = {
            "impact": "code_refs",
            "cycles": "code_refs",
            "grep_changed": "code_grep",
            "find_large": "find_files",
            "read_around": "code_read",
            "explain": "code_explain",
        }
        for kind, tool in owners.items():
            with self.subTest(kind=kind), tempfile.TemporaryDirectory() as tmp:
                d = Path(tmp)
                siblings = dict.fromkeys(gate.TOOLS[tool], True)
                self.write(d, "a", siblings)
                self.write(d, "b", {**siblings, kind: False})
                tools = gate.verdicts(gate.load(d))
                self.assertEqual(tools[tool]["verdict"], "FAIL")
                self.assertIn(f"| `{tool}` | {kind} | locating | b |", gate.markdown(gate.load(d), tools))
                self.write(d, "b", siblings)
                self.assertEqual(gate.verdicts(gate.load(d))[tool]["verdict"], "PASS")

    def test_a_kind_no_tool_lists_or_listed_under_another_tool_is_an_error(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            self.write(d, "a", {"symbols": True, "brand_new_kind": False})
            with self.assertRaisesRegex(ValueError, "brand_new_kind"):
                gate.verdicts(gate.load(d))
            self.write(d, "a", {"read_around": True}, tool={"read_around": "code_grep"})
            with self.assertRaisesRegex(ValueError, "counts against `code_grep`"):
                gate.verdicts(gate.load(d))

    def test_the_report_names_the_binary_commit_and_features(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            self.write(d, "a", {"symbols": True})
            (d / "run.tsv").write_text("corpus\tsource_commit\tbinary\na\tf5fcf1b4-dirty\tbasemind 0.34.0\n")
            data = gate.load(d)
            md = gate.markdown(data, gate.verdicts(data), gate.load_run(d))
            self.assertIn("source commit `f5fcf1b4-dirty`", md)
            self.assertIn("build `abc123def456`", md)
            self.assertIn("features `tokenizer,code-intel-js`", md)


if __name__ == "__main__":
    unittest.main()
