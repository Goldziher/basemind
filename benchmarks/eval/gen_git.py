"""Git tasks: `git_log`, `git_blame` and `git_diff`, with git's own output on the pinned commit as gold.

Kinds (task `mode`):

  git_path         a file -> every commit `git log -- PATH` lists (default history simplification,
                   no `--follow`), as full shas. Baseline: `git log --follow --oneline -- PATH`.
  git_symbol       a Rust fn/method unique by name in its file -> every non-merge commit touching
                   the file whose first-parent version of the function's tokens differs (added,
                   removed or changed; comments and whitespace do not count), read with `rust_gold`
                   from both blobs. Baseline: `git log -s -L '/fn NAME[(<]/,/^INDENT}/:PATH'`.
  blame            a line span -> `sha:line` per line from `git blame --porcelain -L A,B`.
                   Baseline: `git blame -L A,B -- PATH`.
  blame_symbol     a Rust fn/method -> blame of its span (first non-attribute line to its last
                   line, from `rust_gold`). Baseline: `git blame -L '/fn NAME[(<]/,/^INDENT}/'`.
  diff             a non-merge commit and one Rust file it modified -> `+path:line` per added line
                   and `-path:line` per removed line of `git diff -U0 C^ C -- PATH`. Baseline: the
                   same diff with default context.
  diff_structural  a commit C and a Rust file it modified -> the definition names present at HEAD
                   and not at C^ (`+name`) or the reverse (`-name`), from `rust_gold`. Baseline:
                   `git diff C^ -- PATH`, read for definition lines.

What the truth cannot cover: renames (no `--follow` in `git_path` and `git_symbol`), merge commits
in `git_symbol`, and a symbol's identity across a rename of the function. `git_symbol` compares by
name within the file, so a second same-named function in an older revision makes the comparison a
set of bodies. The `-L` baselines find the function by regex and end it at the first closing brace
at its indentation; where that misplaces the span, the baseline is scored down, not the gold.
"""

from __future__ import annotations

import argparse
import subprocess
import sys
from collections import defaultdict

import _common as c
import _rust as r
import gen_rust

NO_MERGES_LOG = ("log", "--no-merges", "--format=%H")


def full(repo: c.Repo, *args: str) -> list[str]:
    return [line for line in repo.git(*args).splitlines() if line]


def indent_of(src_lines: list[str], line: int) -> str:
    text = src_lines[line - 1]
    return text[: len(text) - len(text.lstrip())]


def range_regex(name: str, indent: str) -> str:
    return f"/fn {name}[(<]/,/^{indent}}}/"


def porcelain_blame(repo: c.Repo, path: str, a: int, b: int) -> list[str]:
    out = repo.git("blame", "--porcelain", "-L", f"{a},{b}", "HEAD", "--", path)
    items = []
    for line in out.splitlines():
        parts = line.split()
        if (
            not line.startswith("\t")
            and len(parts) >= 3
            and len(parts[0]) == 40
            and all(ch in "0123456789abcdef" for ch in parts[0])
        ):
            items.append(f"{parts[0]}:{parts[2]}")
    return items


def diff_items(text: str, path: str) -> list[str]:
    """`+path:N` / `-path:N` for a `-U0` diff of one file."""
    items, old, new = [], 0, 0
    for line in text.splitlines():
        if line.startswith(("+++", "---")):
            continue
        if line.startswith("@@"):
            head = line.split()
            old = int(head[1][1:].split(",")[0])
            new = int(head[2][1:].split(",")[0])
            continue
        if line.startswith("+"):
            items.append(f"+{path}:{new}")
            new += 1
        elif line.startswith("-"):
            items.append(f"-{path}:{old}")
            old += 1
    return items


def blob_records(repo: c.Repo, specs: list[str]) -> dict[str, list[dict]]:
    """`rust_gold` records per `rev:path` blob spec (missing blobs map to [])."""
    blobs = []
    for start in range(0, len(specs), 500):
        batch = specs[start : start + 500]
        out = repo_cat(repo, batch)
        blobs.extend(out)
    recs = r.records(blobs)
    by_spec: dict[str, list[dict]] = defaultdict(list)
    for rec in recs:
        by_spec[rec["path"]].append(rec)
    return by_spec


