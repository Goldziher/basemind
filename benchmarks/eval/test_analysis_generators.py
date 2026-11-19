"""Tests for the analysis-kind gold (`_edges.py`, `gen_graph.py`, `gen_changed.py`, `gen_large.py`,
`gen_read_around.py`, `gen_explain.py`): `python3 -m unittest discover benchmarks/eval`.

A throwaway repository with a known module graph, history and file sizes is built once; every
expected value is worked out by hand from the fixture. `rust_gold` is built offline from the cargo
cache on first use (set `BASEMIND_EVAL_RUST_GOLD` to reuse a binary).
"""

import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).parent
sys.path.insert(0, str(HERE))

import _edges as e  # noqa: E402
from gen_read_around import context_items, merged_ranges  # noqa: E402

# Runtime import graph (ADR-0047 decision 4: `mod x;` is containment, not an edge): a <-> b;
# a/inner -> c; c -> c/extra (`use d::D` through the `#[path]` module d); c/extra -> a/inner;
# tests/it -> a (through the package's own name); crates/sib -> c (a path dependency). Cycles:
# {a, b} and {a/inner, c/mod, c/extra}. The `mod` declarations lib -> {a, b, c}, a -> a/inner,
# c -> c/extra and tests/it -> tests/common are edges only with `module_decls`.
FILES = {
    "Cargo.toml": '[package]\nname = "fixture"\nversion = "0.1.0"\n',
    "src/lib.rs": "pub mod a;\npub mod b;\npub mod c;\n// Beacon_marker lives here too\n",
    "src/a.rs": "use crate::b::B;\nmod inner;\n\npub struct A;\n",
    "src/a/inner.rs": "use super::super::c::C;\n\npub fn helper_fn() -> u8 {\n    1\n}\n",
    "src/b.rs": "use crate::a::A;\nuse std::fmt;\nuse self::nested::N;\n\nmod nested {\n    pub struct N;\n}\n\npub struct B;\n",
    "src/c/mod.rs": '#[path = "extra.rs"]\nmod d;\nuse d::D;\n\npub struct C;\n',
    "src/c/extra.rs": "use crate::a::inner::helper_fn;\n\npub struct D;\n",
    "tests/it.rs": "mod common;\nuse fixture::a::A;\n",
    "tests/common.rs": "pub fn shared() {}\n",
    "crates/sib/Cargo.toml": '[package]\nname = "sib"\n\n[dependencies]\nfixture = { path = "../.." }\n',
    "crates/sib/src/lib.rs": "use fixture::c::C;\nuse serde::Serialize;\n",
    "pkg/__init__.py": "",
    "pkg/x.py": "from . import y\n",
    "pkg/y.py": "from pkg import x\n",
    "pkg/z.py": "import os\nimport pkg.y\nfrom .x import thing\n",
}
# 200 lines, the last without a newline: `wc -l` says 199, the scanner says 200.
BIG = "\n".join(f"// line {i}" for i in range(1, 201))
WIDGET = """pub struct Wheel;

impl Wheel {
    pub fn spin_wheel(&self) {}
}

pub fn make_widget_unique(w: &Wheel) -> u8 {
    w.spin_wheel();
    println!("x");
    crate::a::inner::helper_fn()
}

pub fn first_user(w: &Wheel) -> u8 {
    make_widget_unique(w)
}

pub fn second_user(w: &Wheel) -> u8 {
    make_widget_unique(w) + 1
}
"""
CHANGE = "\n// Beacon_marker changed here\n"


def git(repo: Path, *args: str) -> str:
    env = {
        **os.environ,
        "GIT_AUTHOR_NAME": "t",
        "GIT_AUTHOR_EMAIL": "t@e.x",
        "GIT_COMMITTER_NAME": "t",
        "GIT_COMMITTER_EMAIL": "t@e.x",
    }
    return subprocess.run(["git", "-C", str(repo), *args], check=True, capture_output=True, text=True, env=env).stdout


