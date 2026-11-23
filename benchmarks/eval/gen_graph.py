"""Gold for `impact` and `cycles` from the file-level import edges of `_edges.py` (not basemind).

Kinds (task `mode`):

  impact   a file -> every file that reaches it within `depth` import edges (`depth` 1 and 2), as
           `code_refs {relation: "importers", path, depth}` answers it (#121). Set scoring.
  cycles   every file that sits in an import cycle (`code_refs {relation: "cycles"}`, variant `all`),
           or with `path` the files of the cycle holding it (variant `path`) (#135). A cycle is a
           strongly connected component of two or more files. Set scoring.

`--ext rs` reads Rust through `rust_gold`, `--ext py` Python through `ast`, `--ext ts` (or `ts,tsx`) TypeScript
from the source text, barrels included; the edge rules and what
they miss are in `_edges.py`. Rust gold counts runtime imports only, as the graph answers by
default (ADR-0047 decision 4): a `mod x;` declaration is no edge, and a path through the package's
own name or a workspace crate is internal.

Baselines (ADR-0031 rule 5, the shell an agent would run):

  impact   one `git grep -l` per file whose importers the agent needs: the `use` lines naming the
           module's name as a path segment (for a crate root, every `use` through the crate), or for
           Python the module's last name in an import line, or for TypeScript the file's name (the
           directory's for an `index`) in the specifier of an `import` / `export ... from` line. Depth 2 adds one grep per depth-1
           importer of the gold. Scored as paths.
  cycles   none. Finding a cycle means resolving every import to a file and walking the graph, which
           no shell command an agent would run does: the only shell answer is every import line of the
           repository (about 100k tokens on each corpus), which no agent reads and which measures
           the repository's size, not the tool. The kind is gated on correctness alone (ADR-0051).
"""

from __future__ import annotations

import argparse
import re

import _common as c
import _edges as e

NOT_IDENT = "([^A-Za-z0-9_]|$)"
USE_LINE = "^[[:space:]]*(pub(\\([^)]*\\))?[[:space:]]+)?use[[:space:]]"


def module_paths(
    tables: dict[str, dict[tuple[str, ...], str]], libs: dict[str, str]
) -> dict[str, list[tuple[list[str], str | None]]]:
    """Rust: every module path of each file, over every crate root reaching it, with the name of
    the library that root is (None for a binary, test, example or bench)."""
    out: dict[str, list[tuple[list[str], str | None]]] = {}
    for root, table in tables.items():
        for modpath, path in sorted(table.items()):
            out.setdefault(path, []).append((list(modpath), libs.get(root)))
    return out


def rust_importer_grep(modpaths: list[tuple[list[str], str | None]]) -> str:
    """`use` lines naming the module as a path segment (`crate::a::b`, `super::b`, `b::X` in its
    parent, `name::a::b` from another crate); for a crate root, `use` lines through the crate."""
    alts = []
    for mp, lib in modpaths:
        if mp:
            alts.append(f"{USE_LINE}.*(^|[^A-Za-z0-9_]){mp[-1]}{NOT_IDENT}")
        else:
            alts.append(f"{USE_LINE}.*crate::")
            if lib:
                alts.append(f"{USE_LINE}.*(^|[^A-Za-z0-9_:]){lib}::")
    pattern = "|".join(sorted(set(alts)))
    return "git grep -l -E " + c.shell_quote(pattern) + " -- '*.rs'"


def python_importer_grep(path: str) -> str:
    stem = path.rsplit("/", 1)[-1][: -len(".py")]
    if stem == "__init__":
        stem = path.rsplit("/", 2)[-2]
    pattern = f"^[[:space:]]*(from|import)[[:space:]].*(^|[^A-Za-z0-9_]){re.escape(stem)}{NOT_IDENT}"
    return "git grep -l -E " + c.shell_quote(pattern) + " -- '*.py'"