def repo_cat(repo: c.Repo, specs: list[str]) -> list[tuple[str, str]]:
    proc = subprocess.run(
        ["git", "-C", repo.root, "cat-file", "--batch"],
        input="".join(s + "\n" for s in specs).encode(),
        capture_output=True,
        check=False,
    )
    buf, pos, out = proc.stdout, 0, []
    for spec in specs:
        nl = buf.index(b"\n", pos)
        header = buf[pos:nl].split()
        pos = nl + 1
        if len(header) != 3 or header[1] != b"blob":
            continue
        size = int(header[2])
        out.append((spec, buf[pos : pos + size].decode("utf-8", errors="replace")))
        pos += size + 1
    return out


def fn_bodies(recs: list[dict], name: str) -> tuple[str, ...] | None:
    if any(x["t"] == "error" for x in recs):
        return None
    return tuple(
        sorted(d["body"] for d in recs if d["t"] == "def" and d["name"] == name and d["kind"] in ("fn", "method"))
    )


def gen_git_path(repo: c.Repo, rng, n: int) -> list[dict]:
    files = repo.files(())
    tasks = []
    for path in c.sample(rng, files, len(files)):
        if len(tasks) >= n:
            break
        gold = full(repo, "log", "--format=%H", "HEAD", "--", path)
        if not 2 <= len(gold) <= 80:
            continue
        tasks.append(
            {
                "id": f"git_path-{len(tasks):04d}",
                "mode": "git_path",
                "args": {"path": path, "limit": 100},
                "gold": gold,
                "scoring": "set",
                "baseline": {"grep": f"git log --follow --oneline -- {c.shell_quote(path)}", "items": "shas"},
            }
        )
    return tasks


def gen_symbol_kinds(repo: c.Repo, fns: list[dict], head_src: dict[str, list[str]], rng, n: int) -> list[dict]:
    """`git_symbol`, `blame_symbol` tasks."""
    tasks: list[dict] = []
    hist_count = blame_count = 0
    for d in c.sample(rng, fns, len(fns)):
        if hist_count >= n and blame_count >= n:
            break
        path, name = d["path"], d["name"]
        indent = indent_of(head_src[path], d["line"])
        rx = range_regex(name, indent)
        if blame_count < n:
            tasks.append(
                {
                    "id": f"blame_symbol-{len(tasks):04d}",
                    "mode": "blame_symbol",
                    "args": {"path": path, "symbol": name},
                    "gold": porcelain_blame(repo, path, d["line"], d["end"]),
                    "scoring": "set",
                    "baseline": {
                        "grep": f"git blame -L {c.shell_quote(rx)} -- {c.shell_quote(path)}",
                        "items": "blame",
                    },
                }
            )
            blame_count += 1
        if hist_count >= n:
            continue
        commits = full(repo, *NO_MERGES_LOG, "HEAD", "--", path)
        if not 2 <= len(commits) <= 40:
            continue
        specs = [f"{sha}:{path}" for sha in commits] + [f"{sha}^:{path}" for sha in commits]
        recs = blob_records(repo, specs)
        gold, broken = [], False
        for sha in commits:
            now, before = (
                fn_bodies(recs.get(f"{sha}:{path}", []), name),
                fn_bodies(recs.get(f"{sha}^:{path}", []), name),
            )
            if now is None or before is None:
                broken = True
                break
            if now != before:
                gold.append(sha)
        if broken or not gold:
            continue
        tasks.append(
            {
                "id": f"git_symbol-{len(tasks):04d}",
                "mode": "git_symbol",
                "args": {"path": path, "symbol": name, "limit": 100},
                "gold": gold,
                "scoring": "set",
                "baseline": {
                    "grep": f"git log -s --format=%H -L {c.shell_quote(rx + ':' + path)}",
                    "items": "shas",
                },
            }
        )
        hist_count += 1
    return tasks


def gen_blame(repo: c.Repo, head_src: dict[str, list[str]], rng, n: int) -> list[dict]:
    paths = sorted(p for p, lines in head_src.items() if len(lines) >= 20)
    tasks = []
    for path in c.sample(rng, paths, n):
        total = len(head_src[path])
        a = rng.randrange(1, total - 1)
        b = min(total, a + rng.randrange(0, 30))
        tasks.append(
            {
                "id": f"blame-{len(tasks):04d}",
                "mode": "blame",
                "args": {"path": path, "line": a, "end_line": b},
                "gold": porcelain_blame(repo, path, a, b),
                "scoring": "set",
                "baseline": {"grep": f"git blame -L {a},{b} -- {c.shell_quote(path)}", "items": "blame"},
            }
        )
    return tasks


