"""Rust code-navigation tasks with gold from `rust_gold` (syn), not from basemind.

Kinds (task `mode`) and what the gold is:

  symbols          a definition name -> every Rust definition whose name matches it. Four variants
                   (`meta.variant`): `substring` (gold: every name containing the query, set
                   scoring), `exact` (a name that is also a substring of other names; gold: the
                   definitions named exactly that, ranked with k = gold size, so the exact matches
                   must come first), `prefix` (`poll_`; gold: names starting with it, ranked with
                   k = gold size), `fuzzy` (the name with one character deleted, `fuzzy: true`; gold:
                   the definitions named exactly the original, ranked at k = 10).
  outline          a file -> the line of every named definition in it (any depth), set scoring.
  references       a name -> every call site of it (`f(..)`, `x.f(..)`, `T::f(..)`, `f!(..)`, also
                   inside macro arguments), at the line of the callee identifier.
  callers          the same gold, for names defined exactly once, with that definition's `path`.
  implementations  a trait name -> the line of every `impl Trait for Type` (any path or generic form).
  dependents       a module path (`runtime::task`) -> every file with a `use` whose flattened path
                   contains those segments contiguously.
  read             a fn/method unique by name in its file -> every line of its span.

"Definition" means fn, method (with or without a body), struct, enum, union, trait, type alias,
associated type, const, static, mod, `macro_rules!` and extern items; not impl blocks, fields or
variants. Gold is Rust only: basemind indexes every language, so a match in a `.py` or `.ts` file
costs the tool precision, never recall. Known misses of the truth: see `_rust.py`.

Baselines are the cheapest shell command that yields the same items (`git grep -n` for locations,
`git grep -l` for files, the whole file for `outline` and `read`); each names the reader that turns
its output into items (`baseline.items`) so the baseline is scored against the same gold.
"""

from __future__ import annotations

import argparse
import re
import sys
from collections import defaultdict

import _common as c
import _rust as r

KEYWORDS = "fn|struct|enum|union|trait|type|const|static|mod"
IDENT = "[A-Za-z0-9_]"
RS = "'*.rs'"
SKIP_PREFIXES = ("crate", "self", "super", "std", "core", "alloc")


def def_grep(name_re: str) -> str:
    """`git grep -n` for Rust definitions whose name matches `name_re` (POSIX ERE)."""
    pat = f"(^|[^A-Za-z0-9_])({KEYWORDS})[[:space:]]+{name_re}|macro_rules![[:space:]]*{name_re}"
    return f"git grep -n -E {c.shell_quote(pat)} -- {RS}"


def grep_baseline(cmd: str, items: str = "grep_lines", read: list[str] | None = None) -> dict:
    out = {"grep": cmd, "items": items}
    if read:
        out["read"] = read
    return out


def task(kind: str, i: int, args: dict, gold: list[str], scoring: str = "set", **extra) -> dict:
    t = {"id": f"{kind}-{i:04d}", "mode": kind, "args": args, "gold": gold, "scoring": scoring}
    t.update(extra)
    return t


def loc(d: dict) -> str:
    return f"{d['path']}:{d['line']}"


