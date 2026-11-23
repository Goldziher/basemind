"""Gold for `symbols`, `references` and `dependents` in TypeScript, Go and Java (#313), from `_langs.py`.

    gen_structural.py --repo DIR --lang typescript|go|java --kind symbols|references|dependents

The task shapes are the Python generators' (`gen_symbols.py`, `gen_references.py`, `gen_dependents.py`):

  symbols      a definition name -> `path:line` of every definition whose name contains it (basemind's
               `code symbols` is a case-sensitive substring match).
  references   a callee name -> `path:line` of every call site, at the line the call expression starts.
  dependents   an imported module -> every file whose import statement text contains it.

A name is a candidate only where the scanner reads every occurrence exactly (`_langs.py`); a name that also
occurs in another language's file is skipped, as there, because those files would add hits. Java has no
dependents gold: basemind's Java query has no `imports` section.
"""

from __future__ import annotations

import argparse
import re
import sys
from collections import defaultdict
from collections.abc import Callable

import _common as c
import _langs as lg

MIN_NAME = 5
MIN_MODULE = 4
IDENTIFIER = re.compile(lg.IDENT)


def symbol_gold(repo: c.Repo, lang: str) -> dict[str, list[tuple[str, int]]]:
    """Definition name -> every `(path, line)` basemind lists it at."""
    where: dict[str, list[tuple[str, int]]] = defaultdict(list)
    for path, src in repo.blobs(repo.files()):
        for name, line in lg.symbols(src, lang):
            where[name].append((path, line))
    return where


def reference_gold(repo: c.Repo, lang: str, exts: tuple[str, ...]) -> dict[str, list[tuple[str, int]]]:
    """Callee name -> every call-site `(path, line)`, for the names every occurrence of which is plainly a
    call, each site confirmed by `git grep -w` as the Python generator confirms its `ast` calls."""
    sites: dict[str, list[tuple[str, int]]] = defaultdict(list)
    unclear: set[str] = set()
    for path, src in repo.blobs(repo.files()):
        found, bad = lg.call_sites(src, lang)
        unclear |= bad
        for name, lines in found.items():
            sites[name].extend((path, line) for line in lines)
    out: dict[str, list[tuple[str, int]]] = {}
    for name in sorted(set(sites) - unclear):
        hits = set(c.git_grep_lines(repo, "-w", "-F", "-e", name, pathspec=["*" + e for e in exts]))
        out[name] = [site for site in sites[name] if site in hits]
    return out


def dependent_gold(repo: c.Repo, lang: str) -> dict[str, list[str]]:
    """Module specifier -> every file with an import statement whose text contains it."""
    statements: dict[str, list[str]] = {}
    modules: set[str] = set()
    for path, src in repo.blobs(repo.files()):
        found = lg.import_statements(src, lang)
        statements[path] = [text for text, _ in found]
        modules.update(spec for _, specs in found for spec in specs if not re.search(r"\s", spec))
    return {
        module: sorted(path for path, texts in statements.items() if any(module in t for t in texts))
        for module in sorted(modules)
        if len(module) >= MIN_MODULE
    }


def candidates(
    repo: c.Repo,
    a: argparse.Namespace,
    kind: str,
    gold: dict[str, list],
    exts: tuple[str, ...],
    accept: Callable[[str], bool],
) -> list[tuple[str, list]]:
    """The shuffled `(name, gold)` pairs that pass the size and cross-language filters, up to `--n`."""
    names = sorted(name for name in gold if gold[name] and accept(name))
    repo.rng(kind, a.seed).shuffle(names)
    out: list[tuple[str, list]] = []
    for name in names:
        if len(out) >= a.n:
            break
        if len(gold[name]) > a.max_gold:
            continue
        if not a.no_cross_language_check and c.appears_outside(repo, name, exts):
            continue
        out.append((name, gold[name]))
    return out


