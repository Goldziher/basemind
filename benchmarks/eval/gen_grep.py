"""Gold for `grep`: a regex + path filter -> the `git grep -n -E` lines with the same filters.

Each task pairs a pattern with the filters basemind's `code grep` takes (`language`,
`path_contains`), and the gold is `git grep -n -E PATTERN HEAD -- PATHSPEC` with the equivalent
pathspec: `language: python` <-> `*.py`, `path_contains: D` <-> `*D*`. Patterns are kept to the
subset where POSIX ERE and the Rust regex engine agree (literals and an escaped `\\(`).
"""

import argparse
import re
import sys
from collections import Counter

import _common as c

LANGUAGES = {
    "py": "python",
    "ts": "typescript",
    "tsx": "typescript",
    "js": "javascript",
    "rs": "rust",
    "go": "go",
}
WORD = re.compile(r"[A-Za-z_][A-Za-z0-9_]{5,}")


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    c.add_common_args(p)
    p.add_argument("--max-gold", type=int, default=300)
    p.add_argument(
        "--language",
        help="basemind language filter (default: inferred from --ext when it is a single extension)",
    )
    p.add_argument(
        "--with-path-filter",
        action="store_true",
        help="also emit tasks restricted by path_contains",
    )
    a = p.parse_args()

    repo = c.Repo(a.repo, a.exclude, a.ext, a.max_bytes)
    exts = tuple("." + e.strip() for e in a.ext.split(","))
    language = a.language or (LANGUAGES.get(a.ext.strip()) if "," not in a.ext else None)
    if not language:
        sys.exit("need --language when --ext lists several extensions (grep gold must use identical filters)")

    files = repo.files()
    freq: Counter[str] = Counter()
    for _, src in repo.blobs(files):
        freq.update(set(WORD.findall(src)))
    words = sorted(w for w, k in freq.items() if 1 <= k <= 60)
    rng = repo.rng("grep", a.seed)
    rng.shuffle(words)

    pathspec = ["*" + e for e in exts]
    dirs = sorted({"/".join(f.split("/")[:2]) + "/" for f in files if "/" in f})
    templates = ["{w}", "def {w}", "class {w}", "{w}\\("]
    tasks: list[dict] = []
    for w in words:
        if len(tasks) >= a.n:
            break
        pat = templates[len(tasks) % len(templates)].format(w=w)
        args: dict = {"pattern": pat, "language": language, "limit": 1000}
        if a.with_path_filter and len(tasks) % 2 == 1 and dirs:
            sub = rng.choice(dirs)
            args["path_contains"] = sub
        hits = c.git_grep_lines(repo, "-E", "-e", pat, pathspec=pathspec)
        if "path_contains" in args:  # git pathspecs OR together, so intersect the directory filter here
            hits = [(path, ln) for path, ln in hits if args["path_contains"] in path]
        if not hits or len(hits) > a.max_gold:
            continue
        cmd = "git grep -n -E " + c.shell_quote(pat) + " -- " + " ".join(c.shell_quote(s) for s in pathspec)
        tasks.append(
            {
                "id": f"grep-{len(tasks):04d}",
                "mode": "grep",
                "args": args,
                "gold": [f"{path}:{ln}" for path, ln in hits],
                "scoring": "set",
                "baseline": {"grep": cmd},
            }
        )
    c.emit(tasks, a.out)


if __name__ == "__main__":
    main()