def gen_symbols(defs: list[dict], rng, n: int) -> list[dict]:
    by_name: dict[str, list[dict]] = defaultdict(list)
    for d in defs:
        by_name[d["name"]].append(d)
    names = sorted(by_name)
    containing = {}

    def contains(q: str) -> list[str]:
        if q not in containing:
            containing[q] = sorted({loc(d) for name in names if q in name for d in by_name[name]})
        return containing[q]

    tasks: list[dict] = []
    pool = [x for x in names if len(x) >= 5 and not x.startswith("_")]
    shuffled = c.sample(rng, pool, len(pool))

    count = 0
    for name in shuffled:
        if count >= n:
            break
        gold = contains(name)
        if 1 <= len(gold) <= 40:
            tasks.append(
                task(
                    "symbols",
                    len(tasks),
                    {"name": name, "limit": 200},
                    gold,
                    meta={"variant": "substring"},
                    baseline=grep_baseline(def_grep(f"{IDENT}*{re.escape(name)}")),
                )
            )
            count += 1

    count = 0
    for name in shuffled:
        if count >= n:
            break
        exact = sorted({loc(d) for d in by_name[name]})
        if not 1 <= len(exact) <= 10 or len(contains(name)) <= len(exact):
            continue
        tasks.append(
            task(
                "symbols",
                len(tasks),
                {"name": name},
                exact,
                "ranked",
                k=len(exact),
                meta={"variant": "exact"},
                baseline=grep_baseline(def_grep(f"{re.escape(name)}([^A-Za-z0-9_]|$)")),
            )
        )
        count += 1

    prefixes = sorted({x.split("_")[0] + "_" for x in names if "_" in x.strip("_") and len(x.split("_")[0]) >= 3})
    count = 0
    for prefix in c.sample(rng, prefixes, len(prefixes)):
        if count >= n:
            break
        gold = sorted({loc(d) for name in names if name.startswith(prefix) for d in by_name[name]})
        if not 2 <= len(gold) <= 30:
            continue
        tasks.append(
            task(
                "symbols",
                len(tasks),
                {"name": prefix},
                gold,
                "ranked",
                k=len(gold),
                meta={"variant": "prefix"},
                baseline=grep_baseline(def_grep(f"{re.escape(prefix)}{IDENT}*")),
            )
        )
        count += 1

    count = 0
    for name in shuffled:
        if count >= n:
            break
        exact = sorted({loc(d) for d in by_name[name]})
        if len(name) < 8 or not 1 <= len(exact) <= 5:
            continue
        i = rng.randrange(1, len(name) - 1)
        query = name[:i] + name[i + 1 :]
        if any(query in other for other in names):
            continue  # a substring hit, not a fuzzy one
        subseq = "".join(f"{re.escape(ch)}{IDENT}*" for ch in query)
        tasks.append(
            task(
                "symbols",
                len(tasks),
                {"name": query, "fuzzy": True},
                exact,
                "ranked",
                k=10,
                meta={"variant": "fuzzy", "original": name},
                baseline=grep_baseline(def_grep(f"{IDENT}*{subseq}")),
            )
        )
        count += 1
    return tasks


def gen_outline(defs: list[dict], rng, n: int) -> list[dict]:
    per_file: dict[str, set[int]] = defaultdict(set)
    for d in defs:
        per_file[d["path"]].add(d["line"])
    paths = sorted(p for p, lines in per_file.items() if 3 <= len(lines) <= 80)
    return [
        task(
            "outline",
            i,
            {"path": p},
            [f"{p}:{ln}" for ln in sorted(per_file[p])],
            baseline={"read": [p], "items": "read"},
        )
        for i, p in enumerate(c.sample(rng, paths, n))
    ]


def gen_refs(defs: list[dict], calls: list[dict], rng, n: int) -> list[dict]:
    sites: dict[str, set[str]] = defaultdict(set)
    for call in calls:
        sites[call["name"]].add(loc(call))
    def_paths: dict[str, set[str]] = defaultdict(set)
    def_count: dict[str, int] = defaultdict(int)
    for d in defs:
        def_paths[d["name"]].add(d["path"])
        def_count[d["name"]] += 1
    names = sorted(x for x in sites if x in def_count and len(x) >= 5 and 1 <= len(sites[x]) <= 60)
    tasks: list[dict] = []
    refs = callers = 0
    for name in c.sample(rng, names, len(names)):
        gold = sorted(sites[name])
        baseline = grep_baseline(f"git grep -n -w -F -e {c.shell_quote(name)} -- {RS}")
        if refs < n:
            tasks.append(task("references", len(tasks), {"name": name, "limit": 200}, gold, baseline=baseline))
            refs += 1
        fn_defs = [d for d in defs if d["name"] == name] if def_count[name] == 1 else []
        if callers < n and fn_defs and fn_defs[0]["kind"] in ("fn", "method"):
            args = {"name": name, "path": fn_defs[0]["path"], "limit": 200}
            tasks.append(task("callers", len(tasks), args, gold, baseline=baseline))
            callers += 1
        if refs >= n and callers >= n:
            break
    return tasks


def gen_impls(impls: list[dict], rng, n: int) -> list[dict]:
    by_trait: dict[str, set[str]] = defaultdict(set)
    for im in impls:
        by_trait[im["trait"]].add(loc(im))
    traits = sorted(t for t, g in by_trait.items() if 1 <= len(g) <= 60)
    tasks = []
    for t in c.sample(rng, traits, n):
        pat = f"impl[^{{]*[^A-Za-z0-9_]{t}([^A-Za-z0-9_][^{{]*)?[[:space:]]+for[[:space:]]"
        tasks.append(
            task(
                "implementations",
                len(tasks),
                {"name": t, "relation": "implementations", "limit": 200},
                sorted(by_trait[t]),
                baseline=grep_baseline(f"git grep -n -E {c.shell_quote(pat)} -- {RS}"),
            )
        )
    return tasks


