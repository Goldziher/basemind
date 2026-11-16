"""Tests for the Rust and git gold generators.

A throwaway git repository with known Rust structure and history is built once; every expected
value below is worked out by hand from the fixture, never from basemind. `rust_gold` is built
offline from the cargo cache on first use (set `BASEMIND_EVAL_RUST_GOLD` to reuse a binary).
"""

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).parent

LIB_V1 = """pub trait Spin {
    fn spin_up(&self) -> u8;
}

pub struct Widget;

impl Spin for Widget {
    fn spin_up(&self) -> u8 {
        1
    }
}
"""
# Second commit: `make_widget` and a Display impl are added; `spin_up` keeps its body.
LIB_V2 = (
    LIB_V1
    + """
impl std::fmt::Display for Widget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "w")
    }
}

/// Builds one.
pub fn make_widget() -> Widget {
    Widget
}

pub fn make_widget_pair() -> (Widget, Widget) {
    (make_widget(), make_widget())
}
"""
)
USE = """use crate::shapes::widget::make_widget;
use crate::shapes::widget::Spin;

fn run_all() -> u8 {
    let w = make_widget();
    assert_eq!(w.spin_up(), 1);
    w.spin_up()
}
"""
# Third commit changes `make_widget`'s body only.
LIB_V3 = LIB_V2.replace(
    "pub fn make_widget() -> Widget {\n    Widget\n}", "pub fn make_widget() -> Widget {\n    Widget {}\n}"
)


def git(repo: Path, *args: str) -> str:
    env = {
        **os.environ,
        "GIT_AUTHOR_NAME": "t",
        "GIT_AUTHOR_EMAIL": "t@e.x",
        "GIT_COMMITTER_NAME": "t",
        "GIT_COMMITTER_EMAIL": "t@e.x",
    }
    return subprocess.run(["git", "-C", str(repo), *args], check=True, capture_output=True, text=True, env=env).stdout


