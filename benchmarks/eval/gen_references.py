"""Gold for `references` and `callers`: a callee name -> every Python call site of it.

basemind's `code references NAME` is name-only and call-site-only: `NAME(...)` and `x.NAME(...)`
count, a bare `NAME` (decorator without parentheses, argument, assignment) does not. It reports
the line on which the call expression STARTS. The gold is every `ast.Call` whose callee
identifier is NAME at that line, cross-checked against `git grep -n -w`: a call is kept only if
the word occurs on a line of the callee expression in the HEAD tree (drops nothing on a
consistent tree; it guards against parse/line disagreements and is counted in the summary).

`--mode callers` additionally restricts to names defined exactly once in the repo and passes that
definition's path, matching how `code callers PATH NAME` is used. `--mode both` emits both kinds.

Pyrefly note: `pyrefly` 1.x exposes no find-references CLI (only `check`, `infer`, `lsp`, ...);
scope-resolved references would need a `textDocument/references` request to `pyrefly lsp`, which
this generator does not do. The name-only gold here is the right oracle for basemind's name-only
tools anyway.
"""

import argparse
import ast
import sys
from collections import defaultdict

import _common as c


def callee_name(func: ast.expr) -> str | None:
    if isinstance(func, ast.Name):
        return func.id
    if isinstance(func, ast.Attribute):
        return func.attr
    return None


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    c.add_common_args(p)
    p.add_argument("--mode", choices=["references", "callers", "both"], default="references")
    p.add_argument("--max-gold", type=int, default=40)
    p.add_argument("--min-len", type=int, default=5)
    p.add_argument("--no-cross-language-check", action="store_true")
    a = p.parse_args()

    repo = c.Repo(a.repo, a.exclude, a.ext, a.max_bytes)
    calls: dict[str, list[tuple[str, int, int]]] = defaultdict(list)  # name -> (path, start, func_end)
    defs: dict[str, list[str]] = defaultdict(list)  # name -> [defining paths] (def/class only)
    for path, src in repo.blobs(repo.files()):
        tree = c.parse_python(src)
        if tree is None:
            continue
        for node in ast.walk(tree):
            if isinstance(node, ast.Call) and (name := callee_name(node.func)):
                calls[name].append(
                    (
                        path,
                        node.lineno,
                        getattr(node.func, "end_lineno", node.lineno) or node.lineno,
                    )
                )
        # Definitions as basemind indexes them: module/class scope only, never function-nested.
        for name, _ in c.python_symbols(tree, assignments=False):
            defs[name].append(path)

    names = sorted(n for n in calls if len(n) >= a.min_len and not n.startswith("__"))
    rng = repo.rng("references", a.seed)
    rng.shuffle(names)
    exts = tuple("." + e.strip() for e in a.ext.split(","))
    kinds = ["references", "callers"] if a.mode == "both" else [a.mode]
    tasks: list[dict] = []
    dropped = 0
    for name in names:
        if len(tasks) >= a.n:
            break
        if a.mode == "callers" and len(set(defs[name])) != 1:
            continue
        grep_hits = c.git_grep_lines(repo, "-w", "-F", "-e", name, pathspec=["*" + e for e in exts])
        grep_set = set(grep_hits)
        gold = set()
        for path, start, end in calls[name]:
            if any((path, ln) in grep_set for ln in range(start, end + 1)):
                gold.add((path, start))
            else:
                dropped += 1
        if not gold or len(gold) > a.max_gold:
            continue
        if not a.no_cross_language_check and c.appears_outside(repo, name, exts):
            continue
        gold_lines = [f"{path}:{line}" for path, line in sorted(gold)]
        baseline = {
            "grep": "git grep -n -w -F "
            + c.shell_quote(name)
            + " -- "
            + " ".join(c.shell_quote("*" + e) for e in exts),
        }
        for kind in kinds:
            if kind == "callers":
                owners = sorted(set(defs[name]))
                if len(owners) != 1:
                    continue
                args = {"path": owners[0], "name": name, "limit": 200}
            else:
                args = {"name": name, "limit": 200}
            tasks.append(
                {
                    "id": f"{kind}-{len(tasks):04d}",
                    "mode": kind,
                    "args": args,
                    "gold": gold_lines,
                    "scoring": "set",
                    "baseline": baseline,
                }
            )
    print(
        f"cross-check dropped {dropped} ast call sites not confirmed by git grep -w",
        file=sys.stderr,
    )
    c.emit(tasks, a.out)


if __name__ == "__main__":
    main()
