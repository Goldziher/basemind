"""Gold for `docs`, non-verbatim: a markdown heading trail -> the markdown file it is in.

`gen_docs.py` queries with a sentence copied from the file, which any lexical lane answers by exact
match. Here the query is the trail of headings above a section ("Operator guide Setup Install
prerequisites"): its words are spread over several heading lines and never occur in the file as one
run, so the task measures whether retrieval finds a section by topic. Gold is the markdown file
holding the trail; scoring is `ranked`.

Only trails at least `--min-depth` headings deep and `--min-words` words long are used, since a lone
"Usage" names no file. A trail found in more than one file is dropped (ambiguous gold), as is a
trail whose words occur consecutively in any body line of its own file (that would be verbatim).
"""

import argparse
import re
from collections import Counter

import _common as c

HEADING = re.compile(r"^(#{1,6})[ \t]+(.+?)[ \t]*#*[ \t]*$")
FENCE = re.compile(r"^\s*(```|~~~)")
WORD = re.compile(r"[A-Za-z]{3,}")
MAX_TASKS_PER_FILE = 10


def plain(text: str) -> str:
    text = re.sub(r"!?\[([^\]]*)\]\([^)]*\)", r"\1", text)  # links and images keep their label
    text = re.sub(r"<[^>]+>", " ", text)  # inline HTML such as anchors
    text = re.sub(r"[`*_~]", "", text)
    return " ".join(text.split())


def trails(md: str) -> list[tuple[str, ...]]:
    """Every heading trail in document order, as tuples of plain heading texts (outermost first)."""
    out: list[tuple[str, ...]] = []
    stack: list[tuple[int, str]] = []
    fenced = False
    for line in md.splitlines():
        if FENCE.match(line):
            fenced = not fenced
            continue
        m = None if fenced else HEADING.match(line)
        if not m:
            continue
        level, title = len(m.group(1)), plain(m.group(2))
        if not title:
            continue
        while stack and stack[-1][0] >= level:
            stack.pop()
        stack.append((level, title))
        out.append(tuple(t for _, t in stack))
    return out


def body_lines(md: str) -> list[str]:
    fenced, out = False, []
    for line in md.splitlines():
        if FENCE.match(line):
            fenced = not fenced
        elif not fenced and not HEADING.match(line):
            out.append(" ".join(plain(line).lower().split()))
    return out


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    c.add_common_args(p, ext_default="md")
    p.add_argument("--min-depth", type=int, default=2, help="fewest headings in a trail (default 2)")
    p.add_argument("--min-words", type=int, default=4, help="fewest words in a query (default 4)")
    p.add_argument("--max-words", type=int, default=24, help="most words in a query (default 24)")
    p.add_argument("--k", type=int, default=5)
    a = p.parse_args()

    repo = c.Repo(a.repo, a.exclude, a.ext, a.max_bytes)
    found: dict[str, list[tuple[str, tuple[str, ...]]]] = {}
    for path, md in repo.blobs(repo.files()):
        body = body_lines(md)
        for trail in sorted(set(trails(md))):
            query = " ".join(trail)
            words = len(query.split())
            if len(trail) < a.min_depth or not a.min_words <= words <= a.max_words:
                continue
            if not all(WORD.search(heading) for heading in trail):
                continue  # a version or date heading ("[0.15.0] - 2026-07-02") names no topic
            if any(query.lower() in line for line in body):
                continue
            found.setdefault(query, []).append((path, trail))
    unique = sorted((q, hits[0][0], hits[0][1]) for q, hits in found.items() if len({h[0] for h in hits}) == 1)
    rng = repo.rng("docs_headings", a.seed)
    per_file: Counter[str] = Counter()
    tasks = []
    for query, path, trail in c.sample(rng, unique, len(unique)):
        if len(tasks) >= a.n:
            break
        if per_file[path] >= MAX_TASKS_PER_FILE:  # one long document should not dominate
            continue
        per_file[path] += 1
        task = {
            "id": f"docs-headings-{len(tasks):04d}",
            "mode": "docs",
            "args": {"query": query},
            "gold": [path],
            "scoring": "ranked",
            "k": a.k,
        }
        word = max(re.findall(r"[A-Za-z_]{5,}", trail[-1]) or [""], key=lambda w: (len(w), w))
        if word:
            # Without semantic search an agent greps a word of the section title, then opens the file.
            task["baseline"] = {
                "grep": "git grep -n -i -w " + c.shell_quote(word) + " -- '*.md'",
                "read": [path],
            }
        tasks.append(task)
    c.emit(tasks, a.out)


if __name__ == "__main__":
    main()
