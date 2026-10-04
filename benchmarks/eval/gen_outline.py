"""Gold for `outline`: a file -> the line of every symbol basemind lists for it.

Gold is `path:line` for every def / class / simple assignment target in the file at any depth
(the same extractor as gen_symbols.py). Files are sampled among those with a moderate number
of symbols so the outline is neither trivial nor truncated.
"""

import argparse

import _common as c


def main() -> None:
    p = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    c.add_common_args(p)
    p.add_argument("--min-symbols", type=int, default=3)
    p.add_argument("--max-symbols", type=int, default=60)
    a = p.parse_args()

    repo = c.Repo(a.repo, a.exclude, a.ext, a.max_bytes)
    candidates: dict[str, list[tuple[str, int]]] = {}
    for path, src in repo.blobs(repo.files()):
        tree = c.parse_python(src)
        if tree is None:
            continue
        syms = c.python_symbols(tree)
        if a.min_symbols <= len({(n, line) for n, line in syms}) <= a.max_symbols:
            candidates[path] = syms
    paths = sorted(candidates)
    tasks = []
    for path in c.sample(repo.rng("outline", a.seed), paths, a.n):
        lines = sorted({line for _, line in candidates[path]})
        tasks.append(
            {
                "id": f"outline-{len(tasks):04d}",
                "mode": "outline",
                "args": {"path": path},
                "gold": [f"{path}:{line}" for line in lines],
                "scoring": "set",
                "baseline": {"read": [path]},
            }
        )
    c.emit(tasks, a.out)


if __name__ == "__main__":
    main()
