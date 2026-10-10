"""Tests for the gold generators (stdlib unittest): `python3 -m unittest discover benchmarks/eval`.

Builds a tiny git repo whose gold is known by construction, runs every generator against it and
checks the emitted tasks, that output is byte-identical across runs and hash seeds, and that the
cross-language guard really drops names that also occur outside the language under test.
"""

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).parent

WIDGET = """class Widget:
    def spin(self):
        return 1


def make_widget():
    return Widget()


LIMIT_VALUE = 3
"""
USE = """from pkg.widget import make_widget


def run_everything():
    w = make_widget()
    return w.spin()
"""
SCOPED = """REGISTRY_SIZE = 4


class Holder:
    slot_count = 2

    def keep(self):
        scratch_value = 1

        def inner_helper():
            return scratch_value

        return inner_helper


def drive():
    local_total = 0
    return local_total
"""
GUIDE = "# Guide\n\nCall `make_widget` from `pkg/use.py` whenever you need a fresh instance to spin.\n"


def git(repo: Path, *args: str) -> None:
    env = {
        **os.environ,
        "GIT_AUTHOR_NAME": "t",
        "GIT_AUTHOR_EMAIL": "t@e.x",
        "GIT_COMMITTER_NAME": "t",
        "GIT_COMMITTER_EMAIL": "t@e.x",
    }
    subprocess.run(["git", "-C", str(repo), *args], check=True, capture_output=True, env=env)


class GeneratorTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls._tmp = tempfile.TemporaryDirectory()
        cls.repo = Path(cls._tmp.name)
        git(cls.repo, "init", "-q")
        git(cls.repo, "config", "commit.gpgsign", "false")
        (cls.repo / "pkg").mkdir()
        (cls.repo / "docs").mkdir()
        (cls.repo / "pkg/widget.py").write_text(WIDGET)
        (cls.repo / "pkg/use.py").write_text(USE)
        (cls.repo / "pkg/scoped.py").write_text(SCOPED)
        git(cls.repo, "add", "-A")
        git(cls.repo, "commit", "-qm", "introduce gizmo pipeline scaffolding")
        (cls.repo / "docs/guide.md").write_text(GUIDE)
        git(cls.repo, "add", "-A")
        git(cls.repo, "commit", "-qm", "document widget factory usage")

    @classmethod
    def tearDownClass(cls) -> None:
        cls._tmp.cleanup()

    def run_gen(self, name: str, *args: str, hashseed: str = "0") -> list[dict]:
        proc = subprocess.run(
            [
                sys.executable,
                str(HERE / f"gen_{name}.py"),
                "--repo",
                str(self.repo),
                "--n",
                "20",
                *args,
            ],
            capture_output=True,
            text=True,
            env={**os.environ, "PYTHONHASHSEED": hashseed},
            check=False,
        )
        self.assertEqual(proc.returncode, 0, proc.stderr)
        return [json.loads(line) for line in proc.stdout.splitlines()]

    def test_symbols_gold_is_every_substring_match_with_lines(self) -> None:
        tasks = self.run_gen("symbols", "--min-len", "4")
        by_name = {t["args"]["name"]: t for t in tasks}
        # `make_widget` also appears in the markdown guide, so the cross-language guard drops it.
        self.assertNotIn("make_widget", by_name)
        self.assertEqual(by_name["Widget"]["gold"], ["pkg/widget.py:1"])
        self.assertEqual(by_name["LIMIT_VALUE"]["gold"], ["pkg/widget.py:10"])
        unguarded = {
            t["args"]["name"]: t for t in self.run_gen("symbols", "--min-len", "4", "--no-cross-language-check")
        }
        self.assertEqual(unguarded["make_widget"]["gold"], ["pkg/widget.py:6"])

    def test_outline_lists_every_symbol_line(self) -> None:
        tasks = self.run_gen("outline", "--min-symbols", "3")
        widget = next(t for t in tasks if t["args"]["path"] == "pkg/widget.py")
        self.assertEqual(
            widget["gold"],
            [
                "pkg/widget.py:1",
                "pkg/widget.py:2",
                "pkg/widget.py:6",
                "pkg/widget.py:10",
            ],
        )

    def test_outline_and_symbols_skip_function_locals(self) -> None:
        tasks = self.run_gen("outline", "--min-symbols", "3")
        scoped = next(t for t in tasks if t["args"]["path"] == "pkg/scoped.py")
        # REGISTRY_SIZE, Holder, slot_count, keep, drive: no scratch_value / inner_helper / local_total.
        self.assertEqual(scoped["gold"], [f"pkg/scoped.py:{n}" for n in (1, 4, 5, 7, 16)])
        names = {t["args"]["name"] for t in self.run_gen("symbols", "--min-len", "5")}
        self.assertTrue({"scratch_value", "inner_helper", "local_total"}.isdisjoint(names))
        self.assertIn("slot_count", names)

    def test_references_are_call_sites_only_and_callers_name_the_definition(
        self,
    ) -> None:
        refs = {t["args"]["name"]: t for t in self.run_gen("references", "--no-cross-language-check")}
        self.assertEqual(refs["make_widget"]["gold"], ["pkg/use.py:5"])  # the import on line 1 is not a call
        self.assertEqual(refs["Widget"]["gold"], ["pkg/widget.py:7"])  # `class Widget:` is a definition, not a call
        callers = self.run_gen("references", "--mode", "callers", "--no-cross-language-check")
        task = next(t for t in callers if t["args"]["name"] == "make_widget")
        self.assertEqual((task["mode"], task["args"]["path"]), ("callers", "pkg/widget.py"))

    def test_dependents_match_import_statement_text(self) -> None:
        tasks = {t["args"]["module"]: t for t in self.run_gen("dependents")}
        self.assertEqual(tasks["pkg.widget"]["gold"], ["pkg/use.py"])

    def test_grep_gold_equals_git_grep_with_the_same_filters(self) -> None:
        tasks = self.run_gen("grep", "--language", "python")
        self.assertTrue(tasks)
        for t in tasks:
            out = subprocess.run(
                [
                    "git",
                    "-C",
                    str(self.repo),
                    "grep",
                    "-n",
                    "-E",
                    t["args"]["pattern"],
                    "HEAD",
                    "--",
                    "*.py",
                ],
                capture_output=True,
                text=True,
                check=False,
            ).stdout
            expected = sorted(f"{p}:{n}" for p, n in (line.split(":")[1:3] for line in out.splitlines()))
            self.assertEqual(sorted(t["gold"]), expected, t["args"])

    def test_find_indexed_list_limits_gold_to_indexed_paths(self) -> None:
        listing = Path(self._tmp.name) / "indexed.txt"
        listing.write_text("pkg/widget.py\ndocs/guide.md\n")
        tasks = self.run_gen("find", "--ext", "", "--indexed", str(listing))
        self.assertTrue(tasks)
        for t in tasks:
            self.assertIn(t["gold"][0], {"pkg/widget.py", "docs/guide.md"})

    def test_find_mutates_real_paths(self) -> None:
        tasks = self.run_gen("find", "--ext", "")
        self.assertTrue(tasks)
        tracked = set(
            subprocess.run(
                ["git", "-C", str(self.repo), "ls-files"],
                capture_output=True,
                text=True,
                check=False,
            ).stdout.split()
        )
        for t in tasks:
            self.assertEqual(t["scoring"], "ranked")
            self.assertIn(t["gold"][0], tracked)
        basename = next(t for t in tasks if t["id"].endswith("0000"))
        self.assertEqual(basename["args"]["query"], basename["gold"][0].rsplit("/", 1)[-1])

    def test_git_search_gold_contains_the_origin_commit(self) -> None:
        tasks = self.run_gen("git_search", "--ext", "")
        self.assertTrue(tasks)
        for t in tasks:
            self.assertIn(t["meta"]["origin"], t["gold"])
            self.assertTrue(t["meta"]["files"], "diff-tree files recorded")
            self.assertFalse(any(tok.isdigit() for tok in t["args"]["query"].split()))

    def test_docs_gold_is_the_markdown_file(self) -> None:
        tasks = self.run_gen("docs", "--min-words", "8")
        self.assertEqual([t["gold"] for t in tasks], [["docs/guide.md"]])
        self.assertNotIn("`", tasks[0]["args"]["query"])

    def test_output_is_deterministic_across_runs_and_hash_seeds(self) -> None:
        for name, extra in [
            ("symbols", ["--min-len", "4"]),
            ("references", []),
            ("find", ["--ext", ""]),
            ("git_search", ["--ext", ""]),
        ]:
            a = self.run_gen(name, *extra, "--seed", "3", hashseed="1")
            b = self.run_gen(name, *extra, "--seed", "3", hashseed="999")
            self.assertEqual(a, b, name)

    def test_exclude_globs_remove_paths(self) -> None:
        tasks = self.run_gen("dependents", "--exclude", "pkg/use.py")
        self.assertEqual(tasks, [])


if __name__ == "__main__":
    unittest.main()
