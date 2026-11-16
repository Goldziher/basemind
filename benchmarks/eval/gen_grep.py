"""Gold for `grep`: a pattern + path filter -> the `git grep -n` lines with the same filters.

Each task pairs a pattern with the filters `code_grep` takes (`language`, `path`). Tasks alternate
between literal patterns (`code_grep` without `regex`; gold `git grep -n -F`) and regex ones
(`regex: true`; gold `git grep -n -E`), recorded as `meta.variant`. The gold is
`git grep -n PATTERN HEAD -- PATHSPEC` with the equivalent pathspec: `language: python` <->
`*.py`, `path: D` <-> `*D*` (`path` is a substring of the repo-relative path, not a glob).
Regex patterns are kept to the subset where POSIX ERE and the Rust regex engine agree (literals,
alternation, a bracket class and escaped `\\.` / `\\(`).

For Rust (`--ext rs`) each task also carries `annotations`: for every gold line inside a
definition, the name of the innermost one (from `rust_gold`: fn, method, type, const, module, or
an impl block named after its type), so the symbol `code_grep` attaches to a hit is scored too.
A line outside every definition has no annotation and is not scored for it.
"""

import argparse
import re
import sys
from collections import Counter, defaultdict

import _common as c
import _rust as r

LANGUAGES = {
    "py": "python",
    "ts": "typescript",
    "tsx": "typescript",
    "js": "javascript",
    "rs": "rust",
    "go": "go",
}
WORD = re.compile(r"[A-Za-z_][A-Za-z0-9_]{5,}")
# (pattern template, regex?) per language; tasks cycle through them.
TEMPLATES = {
    "python": [("{w}", False), ("def {w}", True), ("class {w}", True), ("{w}\\(", True)],
    "rust": [
        ("{w}", False),
        ("(fn|struct|enum|trait|const) {w}", True),
        ("{w}(", False),
        ("{w}[(<:]", True),
        ("\\.{w}\\(", True),
    ],
}


def enclosing(repo: c.Repo, files: list[str]) -> dict[str, list[tuple[int, int, str]]]:
    """Per file, `(first line, last line, name)` of every definition and impl block."""
    spans: dict[str, list[tuple[int, int, str]]] = defaultdict(list)
    for d in r.by_type(r.records(repo.blobs(files)), "def"):
        if d["kind"] in r.NAMED_KINDS or d["kind"] == "impl":
            spans[d["path"]].append((d["line"], d["end"], d["name"]))
    return spans


def innermost(spans: list[tuple[int, int, str]], line: int) -> str | None:
    inside = [s for s in spans if s[0] <= line <= s[1]]
    return min(inside, key=lambda s: s[1] - s[0])[2] if inside else None


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
        help="also emit tasks restricted by the `path` substring filter",
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
    templates = TEMPLATES.get(language, TEMPLATES["python"])
    spans = enclosing(repo, files) if language == "rust" else {}
    tasks: list[dict] = []
    for w in words:
        if len(tasks) >= a.n:
            break
        template, regex = templates[len(tasks) % len(templates)]
        pat = template.format(w=w)
        flag = "-E" if regex else "-F"
        args: dict = {"pattern": pat, "language": language, "limit": 1000}
        if regex:
            args["regex"] = True
        if a.with_path_filter and len(tasks) % 2 == 1 and dirs:
            sub = rng.choice(dirs)
            args["path"] = sub
        hits = c.git_grep_lines(repo, flag, "-e", pat, pathspec=pathspec)
        if "path" in args:  # git pathspecs OR together, so intersect the directory filter here
            hits = [(path, ln) for path, ln in hits if args["path"] in path]
        if not hits or len(hits) > a.max_gold:
            continue
        cmd = f"git grep -n {flag} -e " + c.shell_quote(pat) + " -- " + " ".join(c.shell_quote(s) for s in pathspec)
        task = {
            "id": f"grep-{len(tasks):04d}",
            "mode": "grep",
            "args": args,
            "gold": [f"{path}:{ln}" for path, ln in hits],
            "scoring": "set",
            "meta": {"variant": "regex" if regex else "literal"},
            "baseline": {"grep": cmd, "items": "grep_lines"},
        }
        if spans:
            notes = {f"{p}:{ln}": innermost(spans[p], ln) for p, ln in hits}
            task["annotations"] = {k: v for k, v in notes.items() if v}
        tasks.append(task)
    c.emit(tasks, a.out)


if __name__ == "__main__":
    main()