def typescript_importer_grep(path: str) -> str:
    stem = path.rsplit("/", 1)[-1].split(".", 1)[0]
    if stem == "index":
        stem = path.rsplit("/", 2)[-2] if "/" in path else stem
    pattern = f"(from|import|require)[^;]*['\"][^'\"]*(^|[^A-Za-z0-9_]){re.escape(stem)}(/index)?(\\.[A-Za-z]+)?['\"]"
    return "git grep -l -E " + c.shell_quote(pattern) + " -- '*.ts' '*.tsx' '*.mts' '*.cts'"


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    c.add_common_args(p, ext_default="rs")
    p.add_argument("--kind", action="append", choices=["impact", "cycles"], help="kinds to emit (default both)")
    p.add_argument("--max-gold", type=int, default=60, help="largest importer set an impact task may have")
    a = p.parse_args()
    if a.ext not in ("rs", "py", "ts", "ts,tsx"):
        p.error("--ext must be rs, py, ts or ts,tsx")
    kinds = a.kind or ["impact", "cycles"]

    repo = c.Repo(a.repo, a.exclude, a.ext, a.max_bytes)
    files = repo.files()
    if a.ext == "rs":
        import _rust as r

        recs = r.records(repo.blobs(files))
        manifests = e.cargo_manifests(repo)
        edges, tables = e.rust_graph(repo.all_files(), recs, manifests)
        modpaths = module_paths(tables, e.lib_names(manifests))

        def importer_grep(path: str) -> str | None:
            return rust_importer_grep(modpaths[path]) if path in modpaths else None
    elif a.ext == "py":
        edges = e.python_edges(repo.blobs(files))

        def importer_grep(path: str) -> str | None:
            return python_importer_grep(path)
    else:
        edges = e.typescript_edges(repo.blobs(files))

        def importer_grep(path: str) -> str | None:
            return typescript_importer_grep(path)

    tasks: list[dict] = []
    if "impact" in kinds:
        rev = e.reverse(edges)
        targets = sorted(t for t in rev if importer_grep(t))
        rng = repo.rng("impact", a.seed)
        count = 0
        for target in c.sample(rng, targets, len(targets)):
            if count >= a.n:
                break
            one = e.importers_within(rev, target, 1)
            two = e.importers_within(rev, target, 2)
            for depth, gold in ((1, one), (2, two)):
                if count >= a.n or not 1 <= len(gold) <= a.max_gold or (depth == 2 and len(two) == len(one)):
                    continue
                frontier = [target] + (sorted(one) if depth == 2 else [])
                cmds = [cmd for f in frontier if (cmd := importer_grep(f))]
                tasks.append(
                    {
                        "id": f"impact-{count:04d}",
                        "mode": "impact",
                        "args": {"relation": "importers", "path": target, "depth": depth},
                        "gold": sorted(gold),
                        "scoring": "set",
                        "meta": {"variant": f"depth{depth}"},
                        "baseline": {"grep": cmds if len(cmds) > 1 else cmds[0], "items": "paths"},
                    }
                )
                count += 1

    if "cycles" in kinds:
        comps = e.cycles(edges)
        if comps:
            tasks.append(
                {
                    "id": "cycles-0000",
                    "mode": "cycles",
                    "args": {"relation": "cycles"},
                    "gold": sorted({f for comp in comps for f in comp}),
                    "scoring": "set",
                    "meta": {"variant": "all", "components": len(comps)},
                }
            )
        rng = repo.rng("cycles", a.seed)
        # One file per component in turn, so small cycles are not crowded out by the largest.
        pools = [c.sample(rng, comp, len(comp)) for comp in comps]
        picks: list[str] = []
        while any(pools) and len(picks) < a.n:
            for pool in pools:
                if pool and len(picks) < a.n:
                    picks.append(pool.pop(0))
        member = {f: comp for comp in comps for f in comp}
        for i, path in enumerate(picks, start=1):
            tasks.append(
                {
                    "id": f"cycles-{i:04d}",
                    "mode": "cycles",
                    "args": {"relation": "cycles", "path": path},
                    "gold": member[path],
                    "scoring": "set",
                    "meta": {"variant": "path"},
                }
            )
    c.emit(tasks, a.out)


if __name__ == "__main__":
    main()
