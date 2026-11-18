"""Gold for `read_around`: a literal pattern -> every line within the context of each match.

The tool answer is two steps (#129): `code_grep {pattern, ranges: true, language}` returns, per file,
the hits merged into line ranges, and `code_read {path, ranges: "a-b,c-d"}` reads them. The gold
ranges are built the way `src/engine/ranges.rs` builds them: each hit line `l` widens to
`[l - 5, l + 5]`, clipped to the file, and overlapping or adjacent ranges merge. The gold is every
`path:line` of every range. A pattern is a word (case-sensitive substring) in 1 to `--max-files`
files with at most `--max-hits` hits, the size of a look-around an agent does by hand.

Baseline (the plan's): `git grep -n -F` for the pattern, then one `sed -n` per file printing the
same ranges. It is a reducing kind (gated at 1.5x), charged for tokens only: by construction the
baseline prints exactly the gold lines.
"""

from __future__ import annotations

import argparse
import re
from collections import defaultdict

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
        gold, reads = [], []
        for f, h in hits.items():
            rows = lines[f]
            last = len(rows) - 1 if rows and rows[-1] == "" else len(rows)
            ranges = merged_ranges(h, last)
            gold.extend(f"{f}:{n}" for start, end in ranges for n in range(start, end + 1))
            sed = " ".join(f"-e '{start},{end}p'" for start, end in ranges)
            reads.append(f"sed -n {sed} {c.shell_quote(f)}")
        grep = "git grep -n -F -e " + c.shell_quote(w) + f" -- '*.{a.ext}'"
        tasks.append(
            {
                "id": f"read_around-{len(tasks):04d}",
                "mode": "read_around",
                "args": {"pattern": w, "ranges": True, "language": LANGUAGES[a.ext]},
                "gold": gold,
                "scoring": "set",
                "meta": {"files": len(hits), "hits": sum(len(h) for h in hits.values())},
                "baseline": {"grep": [grep, *reads]},
            }
        )
    c.emit(tasks, a.out)


if __name__ == "__main__":
    main()