def symbol_tasks(repo: c.Repo, a: argparse.Namespace, exts: tuple[str, ...]) -> list[dict]:
    where = symbol_gold(repo, a.lang)

    def matching(name: str) -> list[tuple[str, int]]:
        return sorted({site for other, sites in where.items() if name in other for site in sites})

    gold = {name: matching(name) for name in where if len(name) >= MIN_NAME and IDENTIFIER.fullmatch(name)}
    tasks = []
    for name, sites in candidates(repo, a, "symbols", gold, exts, lambda _: True):
        tasks.append(
            {
                "id": f"symbols-{len(tasks):04d}",
                "mode": "symbols",
                "args": {"name": name, "limit": 200},
                "gold": [f"{path}:{line}" for path, line in sites],
                "scoring": "set",
                "baseline": {
                    "grep": f"git grep -n -E {c.shell_quote(lg.decl_grep(a.lang, name))} -- "
                    + " ".join(c.shell_quote(g) for g in lg.pathspec(a.lang)),
                    "read": c.read_baseline_files(path for path, _ in sites),
                },
            }
        )
    return tasks


def reference_tasks(repo: c.Repo, a: argparse.Namespace, exts: tuple[str, ...]) -> list[dict]:
    gold = {name: sorted(set(sites)) for name, sites in reference_gold(repo, a.lang, exts).items()}
    tasks = []
    for name, sites in candidates(repo, a, "references", gold, exts, lambda n: len(n) >= MIN_NAME):
        tasks.append(
            {
                "id": f"references-{len(tasks):04d}",
                "mode": "references",
                "args": {"name": name, "limit": 200},
                "gold": [f"{path}:{line}" for path, line in sites],
                "scoring": "set",
                "baseline": {
                    "grep": f"git grep -n -w -F {c.shell_quote(name)} -- "
                    + " ".join(c.shell_quote(g) for g in lg.pathspec(a.lang))
                },
            }
        )
    return tasks


def dependent_tasks(repo: c.Repo, a: argparse.Namespace, exts: tuple[str, ...]) -> list[dict]:
    tasks = []
    for module, paths in candidates(repo, a, "dependents", dependent_gold(repo, a.lang), exts, lambda _: True):
        tasks.append(
            {
                "id": f"dependents-{len(tasks):04d}",
                "mode": "dependents",
                "args": {"name": module, "relation": "importers"},
                "gold": paths,
                "scoring": "set",
                "baseline": {
                    "grep": f"git grep -l -F {c.shell_quote(module)} -- "
                    + " ".join(c.shell_quote(g) for g in lg.pathspec(a.lang))
                },
            }
        )
    return tasks


BUILDERS = {"symbols": symbol_tasks, "references": reference_tasks, "dependents": dependent_tasks}


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    c.add_common_args(p, ext_default="")
    p.add_argument("--lang", required=True, choices=lg.LANGUAGES)
    p.add_argument("--kind", choices=sorted(BUILDERS))
    p.add_argument("--list-kinds", action="store_true", help="print the kinds the language has gold for and exit")
    p.add_argument("--max-gold", type=int, default=40)
    p.add_argument("--no-cross-language-check", action="store_true")
    a = p.parse_args()
    if a.list_kinds:
        print("\n".join(lg.KINDS[a.lang]))
        return
    if a.kind not in lg.KINDS[a.lang]:
        sys.exit(f"gen_structural.py: no `{a.kind}` gold for {a.lang}")

    a.ext = a.ext or lg.EXTENSIONS[a.lang]
    repo = c.Repo(a.repo, a.exclude, a.ext, a.max_bytes)
    exts = tuple("." + e.strip() for e in a.ext.split(","))
    tasks = BUILDERS[a.kind](repo, a, exts)
    if len(tasks) < a.n:
        print(f"only {len(tasks)} of {a.n} requested tasks met the filters", file=sys.stderr)
    c.emit(tasks, a.out)


if __name__ == "__main__":
    main()