class RustAndGitGenerators(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls._tmp = tempfile.TemporaryDirectory()
        cls.repo = Path(cls._tmp.name) / "repo"
        repo = cls.repo
        (repo / "src/shapes").mkdir(parents=True)
        git(repo, "init", "-q")
        git(repo, "config", "commit.gpgsign", "false")
        (repo / "src/shapes/widget.rs").write_text(LIB_V1)
        git(repo, "add", "-A")
        git(repo, "commit", "-qm", "introduce widget")
        cls.c1 = git(repo, "rev-parse", "HEAD").strip()
        (repo / "src/shapes/widget.rs").write_text(LIB_V2)
        (repo / "src/run.rs").write_text(USE)
        git(repo, "add", "-A")
        git(repo, "commit", "-qm", "add widget factory")
        cls.c2 = git(repo, "rev-parse", "HEAD").strip()
        (repo / "src/shapes/widget.rs").write_text(LIB_V3)
        git(repo, "commit", "-qam", "build widget with braces")
        cls.c3 = git(repo, "rev-parse", "HEAD").strip()

    @classmethod
    def tearDownClass(cls) -> None:
        cls._tmp.cleanup()

    def gen(self, script: str, *args: str) -> list[dict]:
        proc = subprocess.run(
            [sys.executable, str(HERE / script), "--repo", str(self.repo), "--n", "40", "--seed", "1", *args],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(proc.returncode, 0, proc.stderr)
        return [json.loads(line) for line in proc.stdout.splitlines()]

    def by(self, tasks: list[dict], mode: str, key: str, value, variant: str | None = None) -> dict:
        found = [
            t
            for t in tasks
            if t["mode"] == mode
            and t["args"].get(key) == value
            and (variant is None or t.get("meta", {}).get("variant") == variant)
        ]
        self.assertEqual(
            len(found), 1, f"{mode} {key}={value} {variant}: {[t['args'] for t in tasks if t['mode'] == mode]}"
        )
        return found[0]

    def test_symbols_variants_have_gold_from_syn(self) -> None:
        tasks = self.gen("gen_rust.py", "--kind", "symbols")
        sub = self.by(tasks, "symbols", "name", "make_widget", "substring")
        self.assertEqual(sub["gold"], ["src/shapes/widget.rs:20", "src/shapes/widget.rs:24"])
        self.assertEqual(sub["scoring"], "set")
        exact = self.by(tasks, "symbols", "name", "make_widget", "exact")
        # The exact name must outrank `make_widget_pair`, so k is the gold size.
        self.assertEqual((exact["gold"], exact["scoring"], exact["k"]), (["src/shapes/widget.rs:20"], "ranked", 1))
        prefix = self.by(tasks, "symbols", "name", "make_", "prefix")
        self.assertEqual(prefix["gold"], ["src/shapes/widget.rs:20", "src/shapes/widget.rs:24"])
        fuzzy = [t for t in tasks if t.get("meta", {}).get("variant") == "fuzzy"]
        self.assertTrue(fuzzy)
        for t in fuzzy:
            self.assertIs(t["args"]["fuzzy"], True)
            original = t["meta"]["original"]
            self.assertEqual(len(t["args"]["name"]), len(original) - 1)
        for t in tasks:
            self.assertEqual(t["baseline"]["items"], "grep_lines")

    def test_baselines_find_what_the_gold_names(self) -> None:
        tasks = self.gen("gen_rust.py", "--kind", "symbols", "--kind", "implementations")
        for t in tasks:
            out = subprocess.run(["sh", "-c", t["baseline"]["grep"]], cwd=self.repo, capture_output=True, text=True)
            self.assertIn(out.returncode, (0, 1), t["baseline"]["grep"])
            found = {":".join(line.split(":")[:2]) for line in out.stdout.splitlines()}
            if t.get("meta", {}).get("variant") != "fuzzy":
                self.assertTrue(set(t["gold"]) <= found, (t["args"], t["gold"], found))

    def test_outline_lists_named_definitions_but_not_impl_blocks(self) -> None:
        tasks = self.gen("gen_rust.py", "--kind", "outline")
        widget = self.by(tasks, "outline", "path", "src/shapes/widget.rs")
        # Spin, spin_up (decl), Widget, spin_up (impl), fmt, make_widget, make_widget_pair.
        self.assertEqual(widget["gold"], [f"src/shapes/widget.rs:{n}" for n in (1, 2, 5, 8, 14, 20, 24)])
        self.assertEqual(widget["baseline"], {"read": ["src/shapes/widget.rs"], "items": "read"})

    def test_references_include_calls_inside_macro_arguments(self) -> None:
        tasks = self.gen("gen_rust.py", "--kind", "references")
        spin_up = self.by(tasks, "references", "name", "spin_up")
        # `w.spin_up()` inside `assert_eq!` (line 6) and on line 7.
        self.assertEqual(spin_up["gold"], ["src/run.rs:6", "src/run.rs:7"])
        make = self.by(tasks, "references", "name", "make_widget")
        self.assertEqual(make["gold"], ["src/run.rs:5", "src/shapes/widget.rs:25"])
        callers = self.by(tasks, "callers", "name", "make_widget")
        self.assertEqual(callers["args"]["path"], "src/shapes/widget.rs")
        self.assertEqual(callers["gold"], make["gold"])

    def test_implementations_match_path_qualified_traits(self) -> None:
        tasks = self.gen("gen_rust.py", "--kind", "implementations")
        self.assertEqual(self.by(tasks, "implementations", "name", "Display")["gold"], ["src/shapes/widget.rs:13"])
        self.assertEqual(self.by(tasks, "implementations", "name", "Spin")["gold"], ["src/shapes/widget.rs:7"])

    def test_dependents_and_read(self) -> None:
        tasks = self.gen("gen_rust.py", "--kind", "dependents", "--kind", "read")
        dep = self.by(tasks, "dependents", "name", "shapes::widget")
        self.assertEqual((dep["gold"], dep["args"]["relation"]), (["src/run.rs"], "importers"))
        read = self.by(tasks, "read", "symbol", "make_widget_pair")
        self.assertEqual(read["gold"], [f"src/shapes/widget.rs:{n}" for n in (24, 25, 26)])

    def test_git_kinds_take_gold_from_git_and_syn(self) -> None:
        tasks = self.gen("gen_git.py", "--kind", "git_path", "--kind", "git_symbol", "--kind", "diff")
        path = self.by(tasks, "git_path", "path", "src/shapes/widget.rs")
        self.assertEqual(path["gold"], [self.c3, self.c2, self.c1])
        sym = self.by(tasks, "git_symbol", "symbol", "make_widget")
        # Introduced in c2, body changed in c3.
        self.assertEqual(sorted(sym["gold"]), sorted([self.c2, self.c3]))
        spin_up = [t for t in tasks if t["mode"] == "git_symbol" and t["args"]["symbol"] == "spin_up"]
        self.assertEqual(spin_up, [], "`spin_up` is not unique in its file")
        diff = self.by(tasks, "diff", "to", self.c3)
        self.assertEqual(diff["gold"], ["-src/shapes/widget.rs:21", "+src/shapes/widget.rs:21"])
        structural = self.by(tasks, "diff_structural", "from", self.c1)
        self.assertEqual(structural["gold"], ["+fmt", "+make_widget", "+make_widget_pair"])
        blame = self.by(tasks, "blame_symbol", "symbol", "make_widget")
        self.assertEqual(blame["gold"], [f"{self.c2}:20", f"{self.c3}:21", f"{self.c2}:22"])

    def test_blame_gold_is_porcelain_per_line(self) -> None:
        tasks = self.gen("gen_git.py", "--kind", "blame")
        self.assertTrue(tasks)
        for t in tasks:
            a, b = t["args"]["line"], t["args"]["end_line"]
            self.assertEqual(len(t["gold"]), b - a + 1)
            self.assertEqual([int(g.split(":")[1]) for g in t["gold"]], list(range(a, b + 1)))

    def test_grep_tasks_alternate_literal_and_regex_and_annotate_enclosing_symbols(self) -> None:
        tasks = self.gen("gen_grep.py", "--ext", "rs", "--max-gold", "50")
        variants = {t["meta"]["variant"] for t in tasks}
        self.assertEqual(variants, {"literal", "regex"})
        for t in tasks:
            self.assertEqual("regex" in t["args"], t["meta"]["variant"] == "regex")
        # Innermost definition (or impl block, named after its type) per line, by hand.
        spans = {
            "src/shapes/widget.rs": [
                (1, 3, "Spin"),
                (2, 2, "spin_up"),
                (5, 5, "Widget"),
                (7, 11, "Widget"),
                (8, 10, "spin_up"),
                (13, 17, "Widget"),
                (14, 16, "fmt"),
                (20, 22, "make_widget"),
                (24, 26, "make_widget_pair"),
            ],
            "src/run.rs": [(4, 8, "run_all")],
        }

        def expected(key: str) -> str | None:
            path, line = key.rsplit(":", 1)
            inside = [s for s in spans[path] if s[0] <= int(line) <= s[1]]
            return min(inside, key=lambda s: s[1] - s[0])[2] if inside else None

        judged = 0
        for t in tasks:
            want = {g: expected(g) for g in t["gold"] if expected(g)}
            self.assertEqual(t.get("annotations", {}), want, t["args"])
            judged += len(want)
        self.assertGreater(judged, 0)


if __name__ == "__main__":
    unittest.main()
