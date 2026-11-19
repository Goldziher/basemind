"""Gold for `explain`: a Rust fn or method -> what a briefing on it must point at, from `rust_gold`.

`basemind code explain <target>` (#120, CLI-only until the kind passes) briefs one symbol. A task
targets a fn or method whose name is defined exactly once in the repository (so a name-based gold
is unambiguous) with 1 to `--max-callers` call sites. The gold is:

- the definition, `path:line` (the first line after attributes and doc comments);
- every call site of the name, `path:line` (name-based, as for `callers`; see `_rust.py`);
- `->name` for every distinct function or method the body calls (macros are not callees).

The tool (symbol target, default sections) answers: the definition (path, line span, signature,
doc comment), the body (up to 60 lines), the callers (call sites grouped by file), the callees (the
names the body calls; macros are not callees: the tool drops `name!` invocations as well), the
importers (files importing the definition's file, one hop, runtime `use` only) and the history (the
last 5 commits that changed the symbol's body).

Baseline (ADR-0047: "outline plus refs plus read done by hand through the shell"; ADR-0031 rule 5,
the shell an agent would run), one command per section the tool is scored and charged for:

- definition: the file's definition lines (`git grep -n` of the item keywords in the file);
- callers: `git grep -n -w NAME`;
- body, doc comment and callees: `sed -n` from the first attribute or doc comment line
  (`rust_gold`'s `start`) to the last line, read for the names it calls;
- importers: the `impact` baseline's `git grep -l -E` for `use` lines naming the definition's module
  (`gen_graph.rust_importer_grep`, depth 1), omitted for a file no crate root's module tree reaches;
- history: `git log -n 5 -s --format='%h %an %aI %s' -L '/fn NAME[(<]/,/^INDENT}/:PATH'`, as the
  `git_symbol` baseline finds the function, with the short sha, author, author time and summary;
  where git cannot place that regex at HEAD the agent's known span `-L LINE,END:PATH` stands in.

It is a reducing kind, charged for tokens only (no reader turns the output into items).
"""

from __future__ import annotations

import argparse
import subprocess
import sys
from collections import defaultdict

import _common as c
import _edges as e
import _rust as r
from gen_git import indent_of, range_regex
from gen_graph import module_paths, rust_importer_grep
from gen_rust import KEYWORDS, RS, loc, unique_fns

HISTORY = "git log -n 5 -s --format='%h %an %aI %s' -L "


def history_command(repo: c.Repo, d: dict, src_lines: list[str]) -> str:
    """The `git_symbol` baseline's `-L` regex, or the known span where git cannot place it."""
    span = range_regex(d["name"], indent_of(src_lines, d["line"])) + ":" + d["path"]
    probe = ["log", "-n", "1", "-s", "--format=%h", "-L", span, "HEAD"]
    proc = subprocess.run(["git", "-C", repo.root, *probe], capture_output=True, check=False)
    if proc.returncode != 0:
        print(f"explain: -L regex misses {d['name']} in {d['path']}; using its line span", file=sys.stderr)
        span = f"{d['line']},{d['end']}:{d['path']}"
    return HISTORY + c.shell_quote(span)


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    c.add_common_args(p, ext_default="rs")
    p.add_argument("--max-callers", type=int, default=30)
    a = p.parse_args()
    if a.ext != "rs":
        p.error("--ext must be rs: the gold comes from rust_gold")

    repo = c.Repo(a.repo, a.exclude, a.ext, a.max_bytes)
    blobs = list(repo.blobs(repo.files()))
    src = {path: text.split("\n") for path, text in blobs}
    recs = r.records(blobs)
    print(r.summary(recs), file=sys.stderr)
    manifests = e.cargo_manifests(repo)
    _, tables = e.rust_graph(repo.all_files(), recs, manifests)
    modpaths = module_paths(tables, e.lib_names(manifests))
    defs = [d for d in r.by_type(recs, "def") if d["kind"] in r.NAMED_KINDS]
    calls = r.by_type(recs, "call")
    def_count: dict[str, int] = defaultdict(int)
    for d in defs:
        def_count[d["name"]] += 1
    sites: dict[str, set[str]] = defaultdict(set)
    in_file: dict[str, list[dict]] = defaultdict(list)
    for call in calls:
        sites[call["name"]].add(loc(call))
        in_file[call["path"]].append(call)
    pool = sorted(
        (
            d
            for d in unique_fns(defs)
            if def_count[d["name"]] == 1 and len(d["name"]) >= 5 and 1 <= len(sites[d["name"]]) <= a.max_callers
        ),
        key=lambda d: (d["path"], d["line"]),
    )
    rng = repo.rng("explain", a.seed)
    outline = f"(^|[^A-Za-z0-9_])({KEYWORDS})[[:space:]]+[A-Za-z_]"
    tasks = []
    for d in c.sample(rng, pool, a.n):
        callees = sorted(
            {
                f"->{call['name']}"
                for call in in_file[d["path"]]
                if call["via"] != "macro" and d["line"] <= call["line"] <= d["end"] and call["name"] != d["name"]
            }
        )
        gold = [loc(d), *sorted(sites[d["name"]] - {loc(d)}), *callees]
        grep = [
            f"git grep -n -E {c.shell_quote(outline)} -- {c.shell_quote(d['path'])}",
            f"git grep -n -w -F -e {c.shell_quote(d['name'])} -- {RS}",
            f"sed -n '{d['start']},{d['end']}p' {c.shell_quote(d['path'])}",
        ]
        if d["path"] in modpaths:
            grep.append(rust_importer_grep(modpaths[d["path"]]))
        grep.append(history_command(repo, d, src[d["path"]]))
        tasks.append(
            {
                "id": f"explain-{len(tasks):04d}",
                "mode": "explain",
                "args": {"target": d["name"]},
                "gold": gold,
                "scoring": "set",
                "meta": {"path": d["path"], "callers": len(sites[d["name"]]), "callees": len(callees)},
                "baseline": {"grep": grep},
            }
        )
    c.emit(tasks, a.out)


if __name__ == "__main__":
    main()
