"""Gold for `find_large`: a line threshold (and a directory) -> every file at least that long.

`find_files {min_lines, language, path_prefix?}` (#127) lists the indexed files with at least
`min_lines` lines, largest first. A file's line count follows the scanner's convention
(`src/scanner_lines.rs`): every `\\n` ends a line and a final line without one still counts, so
`a\\nb` has 2 lines where `wc -l` says 1. Thresholds are round numbers from 200 to 10000, over the
whole repository and over each top-level directory with enough files; a task is kept when its gold
holds 1 to `--max-gold` files. Files over `--max-bytes` are not indexed, so they are not gold.

Baseline (the plan's `git ls-files | xargs wc -l | sort -rn | awk '$1>=N'`, normalised): `awk`
prints `<lines> <path>` per file counting records, which counts an unterminated final line as the
scanner does, where `wc -l` would not. Scored as paths through the `counted_paths` reader.
"""

from __future__ import annotations

import argparse
from collections import Counter

import _common as c

LANGUAGES = {"py": "python", "ts": "typescript", "js": "javascript", "rs": "rust", "go": "go"}
THRESHOLDS = [200, 300, 500, 750, 1000, 1500, 2000, 3000, 5000, 10000]
# Per file `<lines> <path>`; awk counts a final line without `\n` as a record.
AWK_COUNT = 'awk \'FNR==1&&NR>1{print n" "f} {n=FNR;f=FILENAME} END{if(NR)print n" "f}\''


def count_lines(text: str) -> int:
    """`src/scanner_lines.rs::count_lines` on the UTF-8 text."""
    return text.count("\n") + (1 if text and not text.endswith("\n") else 0)


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    c.add_common_args(p, ext_default="rs")
    p.add_argument("--max-gold", type=int, default=80)
    p.add_argument("--min-dir-files", type=int, default=20, help="least files a directory needs for its own tasks")
    a = p.parse_args()
    if a.ext not in LANGUAGES:
        p.error(f"--ext must be one of {sorted(LANGUAGES)}")

    repo = c.Repo(a.repo, a.exclude, a.ext, a.max_bytes)
    lines = {path: count_lines(text) for path, text in repo.blobs(repo.files())}
    per_dir = Counter(path.split("/", 1)[0] for path in lines if "/" in path)
    scopes = [""] + sorted(d for d, n in per_dir.items() if n >= a.min_dir_files)

    candidates = []
    for scope in scopes:
        inside = {p: n for p, n in lines.items() if not scope or p.startswith(scope + "/")}
        for threshold in THRESHOLDS:
            gold = sorted(p for p, n in inside.items() if n >= threshold)
            if 1 <= len(gold) <= a.max_gold:
                candidates.append((scope, threshold, gold))
    rng = repo.rng("find_large", a.seed)
    tasks = []
    for scope, threshold, gold in sorted(c.sample(rng, candidates, a.n), key=lambda t: (t[0], t[1])):
        args = {"min_lines": threshold, "language": LANGUAGES[a.ext]}
        if scope:
            args["path_prefix"] = scope + "/"
        spec = c.shell_quote(f"{scope}/*.{a.ext}" if scope else f"*.{a.ext}")
        cmd = f"git ls-files -z -- {spec} | xargs -0 {AWK_COUNT} | sort -rn | awk '$1>={threshold}'"
        tasks.append(
            {
                "id": f"find_large-{len(tasks):04d}",
                "mode": "find_large",
                "args": args,
                "gold": gold,
                "scoring": "set",
                "meta": {"variant": "dir" if scope else "repo"},
                "baseline": {"grep": cmd, "items": "counted_paths"},
            }
        )
    c.emit(tasks, a.out)


if __name__ == "__main__":
    main()
