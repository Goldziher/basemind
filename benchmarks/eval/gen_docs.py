"""Gold for `docs`: a markdown sentence naming code -> the markdown file it is in.

Sentences that name a backticked path or symbol (`pkg/widget.py`, `make_widget`) are documentation
about that code. The query is the sentence with the backticks removed and gold is the markdown
file holding it; scoring is `ranked` (hit@1/@5, MRR, nDCG). Sentences that occur in more than one
file are dropped (ambiguous gold). `docs` needs a basemind built with the `documents` feature and a
scan that indexed the markdown files.
"""

import argparse
import re
from collections import Counter

import _common as c

SENTENCE = re.compile(r"(?<=[.!?])\s+")
TICKED = re.compile(r"`([^`\n]+)`")
CODEISH = re.compile(r"^[\w./-]+(\.\w+|/[\w./-]*)$|^[A-Za-z_]\w{4,}(\(\))?$")


def sentences(md: str) -> list[str]:
    md = re.sub(
        r"```.*?```", " ", md, flags=re.DOTALL
    )  # fenced blocks are code, not prose
    out = []
    for block in re.split(r"\n\s*\n", md):
        if block.lstrip().startswith(("|", "#", "<", "-", "*", ">", "[")):
            continue
        flat = " ".join(block.split())
        out.extend(s.strip() for s in SENTENCE.split(flat))
    return out


def main() -> None:
    p = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    c.add_common_args(p, ext_default="md")
    p.add_argument("--min-words", type=int, default=8)
    p.add_argument("--max-words", type=int, default=40)
    p.add_argument("--k", type=int, default=5)
    a = p.parse_args()

    repo = c.Repo(a.repo, a.exclude, a.ext, a.max_bytes)
    found: dict[str, list[str]] = {}  # sentence -> files
    for path, md in repo.blobs(repo.files()):
        for s in sentences(md):
            words = len(s.split())
            ticked = TICKED.findall(s)
            if a.min_words <= words <= a.max_words and any(
                CODEISH.match(t) for t in ticked
            ):
                found.setdefault(s, []).append(path)
    unique = sorted((s, fs[0]) for s, fs in found.items() if len(set(fs)) == 1)
    counts = Counter(p for _, p in unique)
    rng = repo.rng("docs", a.seed)
    tasks = []
    for sentence, path in c.sample(rng, unique, len(unique)):
        if len(tasks) >= a.n:
            break
        if counts[path] > 50:  # one chatty file should not dominate
            continue
        query = re.sub(r"\[([^\]]+)\]\([^)]*\)", r"\1", sentence)  # [text](url) -> text
        query = re.sub(r"[`*]", "", query)
        word = max(
            re.findall(r"[A-Za-z_]{5,}", query) or [""], key=lambda w: (len(w), w)
        )
        task = {
            "id": f"docs-{len(tasks):04d}",
            "mode": "docs",
            "args": {"query": query},
            "gold": [path],
            "scoring": "ranked",
            "k": a.k,
        }
        if word:
            # Without semantic search an agent greps a keyword, then opens the document the hit
            # points at: grep alone returns bare lines, not the passage the question needs.
            task["baseline"] = {
                "grep": "git grep -n -i -w " + c.shell_quote(word) + " -- '*.md'",
                "read": [path],
            }
        tasks.append(task)
    c.emit(tasks, a.out)


if __name__ == "__main__":
    main()
