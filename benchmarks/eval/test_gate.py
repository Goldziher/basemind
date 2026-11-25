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
            "read_around": "code_grep",
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
            self.write(d, "a", {"read_around": True}, tool={"read_around": "code_read"})
            with self.assertRaisesRegex(ValueError, "counts against `code_read`"):
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

    def test_the_ratio_column_tells_two_thirds_from_0_67(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            self.write(d, "a", {"find": False})
            report = json.loads((d / "a.report.json").read_text())
            report["gate"]["find"]["median_ratio"] = 2 / 3
            (d / "a.report.json").write_text(json.dumps(report))
            data = gate.load(d)
            md = gate.markdown(data, gate.verdicts(data))
            self.assertIn("| 0.6667 |", md)
            self.assertNotIn("| 0.67 |", md)

    def test_the_binary_line_maps_reported_features_to_the_build_flag(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            self.write(d, "a", {"symbols": True})
            report = json.loads((d / "a.report.json").read_text())
            report["build"]["features"] = ["tokenizer", "code-intel-js", "code-intel-stack"]
            (d / "a.report.json").write_text(json.dumps(report))
            line = gate.binary_line(gate.load(d), [])
            self.assertIn(
                "features `tokenizer,code-intel-js,code-intel-stack` (build flags `tokenizer,code-intel`)", line
            )
            report["build"]["features"] = ["tokenizer", "code-intel-js"]
            (d / "a.report.json").write_text(json.dumps(report))
            line = gate.binary_line(gate.load(d), [])
            self.assertNotIn("build flags", line)

    def test_a_kind_only_the_corpora_of_other_languages_have_is_not_missing(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            self.write(d, "r1", {"symbols": True, "references": True, "callers": True, "dependents": True})
            self.write(d, "r2", {"symbols": True, "references": True, "dependents": True})
            self.write(d, "g", {"symbols": True, "references": True, "dependents": True})
            langs = {"r1": ["rust"], "r2": ["rust"], "g": ["go"]}
            self.assertEqual(gate.verdicts(gate.load(d), langs)["code_refs"]["missing"], "callers@r2")
            self.assertEqual(
                gate.verdicts(gate.load(d))["code_refs"]["missing"],
                "callers@g, callers@r2",
            )

    def test_a_language_alone_in_its_group_still_lacks_the_kinds_it_must_report(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            self.write(d, "r", {"symbols": True, "references": True, "dependents": True})
            self.write(d, "p", {"symbols": True, "dependents": True})
            langs = {"r": ["rust"], "p": ["python"]}
            tools = gate.verdicts(gate.load(d), langs)
            self.assertEqual(tools["code_refs"]["missing"], "references@p")
            self.assertEqual(tools["code_refs"]["verdict"], "no verdict")

    def test_every_corpus_language_must_report_the_structural_kinds_it_has_gold_for(self) -> None:
        cases = {
            "typescript": ("symbols", "references", "dependents"),
            "go": ("symbols", "references", "dependents"),
            "java": ("symbols", "references"),
        }
        for language, required in cases.items():
            for absent in required:
                with self.subTest(language=language, absent=absent), tempfile.TemporaryDirectory() as tmp:
                    d = Path(tmp)
                    self.write(d, "r", {"symbols": True, "references": True, "dependents": True})
                    self.write(d, "x", {kind: True for kind in required if kind != absent})
                    tools = gate.verdicts(gate.load(d), {"r": ["rust"], "x": [language]})
                    kinds = "code_symbols" if absent == "symbols" else "code_refs"
                    self.assertEqual(tools[kinds]["missing"], f"{absent}@x")
                    self.assertEqual(tools[kinds]["verdict"], "no verdict")

    def test_java_needs_no_dependents_gold(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            self.write(d, "r", {"symbols": True, "references": True, "dependents": True})
            self.write(d, "j", {"symbols": True, "references": True})
            langs = {"r": ["rust"], "j": ["java"]}
            tools = gate.verdicts(gate.load(d), langs)
            self.assertEqual(tools["code_refs"]["missing"], "")
            self.assertEqual(tools["code_refs"]["verdict"], "PASS")

    def test_recall_is_reported_per_language(self) -> None:
        def row(task_id: str, mode: str, recall: float | None, base: float | None = None) -> dict:
            out: dict = {"id": task_id, "mode": mode, "tokens": 10}
            if recall is not None:
                out["score"] = {"recall": recall}
            if base is not None:
                out["baseline_score"] = {"recall": base}
            return out

        data = {
            "click": ({}, [row("impact-0000", "impact", 1.0, 0.5), row("impact-0001", "impact", 0.5, 0.5)]),
            "mono": (
                {},
                [
                    row("impact-0000-ts", "impact", 0.25),
                    row("impact-0000-py", "impact", 1.0),
                    row("impact-0001-py", "impact", None),
                    row("find-0000", "find", 1.0),
                ],
            ),
        }
        langs = {"click": ["python"], "mono": ["typescript", "python"]}
        got = gate.recall_by_language(data, langs)
        self.assertEqual(
            got,
            {
                "(whole repository)": {"find": {"tasks": 1, "recall": 1.0, "baseline_recall": None}},
                "python": {"impact": {"tasks": 4, "recall": 0.625, "baseline_recall": 0.5}},
                "typescript": {"impact": {"tasks": 1, "recall": 0.25, "baseline_recall": None}},
            },
        )

    def test_the_markdown_lists_recall_per_language(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            self.write(d, "click", {"impact": True})
            data = gate.load(d)
            md = gate.markdown(data, gate.verdicts(data), None, {"click": ["python"]})
            self.assertIn("| python | impact | 1 | 0.000 | - |", md)

    def test_load_languages_reads_the_sixth_column(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tsv = Path(tmp) / "corpus.tsv"
            tsv.write_text(
                "# header\na\turl\tsha\tsum\t3\tpython\nb\turl\tsha\tsum\t3\tgo,java\nold\turl\tsha\tsum\t3\n"
            )
            self.assertEqual(gate.load_languages(tsv), {"a": ["python"], "b": ["go", "java"]})


if __name__ == "__main__":
    unittest.main()