def modified_rs(repo: c.Repo, sha: str) -> list[str]:
    out = repo.git("diff-tree", "--no-commit-id", "-r", "--diff-filter=M", "--name-only", sha)
    return sorted(p for p in out.splitlines() if p.endswith(".rs") and not repo.excluded(p))


def gen_diffs(repo: c.Repo, head_paths: set[str], rng, n: int, recent: int) -> list[dict]:
    commits = full(repo, *NO_MERGES_LOG, f"-n{recent}", "HEAD")
    tasks: list[dict] = []
    diff_count = structural_count = 0
    for sha in c.sample(rng, commits, len(commits)):
        if diff_count >= n and structural_count >= n:
            break
        paths = modified_rs(repo, sha)
        if not paths:
            continue
        path = rng.choice(paths)
        parent = repo.git("rev-parse", f"{sha}^").strip()
        if diff_count < n:
            gold = diff_items(repo.git("diff", "-U0", parent, sha, "--", path), path)
            if 1 <= len(gold) <= 200:
                tasks.append(
                    {
                        "id": f"diff-{len(tasks):04d}",
                        "mode": "diff",
                        "args": {"path": path, "from": parent, "to": sha},
                        "gold": gold,
                        "scoring": "set",
                        "baseline": {
                            "grep": f"git diff {parent} {sha} -- {c.shell_quote(path)}",
                            "items": "diff",
                        },
                    }
                )
                diff_count += 1
        if structural_count >= n or path not in head_paths:
            continue
        recs = blob_records(repo, [f"{parent}:{path}", f"HEAD:{path}"])
        before, now = recs.get(f"{parent}:{path}", []), recs.get(f"HEAD:{path}", [])
        if any(x["t"] == "error" for x in before + now):
            continue

        def names(rs: list[dict]) -> set[str]:
            return {d["name"] for d in rs if d["t"] == "def" and d["kind"] in r.NAMED_KINDS}

        gold = sorted(f"+{x}" for x in names(now) - names(before)) + sorted(f"-{x}" for x in names(before) - names(now))
        if not 1 <= len(gold) <= 40:
            continue
        tasks.append(
            {
                "id": f"diff_structural-{len(tasks):04d}",
                "mode": "diff_structural",
                "args": {"path": path, "from": parent, "structural": True},
                "gold": gold,
                "scoring": "set",
                "baseline": {"grep": f"git diff {parent} -- {c.shell_quote(path)}", "items": "diff_symbols"},
            }
        )
        structural_count += 1
    return tasks


KINDS = ["git_path", "git_symbol", "blame", "diff"]


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    c.add_common_args(p, ext_default="rs")
    p.add_argument(
        "--kind",
        action="append",
        choices=KINDS,
        help="git_path, git_symbol (+ blame_symbol), blame, diff (+ diff_structural); default all",
    )
    p.add_argument("--recent", type=int, default=1000, help="sample diff commits from the newest N")
    a = p.parse_args()

    repo = c.Repo(a.repo, a.exclude, a.ext, a.max_bytes)
    kinds = a.kind or KINDS
    head = dict(repo.blobs(repo.files()))
    head_src = {path: src.splitlines() for path, src in head.items()}
    tasks: list[dict] = []
    for kind in kinds:
        rng = repo.rng(f"git-{kind}", a.seed)
        if kind == "git_path":
            tasks += gen_git_path(repo, rng, a.n)
        elif kind == "git_symbol":
            recs = r.records(head.items())
            print(r.summary(recs), file=sys.stderr)
            defs = [d for d in r.by_type(recs, "def") if d["kind"] in r.NAMED_KINDS]
            fns = sorted(gen_rust.unique_fns(defs), key=lambda d: (d["path"], d["line"]))
            tasks += gen_symbol_kinds(repo, fns, head_src, rng, a.n)
        elif kind == "blame":
            tasks += gen_blame(repo, head_src, rng, a.n)
        elif kind == "diff":
            tasks += gen_diffs(repo, set(head_src), rng, a.n, a.recent)
    for i, t in enumerate(tasks):
        t["id"] = f"{t['mode']}-{i:04d}"
    c.emit(tasks, a.out)


if __name__ == "__main__":
    main()
