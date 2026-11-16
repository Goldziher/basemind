"""Gold for `docs` under a metadata filter: a document sentence plus its file's `mime_type`.

The query and gold are `gen_docs.py`'s (a sentence naming backticked code -> the file holding it),
drawn from every document extension given, and each task also passes `mime_type` set to the type
basemind stores for the gold file. The filter therefore never excludes the gold, so a miss means the
filter cost recall: it was applied after the candidate cut instead of before ranking. Scoring is
`ranked`; the gate reads `mean_recall`.

The MIME type comes from the extension, as basemind's scan detects it (xberg `core/mime.rs`). Run it
on a repository with at least two document types, or every filter keeps the whole corpus; the
per-type task counts are printed to stderr so that is visible.
"""

import argparse
import re
import sys
from collections import Counter

import _common as c
import gen_docs

MIME_BY_EXT = {
    "md": "text/markdown",
    "markdown": "text/markdown",
    "mdx": "text/mdx",
    "txt": "text/plain",
    "rst": "text/prs.fallenstein.rst",
    "html": "text/html",
    "htm": "text/html",
}
MAX_SENTENCES_PER_FILE = 50


def mime_of(path: str, extra: dict[str, str]) -> str | None:
    ext = path.rsplit(".", 1)[-1].lower() if "." in path else ""
    return extra.get(ext) or MIME_BY_EXT.get(ext)


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    c.add_common_args(p, ext_default="md,markdown,mdx,txt,rst")
    p.add_argument("--min-words", type=int, default=8)
    p.add_argument("--max-words", type=int, default=40)
    p.add_argument("--k", type=int, default=5)
    p.add_argument(
        "--mime",
        action="append",
        default=[],
        metavar="EXT=TYPE",
        help="MIME type for an extension, overriding or extending the built-in table (repeatable)",
    )
    a = p.parse_args()
    extra = dict(pair.split("=", 1) for pair in a.mime)

    repo = c.Repo(a.repo, a.exclude, a.ext, a.max_bytes)
    found: dict[str, list[str]] = {}
    for path, text in repo.blobs(repo.files()):
        if mime_of(path, extra) is None:
            continue
        for s in gen_docs.sentences(text):
            ticked = gen_docs.TICKED.findall(s)
            if a.min_words <= len(s.split()) <= a.max_words and any(gen_docs.CODEISH.match(t) for t in ticked):
                found.setdefault(s, []).append(path)
    unique = sorted((s, fs[0]) for s, fs in found.items() if len(set(fs)) == 1)
    counts = Counter(path for _, path in unique)
    rng = repo.rng("docs_filtered", a.seed)
    tasks = []
    for sentence, path in c.sample(rng, unique, len(unique)):
        if len(tasks) >= a.n:
            break
        if counts[path] > MAX_SENTENCES_PER_FILE:  # one chatty file should not dominate
            continue
        query = re.sub(r"\[([^\]]+)\]\([^)]*\)", r"\1", sentence)
        query = re.sub(r"[`*]", "", query)
        tasks.append(
            {
                "id": f"docs-filtered-{len(tasks):04d}",
                "mode": "docs",
                "args": {"query": query, "mime_type": mime_of(path, extra)},
                "gold": [path],
                "scoring": "ranked",
                "k": a.k,
            }
        )
    by_type = Counter(t["args"]["mime_type"] for t in tasks)
    print("tasks per mime_type: " + ", ".join(f"{m}={n}" for m, n in sorted(by_type.items())), file=sys.stderr)
    c.emit(tasks, a.out)


if __name__ == "__main__":
    main()
