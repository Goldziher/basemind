"""Gold for `git_search`: words from a recent commit subject -> the commits containing all of them.

basemind's `git search` tokenizes (lowercase, split on non-alphanumerics) and ANDs the query tokens
over commit messages. For each sampled recent commit the query is its `--tokens` rarest subject
tokens (rarest over the scanned history, so the query is near-unique), and the gold is EVERY
commit in the scanned window whose message tokens contain all of them, computed with the same
tokenization. `search` returns commits, not files, so gold items are commit shas; the files the
origin commit changed (`git diff-tree`) ride along in `meta.files` for downstream analysis (the
harness ignores unknown task fields).

Only the newest `--history` commits are scanned; basemind searches all indexed history, so a
query token set that also matched an older commit would cost precision. Rare tokens make that
unlikely; raise `--history` on small repos to remove the gap.
"""

import argparse
import re
from collections import Counter

import _common as c

TOKEN = re.compile(r"[a-z0-9]+")
STOP = {
    "the",
    "and",
    "for",
    "with",
    "from",
    "this",
    "that",
    "into",
    "fix",
    "add",
    "update",
    "remove",
    "use",
    "not",
    "are",
    "was",
    "has",
    "have",
    "when",
    "more",
}


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    c.add_common_args(p, ext_default="")
    p.add_argument(
        "--history",
        type=int,
        default=20000,
        help="newest commits to scan for tokens and gold",
    )
    p.add_argument(
        "--recent",
        type=int,
        default=2000,
        help="sample query commits from the newest N",
    )
    p.add_argument("--tokens", type=int, default=3, help="rare tokens per query")
    p.add_argument("--max-gold", type=int, default=20)
    a = p.parse_args()

    repo = c.Repo(a.repo, a.exclude, a.ext, a.max_bytes)
    raw = repo.git("log", f"-n{a.history}", "--no-merges", "--format=%H%x1f%s%x1f%b%x1e")
    commits = []  # newest first: (sha, subject, token set)
    for rec in raw.split("\x1e"):
        parts = rec.strip("\n").split("\x1f")
        if len(parts) == 3 and parts[0]:
            commits.append(
                (
                    parts[0],
                    parts[1],
                    set(TOKEN.findall((parts[1] + " " + parts[2]).lower())),
                )
            )
    df = Counter(t for _, _, toks in commits for t in toks)
    pool = commits[: a.recent]
    rng = repo.rng("git_search", a.seed)
    tasks = []
    for sha, subject, toks in c.sample(rng, pool, len(pool)):
        if len(tasks) >= a.n:
            break
        subj_tokens = sorted(
            {t for t in TOKEN.findall(subject.lower()) if len(t) >= 4 and t not in STOP and not t.isdigit()}
        )
        if len(subj_tokens) < 2:
            continue
        chosen = sorted(sorted(subj_tokens, key=lambda t: (df[t], t))[: a.tokens])
        gold = sorted(s for s, _, ts in commits if all(t in ts for t in chosen))
        if sha not in gold or len(gold) > a.max_gold:
            continue
        files = repo.git("diff-tree", "--no-commit-id", "--name-only", "-r", "-m", "--root", sha).split()
        tasks.append(
            {
                "id": f"git_search-{len(tasks):04d}",
                "mode": "git_search",
                "args": {"query": " ".join(chosen), "field": "message", "limit": 100},
                "gold": gold,
                "scoring": "ranked",
                "k": 10,
                "meta": {"origin": sha, "subject": subject, "files": sorted(files)},
                "baseline": {
                    "grep": "git log --oneline -i --all-match " + " ".join("--grep=" + c.shell_quote(t) for t in chosen)
                },
            }
        )
    c.emit(tasks, a.out)


if __name__ == "__main__":
    main()
