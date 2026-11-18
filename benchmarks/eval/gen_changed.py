"""Gold for `grep_changed`: a literal pattern scoped to a change -> its lines in the changed files.

`code_grep {pattern, changed, language}` (#122) searches only the files that differ between the
rev `changed` and the working tree. On the corpus (a clean checkout of HEAD) that is the diff
`REV..HEAD`, so each task picks `REV = HEAD~k` along the first-parent chain (k = 1, 2, 3, 5, 8, ...)
and passes its full sha. The gold is every `path:line` of a changed file that exists at HEAD and
contains the pattern (case-sensitive substring, as `code_grep` matches a literal). A pattern is a
word that also occurs outside the changed files, so the scope is what the task tests.

Baseline (the plan's): `git diff --name-only REV | xargs git grep -n -F PATTERN`, restricted to the
language, scored as `path:line` lines.
"""

from __future__ import annotations

import argparse
import re
from collections import defaultdict

import _common as c

LANGUAGES = {"py": "python", "ts": "typescript", "js": "javascript", "rs": "rust", "go": "go"}
WORD = re.compile(r"[A-Za-z_][A-Za-z0-9_]{5,}")


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    c.add_common_args(p, ext_default="rs")
    p.add_argument("--max-files", type=int, default=40, help="largest change (in files) to draw from")
    p.add_argument("--max-gold", type=int, default=40)
    p.add_argument("--per-rev", type=int, default=5, help="most tasks per rev")
    a = p.parse_args()
    if a.ext not in LANGUAGES:
        p.error(f"--ext must be one of {sorted(LANGUAGES)}")
    language = LANGUAGES[a.ext]

    repo = c.Repo(a.repo, a.exclude, a.ext, a.max_bytes)
    texts = dict(repo.blobs(repo.files()))
    chain = repo.git("rev-list", "--first-parent", "--max-count=200", "HEAD").split()
    steps, k = [], 1
    while k < len(chain):
        steps.append(k)
        k = k + 1 if k < 3 else int(k * 1.6) + 1

    # Every file a word occurs in, to require an occurrence outside the change.
    word_files: dict[str, set[str]] = defaultdict(set)
    for path, text in texts.items():
        for w in set(WORD.findall(text)):
            word_files[w].add(path)

    rng = repo.rng("grep_changed", a.seed)
    tasks: list[dict] = []
    for k in steps:
        if len(tasks) >= a.n:
            break
        rev = chain[k]
        listed = repo.git("diff", "--name-only", "--diff-filter=d", rev, "HEAD", "--", f"*.{a.ext}").split("\n")
        changed = sorted(f for f in listed if f in texts)
        if not changed or len(changed) > a.max_files:
            continue
        scoped = set(changed)
        words = sorted({w for f in changed for w in WORD.findall(texts[f]) if word_files[w] - scoped})
        made = 0
        for w in c.sample(rng, words, len(words)):
            if made >= a.per_rev or len(tasks) >= a.n:
                break
            gold = [f"{f}:{i}" for f in changed for i, line in enumerate(texts[f].split("\n"), start=1) if w in line]
            if not 1 <= len(gold) <= a.max_gold:
                continue
            cmd = (
                f"git diff -z --name-only --diff-filter=d {rev} -- '*.{a.ext}' | xargs -0 git grep -n -F -e "
                + c.shell_quote(w)
                + " --"
            )
            tasks.append(
                {
                    "id": f"grep_changed-{len(tasks):04d}",
                    "mode": "grep_changed",
                    "args": {"pattern": w, "changed": rev, "language": language},
                    "gold": gold,
                    "scoring": "set",
                    "meta": {"commits_back": k, "changed_files": len(changed)},
                    "baseline": {"grep": cmd, "items": "grep_lines"},
                }
            )
            made += 1
    c.emit(tasks, a.out)


if __name__ == "__main__":
    main()