class AnalysisGenerators(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls._tmp = tempfile.TemporaryDirectory()
        cls.repo = repo = Path(cls._tmp.name) / "repo"
        repo.mkdir()
        git(repo, "init", "-q")
        git(repo, "config", "commit.gpgsign", "false")
        for rel, text in {**FILES, "src/big.rs": BIG, "src/widget.rs": WIDGET}.items():
            (repo / rel).parent.mkdir(parents=True, exist_ok=True)
            (repo / rel).write_text(text)
        git(repo, "add", "-A")
        git(repo, "commit", "-qm", "fixture")
        cls.c1 = git(repo, "rev-parse", "HEAD").strip()
        with (repo / "src/b.rs").open("a") as fh:
            fh.write(CHANGE)
        git(repo, "commit", "-qam", "touch b")

    @classmethod
    def tearDownClass(cls) -> None:
        cls._tmp.cleanup()

    def gen(self, script: str, *args: str) -> list[dict]:
        cmd = [sys.executable, str(HERE / script), "--repo", str(self.repo), "--seed", "1", *args]
        proc = subprocess.run(cmd, capture_output=True, text=True, check=False)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        again = subprocess.run(cmd, capture_output=True, text=True, check=False)
        self.assertEqual(proc.stdout, again.stdout, "generator output must be deterministic")
        return [json.loads(line) for line in proc.stdout.splitlines()]

    def baseline_output(self, task: dict) -> str:
        cmds = task["baseline"]["grep"]
        out = ""
        for cmd in cmds if isinstance(cmds, list) else [cmds]:
            # stdin closed, as the harness runs it: rg reads a piped stdin instead of the directory.
            proc = subprocess.run(
                ["sh", "-c", cmd], cwd=self.repo, stdin=subprocess.DEVNULL, capture_output=True, text=True, check=False
            )
            self.assertIn(proc.returncode, (0, 1), f"{cmd}: {proc.stderr}")
            out += proc.stdout
        return out

    def test_rust_edges_follow_mod_use_super_self_and_path_attributes(self) -> None:
        import _common as c
        import _rust as r

        repo = c.Repo(str(self.repo), exts="rs")
        recs = r.records(repo.blobs(repo.files()))
        manifests = e.cargo_manifests(repo)
        edges = e.rust_edges(repo.all_files(), recs, manifests)
        got = {src: sorted(t) for src, t in edges.items()}
        expected = {
            "src/a.rs": ["src/b.rs"],
            "src/a/inner.rs": ["src/c/mod.rs"],
            # `use self::nested::N` names b.rs itself and `std::fmt` the standard library: no edges.
            "src/b.rs": ["src/a.rs"],
            "src/c/mod.rs": ["src/c/extra.rs"],
            "src/c/extra.rs": ["src/a/inner.rs"],
            # `use fixture::a::A` names the package's own library, a crate of the repository.
            "tests/it.rs": ["src/a.rs"],
            # A path dependency is internal; `serde`, a crate no manifest declares, is no edge.
            "crates/sib/src/lib.rs": ["src/c/mod.rs"],
        }
        self.assertEqual(got, expected)
        self.assertEqual(edges["src/a.rs"], {"src/b.rs": 1}, "first import line per target")
        with_decls = e.rust_edges(repo.all_files(), recs, manifests, module_decls=True)
        self.assertEqual(sorted(with_decls["src/lib.rs"]), ["src/a.rs", "src/b.rs", "src/c/mod.rs"])
        self.assertEqual(with_decls["src/a.rs"], {"src/b.rs": 1, "src/a/inner.rs": 2})
        self.assertEqual(sorted(with_decls["tests/it.rs"]), ["src/a.rs", "tests/common.rs"])

    def test_rust_crate_names_resolve_through_the_manifest_and_the_workspace(self) -> None:
        files = ["Cargo.toml", "a/Cargo.toml", "a/src/lib.rs", "a/src/x.rs", "a/src/x_win.rs", "b/Cargo.toml"]
        files += ["b/src/lib.rs"]
        files += ["c/Cargo.toml", "c/src/lib.rs", "c/src/z.rs", "examples/Cargo.toml", "examples/ex.rs"]
        manifests = {
            "Cargo.toml": '[workspace]\nmembers = ["a", "b", "c"]\n\n[workspace.dependencies]\n'
            'cee = { path = "c", package = "c" }\n\n[patch.crates-io]\na = { path = "a" }\n',
            "a/Cargo.toml": '[package]\nname = "a"\n',
            "b/Cargo.toml": '[package]\nname = "b"\n\n[dependencies]\na = "1"\nserde = "1"\ncee.workspace = true\n',
            "c/Cargo.toml": '[package]\nname = "c"\n\n[lib]\npath = "src/z.rs"\n',
            # A member in the workspace root's `examples/`: its example belongs to it, not to the
            # virtual root manifest, which has no targets.
            "examples/Cargo.toml": '[package]\nname = "examples"\n\n[[example]]\nname = "ex"\npath = "ex.rs"\n\n'
            '[dev-dependencies]\na = { path = "../a" }\n',
        }

        def rec(t: str, path: str, line: int, **kw: str) -> dict:
            return {"t": t, "path": path, "line": line, "scope": "", **kw}

        recs = [
            rec("mod_file", "a/src/lib.rs", 1, name="x", path_attr=""),
            # The same module under another `#[cfg]`: walked too, while `a::x` names the first.
            rec("mod_file", "a/src/lib.rs", 3, name="x", path_attr="x_win.rs"),
            rec("use", "a/src/x_win.rs", 1, module="super::*"),
            rec("use", "b/src/lib.rs", 1, module="a::x::Thing"),
            rec("use", "b/src/lib.rs", 2, module="serde::Serialize"),
            rec("use", "b/src/lib.rs", 3, module="std::fmt"),
            rec("use", "b/src/lib.rs", 4, module="undeclared::Thing"),
            rec("use", "b/src/lib.rs", 5, module="cee::Item"),
            rec("use", "examples/ex.rs", 2, module="a::x"),
        ]
        got = e.rust_edges(files, recs, manifests)
        # `a` is a registry dependency the workspace patches to a path, `cee` a workspace path
        # dependency renamed from `c`, whose `[lib] path` is its root.
        self.assertEqual(
            got,
            {
                "a/src/x_win.rs": {"a/src/lib.rs": 1},
                "b/src/lib.rs": {"a/src/x.rs": 1, "c/src/z.rs": 5},
                "examples/ex.rs": {"a/src/x.rs": 2},
            },
        )
        alone = {k: v for k, v in manifests.items() if k != "Cargo.toml"}
        self.assertEqual(
            e.rust_edges(files, recs, alone),
            {"a/src/x_win.rs": {"a/src/lib.rs": 1}, "examples/ex.rs": {"a/src/x.rs": 2}},
            "no workspace, no patch",
        )

    def test_python_edges_resolve_absolute_relative_and_submodule_imports(self) -> None:
        sources = [(p, t) for p, t in FILES.items() if p.endswith(".py")]
        got = {src: sorted(t) for src, t in e.python_edges(sources).items()}
        self.assertEqual(
            got,
            {
                "pkg/x.py": ["pkg/y.py"],
                "pkg/y.py": ["pkg/x.py"],
                "pkg/z.py": ["pkg/x.py", "pkg/y.py"],
            },
        )

    def test_cycles_and_importers_on_hand_built_graphs(self) -> None:
        edges = {"a": {"b": 1}, "b": {"c": 1, "a": 2}, "c": {"d": 1}, "d": {"c": 1}, "e": {"a": 1}, "f": {"f": 1}}
        self.assertEqual(e.cycles(edges), [["a", "b"], ["c", "d"]], "a self-edge is no cycle")
        rev = e.reverse(edges)
        self.assertEqual(e.importers_within(rev, "c", 1), {"b": 1, "d": 1})
        self.assertEqual(e.importers_within(rev, "c", 2), {"b": 1, "d": 1, "a": 2})
        self.assertEqual(e.importers_within(rev, "c", 3), {"b": 1, "d": 1, "a": 2, "e": 3})
        chain = {str(i): {str(i + 1): 1} for i in range(3000)}
        chain["3000"] = {"0": 1}
        self.assertEqual(len(e.cycles(chain)[0]), 3001, "iterative: no recursion limit")

    def test_graph_tasks(self) -> None:
        tasks = self.gen("gen_graph.py", "--n", "40")
        impact = {(t["args"]["path"], t["args"]["depth"]): t for t in tasks if t["mode"] == "impact"}
        self.assertEqual(impact[("src/b.rs", 1)]["gold"], ["src/a.rs"], "lib.rs only declares `mod b;`")
        self.assertEqual(impact[("src/b.rs", 2)]["gold"], ["src/a.rs", "tests/it.rs"])
        self.assertEqual(impact[("src/c/mod.rs", 1)]["gold"], ["crates/sib/src/lib.rs", "src/a/inner.rs"])
        inner2 = impact[("src/a/inner.rs", 2)]
        self.assertEqual(inner2["gold"], ["src/c/extra.rs", "src/c/mod.rs"])
        self.assertEqual(inner2["args"]["relation"], "importers")
        self.assertEqual(len(inner2["baseline"]["grep"]), 2, "the target and its one depth-1 importer")
        got = {line.strip() for line in self.baseline_output(impact[("src/b.rs", 1)]).splitlines()}
        self.assertEqual(got, {"src/a.rs"})
        got = {line.strip() for line in self.baseline_output(impact[("src/a.rs", 1)]).splitlines()}
        # The test names `a` through the crate's own name; c/extra.rs names `a` on its way to
        # `a::inner`, which costs the grep precision. widget.rs's `crate::a::...` call is no `use`.
        self.assertEqual(got, {"src/b.rs", "tests/it.rs", "src/c/extra.rs"})

        cycles = [t for t in tasks if t["mode"] == "cycles"]
        everything = next(t for t in cycles if "path" not in t["args"])
        self.assertEqual(
            everything["gold"], ["src/a.rs", "src/a/inner.rs", "src/b.rs", "src/c/extra.rs", "src/c/mod.rs"]
        )
        by_path = {t["args"]["path"]: t["gold"] for t in cycles if "path" in t["args"]}
        self.assertEqual(by_path["src/a.rs"], ["src/a.rs", "src/b.rs"])
        self.assertEqual(by_path["src/c/extra.rs"], ["src/a/inner.rs", "src/c/extra.rs", "src/c/mod.rs"])
        self.assertEqual(len(by_path), 5)
        # No shell command an agent would run finds a cycle, so the kind has no baseline to charge.
        self.assertFalse([t["id"] for t in cycles if "baseline" in t])

        py = self.gen("gen_graph.py", "--ext", "py", "--kind", "cycles")
        self.assertEqual(py[0]["gold"], ["pkg/x.py", "pkg/y.py"])

    def test_grep_changed_scopes_gold_to_the_change(self) -> None:
        tasks = self.gen("gen_changed.py", "--n", "50", "--per-rev", "50")
        self.assertTrue(tasks)
        for t in tasks:
            self.assertEqual(t["args"]["changed"], self.c1)
            self.assertTrue(all(g.startswith("src/b.rs:") for g in t["gold"]), t)
        beacon = next(t for t in tasks if t["args"]["pattern"] == "Beacon_marker")
        self.assertEqual(beacon["gold"], ["src/b.rs:11"], "the occurrence in lib.rs is out of scope")
        got = {":".join(line.split(":")[:2]) for line in self.baseline_output(beacon).splitlines()}
        self.assertEqual(got, {"src/b.rs:11"})

    def test_find_large_counts_an_unterminated_last_line(self) -> None:
        tasks = self.gen("gen_large.py", "--min-dir-files", "100")
        at200 = next(t for t in tasks if t["args"]["min_lines"] == 200)
        self.assertEqual(at200["gold"], ["src/big.rs"])
        self.assertEqual(at200["args"]["language"], "rust")
        self.assertNotIn("path_prefix", at200["args"])
        wc = subprocess.run(["wc", "-l", "src/big.rs"], cwd=self.repo, capture_output=True, text=True, check=True)
        self.assertEqual(int(wc.stdout.split()[0]), 199, "the fixture really ends without a newline")
        rows = [re.match(r"\s*(\d+) (.*)", line).groups() for line in self.baseline_output(at200).splitlines()]
        self.assertEqual(rows, [("200", "src/big.rs")])
        self.assertFalse(any(t["args"]["min_lines"] > 200 for t in tasks), "no file reaches 300")

    def test_read_around_merges_overlapping_and_adjacent_ranges(self) -> None:
        self.assertEqual(merged_ranges([3, 9, 30], 33), [(1, 14), (25, 33)])
        self.assertEqual(merged_ranges([1, 12], 40), [(1, 17)], "adjacent ranges merge")
        self.assertEqual(merged_ranges([20, 40], 60), [(15, 25), (35, 45)])
        tasks = self.gen("gen_read_around.py", "--n", "500")
        spin = next(t for t in tasks if t["args"]["pattern"] == "spin_wheel")
        self.assertEqual(spin["args"]["ranges"], True)
        self.assertEqual(spin["gold"], [f"src/widget.rs:{n}" for n in range(1, 14)])
        self.assertEqual(spin["baseline"], {"grep": "rg -n -C5 -F -e 'spin_wheel' -g '*.rs'", "items": "grep_context"})

    def test_context_items_split_at_the_earliest_separator_naming_a_file(self) -> None:
        out = (
            "src/widget.rs-3-x\nsrc/widget.rs:4:    pub fn spin_wheel(&self) {}\n--\n"
            'src/b.rs-9-let s = "src/widget.rs:4:";\nsrc/b.rs:10-1-mixed\nnope.rs:1:x\n'
        )
        self.assertEqual(context_items(out, str(self.repo)), ["src/widget.rs:3", "src/widget.rs:4", "src/b.rs:9"])

    @unittest.skipUnless(shutil.which("rg"), "rg is not on PATH")
    def test_read_around_baseline_is_what_rg_prints(self) -> None:
        tasks = self.gen("gen_read_around.py", "--n", "500")
        self.assertTrue(tasks)
        separated = 0
        for t in tasks:
            out = self.baseline_output(t)
            separated += "--" in out.splitlines()
            # rg merges overlapping and adjacent windows itself: every gold line exactly once. Sorted,
            # because rg searches files in parallel and their order is not fixed.
            self.assertEqual(sorted(context_items(out, str(self.repo))), sorted(t["gold"]), t["args"]["pattern"])
        self.assertGreater(separated, 0, "some task spans two groups, so the `--` rows were read")

    def test_explain_gold_is_definition_call_sites_and_callees(self) -> None:
        tasks = self.gen("gen_explain.py", "--n", "50")
        t = next(t for t in tasks if t["args"]["target"] == "make_widget_unique")
        self.assertEqual(
            t["gold"],
            ["src/widget.rs:7", "src/widget.rs:14", "src/widget.rs:18", "->helper_fn", "->spin_wheel"],
        )
        # widget.rs sits in no crate root's module tree, so no importers grep: outline, callers,
        # body, history.
        self.assertEqual(t["baseline"]["items"], "explain", "the baseline is scored against the same gold")
        _outline, _callers, body, history = t["baseline"]["grep"]
        self.assertEqual(body, "sed -n '7,11p' 'src/widget.rs'")
        self.assertEqual(
            history, "git log -n 5 -s --format='%h %an %aI %s' -L '/fn make_widget_unique[(<]/,/^}/:src/widget.rs'"
        )
        rows = self.baseline_output({"baseline": {"grep": history}}).splitlines()
        self.assertEqual(len(rows), 1, "one commit changed the body")
        self.assertRegex(rows[0], r"^[0-9a-f]{7,} t \d{4}-\d\d-\d\dT\S+ fixture$")
        self.assertIn("make_widget_unique", self.baseline_output(t))

        helper = next(t for t in tasks if t["args"]["target"] == "helper_fn")
        cmds = helper["baseline"]["grep"]
        self.assertEqual(len(cmds), 5, "outline, callers, body, importers, history")
        importers = self.baseline_output({"baseline": {"grep": cmds[3]}}).split()
        self.assertEqual(importers, ["src/c/extra.rs"], "the one runtime `use` of a::inner")


if __name__ == "__main__":
    unittest.main()
