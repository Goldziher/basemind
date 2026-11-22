"""Gold for `read_around`: a literal pattern -> every line within the context of each match.

The tool answer is one call, `code_grep {pattern, context: 5, language}`: each hit's line and the 5
lines on either side, a line two windows share written once. The gold ranges are built the same way
(and the way `src/engine/ranges.rs` merges read ranges): each hit line `l` widens to `[l - 5, l + 5]`,
clipped to the file, and overlapping or adjacent ranges merge. The gold is every
`path:line` of every range. A pattern is a word (case-sensitive substring) in 1 to `--max-files`
files with at most `--max-hits` hits, the size of a look-around an agent does by hand.

Baseline: what an agent without basemind runs, the literal output of
`rg -n -C5 -F -e PATTERN -g '*.EXT'` in the repository root (no tty, as the harness runs it with
`sh -c`). rg merges overlapping and adjacent windows itself, prints the path on every line
(`path:N:text` for a match, `path-N-text` for a context line) and `--` between groups. `-g '*.EXT'`
keeps the gold's file set (the extension, as the old `git grep -- '*.EXT'` pathspec did); `-t`
would widen `py` to `.pyi` and `ts` to `.tsx`, files the gold never holds. rg skips hidden and
ignored files that `git grep` would search: such a miss is the baseline's recall, not dropped. It
is a reducing kind (gated at 1.4x, ADR-0055), scored as `grep_context`. rg is not pinned by the repository's
mise toolchain, so `run_gate.sh` refuses to run this kind without it; when rg is on PATH this
generator checks each task's rg output against the gold and warns on a difference.
"""

from __future__ import annotations

import argparse
import re
import shutil
import subprocess
import sys
from collections import defaultdict
from pathlib import Path

import _common as c

LANGUAGES = {"py": "python", "ts": "typescript", "js": "javascript", "rs": "rust", "go": "go"}
WORD = re.compile(r"[A-Za-z_][A-Za-z0-9_]{5,}")
CONTEXT = 5


def merged_ranges(hits: list[int], last: int, context: int = CONTEXT) -> list[tuple[int, int]]:
    """Hit lines widened by `context`, clipped to `1..=last`, overlapping and adjacent merged."""
    out: list[tuple[int, int]] = []
    for line in sorted(set(hits)):
        start, end = max(1, line - context), min(last, line + context)
        if out and start <= out[-1][1] + 1:
            out[-1] = (out[-1][0], max(out[-1][1], end))
        else:
            out.append((start, end))
    return out


# A `grep -n -C` row: `path<sep>N<sep>text`, `sep` `:` for a match and `-` for a context line.
CONTEXT_SPLIT = re.compile(r"(?=([:-])(\d+)\1)")


def baseline_command(pattern: str, ext: str, context: int = CONTEXT) -> str:
    return f"rg -n -C{context} -F -e {c.shell_quote(pattern)} -g '*.{ext}'"


def context_items(output: str, root: str) -> list[str]:
    """`path:line` of every row of `rg -n -C` output, split at the earliest `<sep>N<sep>` whose
    prefix is a file under `root` (a path may hold `-`, text may hold `:N:`); `--` rows place
    nothing. The reader `grep_context` in `src/eval/baseline_items.rs` does the same."""
    items, is_file = [], {}
    for row in output.splitlines():
        if row == "--":
            continue
        for m in CONTEXT_SPLIT.finditer(row):
            path = row[: m.start()]
            if path and is_file.setdefault(path, (Path(root) / path).is_file()):
                items.append(f"{path}:{m.group(2)}")
                break
    return items


def run_rg(cmd: str, root: str) -> str:
    proc = subprocess.run(
        ["sh", "-c", cmd],
        cwd=root,
        stdin=subprocess.DEVNULL,
        capture_output=True,
        text=True,
        errors="replace",
        check=False,
    )
    return proc.stdout


def gold_lines(hits: dict[str, list[int]], lines: dict[str, list[str]]) -> list[str]:
    """Every `path:line` of each file's merged ranges."""
    gold = []
    for f, h in hits.items():
        rows = lines[f]
        last = len(rows) - 1 if rows and rows[-1] == "" else len(rows)
        gold.extend(f"{f}:{n}" for start, end in merged_ranges(h, last) for n in range(start, end + 1))
    return gold


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    c.add_common_args(p, ext_default="rs")
    p.add_argument("--max-files", type=int, default=4)
    p.add_argument("--max-hits", type=int, default=12)
    a = p.parse_args()
    if a.ext not in LANGUAGES:
        p.error(f"--ext must be one of {sorted(LANGUAGES)}")

    repo = c.Repo(a.repo, a.exclude, a.ext, a.max_bytes)
    texts = dict(repo.blobs(repo.files()))
    lines = {path: text.split("\n") for path, text in texts.items()}
    word_files: dict[str, set[str]] = defaultdict(set)
    for path, rows in lines.items():
        for row in rows:
            for w in WORD.findall(row):
                word_files[w].add(path)
    words = sorted(w for w, files in word_files.items() if len(files) <= a.max_files)
    rng = repo.rng("read_around", a.seed)
    rg = shutil.which("rg")
    tasks = []
    for w in c.sample(rng, words, len(words)):
        if len(tasks) >= a.n:
            break
        # A substring match, as `code_grep` matches a literal: `w` inside a longer word counts.
        within = sorted(f for f, text in texts.items() if w in text)
        if len(within) > a.max_files:
            continue
        hits = {f: [i for i, row in enumerate(lines[f], start=1) if w in row] for f in within}
        if sum(len(h) for h in hits.values()) > a.max_hits:
            continue
        gold = gold_lines(hits, lines)
        cmd = baseline_command(w, a.ext)
        if rg and sorted(set(context_items(run_rg(cmd, repo.root), repo.root))) != sorted(gold):
            print(f"read_around: rg output for {w!r} differs from the gold (baseline recall)", file=sys.stderr)
        tasks.append(
            {
                "id": f"read_around-{len(tasks):04d}",
                "mode": "read_around",
                "args": {"pattern": w, "context": CONTEXT, "language": LANGUAGES[a.ext]},
                "gold": gold,
                "scoring": "set",
                "meta": {"files": len(hits), "hits": sum(len(h) for h in hits.values())},
                "baseline": {"grep": cmd, "items": "grep_context"},
            }
        )
    c.emit(tasks, a.out)


if __name__ == "__main__":
    main()
