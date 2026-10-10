"""Gold for `dependents`: an imported module -> every file importing it.

basemind's `code dependents MODULE` is a substring match over the raw text of each import
statement (`from collections import (\\n OrderedDict,\\n ...)` matches `collections` and
`OrderedDict`). The gold is every Python file with an `import` / `from ... import` whose
statement source contains MODULE.
"""

import argparse
import ast

import _common as c


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    c.add_common_args(p)
    p.add_argument("--max-gold", type=int, default=60)
    p.add_argument("--min-len", type=int, default=4)
    p.add_argument("--no-cross-language-check", action="store_true")
    a = p.parse_args()

    repo = c.Repo(a.repo, a.exclude, a.ext, a.max_bytes)
    stmts: dict[str, list[str]] = {}  # path -> import statement texts
    modules: set[str] = set()
    for path, src in repo.blobs(repo.files()):
        tree = c.parse_python(src)
        if tree is None:
            continue
        texts = []
        for node in ast.walk(tree):
            if isinstance(node, (ast.Import, ast.ImportFrom)):
                seg = ast.get_source_segment(src, node)
                if seg:
                    texts.append(seg)
                if isinstance(node, ast.ImportFrom) and node.module:
                    modules.add(node.module)
                elif isinstance(node, ast.Import):
                    modules.update(alias.name for alias in node.names)
        stmts[path] = texts
    candidates = sorted(m for m in modules if len(m) >= a.min_len)
    rng = repo.rng("dependents", a.seed)
    rng.shuffle(candidates)
    exts = tuple("." + e.strip() for e in a.ext.split(","))
    tasks = []
    for module in candidates:
        if len(tasks) >= a.n:
            break
        gold = sorted(path for path, texts in stmts.items() if any(module in t for t in texts))
        if not gold or len(gold) > a.max_gold:
            continue
        if not a.no_cross_language_check and c.appears_outside(repo, module, exts):
            continue
        tasks.append(
            {
                "id": f"dependents-{len(tasks):04d}",
                "mode": "dependents",
                "args": {"module": module},
                "gold": gold,
                "scoring": "set",
                "baseline": {
                    "grep": "git grep -l -E "
                    + c.shell_quote(f"^[[:space:]]*(import|from)[[:space:]].*{module}")
                    + " -- "
                    + " ".join(c.shell_quote("*" + e) for e in exts)
                },
            }
        )
    c.emit(tasks, a.out)


if __name__ == "__main__":
    main()