def module_segments(module: str) -> list[str]:
    segs = module.split("::")
    while segs and segs[0] in SKIP_PREFIXES:
        segs = segs[1:]
    return [s for s in segs if s not in ("*", "self")]


def gen_dependents(uses: list[dict], rng, n: int) -> list[dict]:
    files_by_seq: dict[str, set[str]] = defaultdict(set)
    paths_segs: list[tuple[str, list[str]]] = []
    for u in uses:
        segs = [s for s in u["module"].split("::") if s not in ("*", "self")]
        paths_segs.append((u["path"], segs))
        inner = module_segments(u["module"])
        if len(inner) >= 3:  # leave the imported item off: a module, not a symbol
            files_by_seq["::".join(inner[:2])].add(u["path"])

    def importers(seq: str) -> list[str]:
        want = seq.split("::")
        return sorted(
            {
                path
                for path, segs in paths_segs
                if any(segs[i : i + len(want)] == want for i in range(len(segs) - len(want) + 1))
            }
        )

    seqs = sorted(s for s in files_by_seq if all(len(p) >= 2 for p in s.split("::")))
    tasks = []
    for seq in c.sample(rng, seqs, len(seqs)):
        if len(tasks) >= n:
            break
        gold = importers(seq)
        if not 1 <= len(gold) <= 40:
            continue
        pat = "^[[:space:]]*(pub(\\([^)]*\\))?[[:space:]]+)?use[[:space:]].*" + seq.replace("::", "::")
        tasks.append(
            task(
                "dependents",
                len(tasks),
                {"name": seq, "relation": "importers", "limit": 200},
                gold,
                baseline=grep_baseline(f"git grep -l -E {c.shell_quote(pat)} -- {RS}", "paths"),
            )
        )
    return tasks


def unique_fns(defs: list[dict], min_lines: int = 3, max_lines: int = 150) -> list[dict]:
    """fn/method definitions whose name occurs once among the definitions of their file."""
    per_file: dict[tuple[str, str], int] = defaultdict(int)
    for d in defs:
        per_file[(d["path"], d["name"])] += 1
    return [
        d
        for d in defs
        if d["kind"] in ("fn", "method")
        and per_file[(d["path"], d["name"])] == 1
        and min_lines <= d["end"] - d["line"] + 1 <= max_lines
    ]


def gen_read(defs: list[dict], rng, n: int) -> list[dict]:
    pool = sorted(unique_fns(defs), key=lambda d: (d["path"], d["line"]))
    tasks = []
    for d in c.sample(rng, pool, n):
        gold = [f"{d['path']}:{ln}" for ln in range(d["line"], d["end"] + 1)]
        grep = f"git grep -n -E {c.shell_quote('fn[[:space:]]+' + d['name'] + '[(<]')} -- {c.shell_quote(d['path'])}"
        tasks.append(
            task(
                "read",
                len(tasks),
                {"path": d["path"], "symbol": d["name"]},
                gold,
                baseline=grep_baseline(grep, "read", read=[d["path"]]),
            )
        )
    return tasks


KINDS = ["symbols", "outline", "references", "implementations", "dependents", "read"]


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    c.add_common_args(p, ext_default="rs")
    p.add_argument("--kind", action="append", choices=KINDS, help="kinds to emit (default all; repeatable)")
    a = p.parse_args()

    repo = c.Repo(a.repo, a.exclude, a.ext, a.max_bytes)
    recs = r.records(repo.blobs(repo.files()))
    print(r.summary(recs), file=sys.stderr)
    defs = [d for d in r.by_type(recs, "def") if d["kind"] in r.NAMED_KINDS]
    kinds = a.kind or KINDS
    tasks: list[dict] = []
    for kind in kinds:
        rng = repo.rng(f"rust-{kind}", a.seed)
        if kind == "symbols":
            tasks += gen_symbols(defs, rng, a.n)
        elif kind == "outline":
            tasks += gen_outline(defs, rng, a.n)
        elif kind == "references":
            tasks += gen_refs(defs, r.by_type(recs, "call"), rng, a.n)
        elif kind == "implementations":
            tasks += gen_impls(r.by_type(recs, "impl"), rng, a.n)
        elif kind == "dependents":
            tasks += gen_dependents(r.by_type(recs, "use"), rng, a.n)
        elif kind == "read":
            tasks += gen_read(defs, rng, a.n)
    c.emit(tasks, a.out)


if __name__ == "__main__":
    main()
