"""Gold for `explain`: a Rust fn or method -> what a briefing on it must point at, from `rust_gold`.

`basemind code explain <target>` (#120, CLI-only until the kind passes) briefs one symbol. A task
targets a fn or method whose name is defined exactly once in the repository (so a name-based gold
is unambiguous) with 1 to `--max-callers` call sites. The gold is:

- the definition, `path:line` (the first line after attributes and doc comments);
- every call site of the name, `path:line` (name-based, as for `callers`; see `_rust.py`);
- `->name` for every distinct function or method the body calls (macros are not callees).

Baseline (ADR-0047: "outline plus refs plus read done by hand through the shell"): the file's
definitions (`git grep -n` of the item keywords in the file), the call sites
(`git grep -n -w NAME`), and the body (`sed -n` over its span). It is a reducing kind, charged
for tokens only.
"""

from __future__ import annotations

import argparse
import sys
from collections import defaultdict

import _common as c
import _rust as r
from gen_rust import KEYWORDS, RS, loc, unique_fns


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    c.add_common_args(p, ext_default="rs")
    p.add_argument("--max-callers", type=int, default=30)
    a = p.parse_args()
    if a.ext != "rs":
        p.error("--ext must be rs: the gold comes from rust_gold")

    repo = c.Repo(a.repo, a.exclude, a.ext, a.max_bytes)
    recs = r.records(repo.blobs(repo.files()))
    print(r.summary(recs), file=sys.stderr)
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
        tasks.append(
            {
                "id": f"explain-{len(tasks):04d}",
                "mode": "explain",
                "args": {"target": d["name"]},
                "gold": gold,
                "scoring": "set",
                "meta": {"path": d["path"], "callers": len(sites[d["name"]]), "callees": len(callees)},
                "baseline": {
                    "grep": [
                        f"git grep -n -E {c.shell_quote(outline)} -- {c.shell_quote(d['path'])}",
                        f"git grep -n -w -F -e {c.shell_quote(d['name'])} -- {RS}",
                        f"sed -n '{d['line']},{d['end']}p' {c.shell_quote(d['path'])}",
                    ]
                },
            }
        )
    c.emit(tasks, a.out)


if __name__ == "__main__":
    main()
