"""Gold for `symbols`: a definition name -> every Python definition whose name contains it.

basemind's `code symbols` is a case-sensitive substring match over every indexed definition, so
the gold is every def / class / simple assignment target (see `_common.python_symbols`) at any
depth whose name contains the query. Names that also occur in other languages' files are skipped
(unless --no-cross-language-check) because those files would legitimately add hits.
"""

import argparse
import sys
from collections import defaultdict

import _common as c


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    c.add_common_args(p)
    p.add_argument(
        "--max-gold",
        type=int,
        default=40,
        help="skip names with more matches than this",
    )
    p.add_argument("--min-len", type=int, default=5, help="shortest query name")
    p.add_argument("--no-cross-language-check", action="store_true")
    a = p.parse_args()

    repo = c.Repo(a.repo, a.exclude, a.ext, a.max_bytes)
    where: dict[str, list[tuple[str, int]]] = defaultdict(list)  # name -> [(path, line)]
    for path, src in repo.blobs(repo.files()):
        tree = c.parse_python(src)
        if tree is not None:
            for name, line in c.python_symbols(tree):
                where[name].append((path, line))
    names = sorted(n for n in where if len(n) >= a.min_len and n.isidentifier() and not n.startswith("__"))
    rng = repo.rng("symbols", a.seed)
    rng.shuffle(names)

    tasks: list[dict] = []
    exts = tuple("." + e.strip() for e in a.ext.split(","))
    for name in names:
        if len(tasks) >= a.n:
            break
        gold = sorted({(path, line) for other, locs in where.items() if name in other for path, line in locs})
        if not gold or len(gold) > a.max_gold:
            continue
        if not a.no_cross_language_check and c.appears_outside(repo, name, exts):
            continue
        files = c.read_baseline_files(p for p, _ in gold)
        tasks.append(
            {
                "id": f"symbols-{len(tasks):04d}",
                "mode": "symbols",
                "args": {"name": name, "limit": 200},
                "gold": [f"{path}:{line}" for path, line in gold],
                "scoring": "set",
                "baseline": {
                    "grep": "git grep -n -E "
                    + c.shell_quote(f"(def|class)[[:space:]]+[A-Za-z_0-9]*{name}|{name}[[:space:]]*=")
                    + " -- "
                    + " ".join(c.shell_quote("*" + e) for e in exts),
                    "read": files,
                },
            }
        )
    if len(tasks) < a.n:
        print(
            f"only {len(tasks)} of {a.n} requested tasks met the filters",
            file=sys.stderr,
        )
    c.emit(tasks, a.out)


if __name__ == "__main__":
    main()
