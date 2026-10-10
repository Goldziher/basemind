"""Shared helpers for the gold generators (stdlib only).

Determinism contract: for a fixed HEAD sha, `--seed` and arguments, every generator emits
byte-identical JSONL. Everything is read from the HEAD tree (never the working tree), every
candidate list is sorted before sampling, and the RNG is seeded from `"<head>:<seed>:<name>"`.
"""

from __future__ import annotations

import argparse
import ast
import json
import random
import re
import subprocess
import sys
import warnings
from collections.abc import Iterable, Iterator
from pathlib import Path

warnings.simplefilter("ignore")  # old code trips SyntaxWarning (invalid escapes) on parse

DEFAULT_MAX_BYTES = 512 * 1024  # skip generated/minified giants; basemind skips oversized files too


def add_common_args(p: argparse.ArgumentParser, *, ext_default: str = "py") -> None:
    p.add_argument("--repo", default=".", help="git repository to generate gold from (default: .)")
    p.add_argument("--seed", type=int, default=0, help="sampling seed (default 0)")
    p.add_argument("--n", type=int, default=50, help="number of tasks to emit (default 50)")
    p.add_argument(
        "--exclude",
        action="append",
        default=[],
        metavar="GLOB",
        help="repo-relative glob to leave out (repeatable; '**' crosses dirs, a glob without '/' "
        "matches at any depth). Keep these identical to your basemind.toml scan excludes.",
    )
    p.add_argument(
        "--ext",
        default=ext_default,
        help=f"comma-separated file extensions to consider (default {ext_default})",
    )
    p.add_argument(
        "--max-bytes",
        type=int,
        default=DEFAULT_MAX_BYTES,
        help="skip blobs larger than this",
    )
    p.add_argument("--out", default="-", help="output JSONL path (default stdout)")


def _glob_to_regex(glob: str) -> re.Pattern[str]:
    i, out, n = 0, [], len(glob)
    while i < n:
        c = glob[i]
        if c == "*":
            if glob[i : i + 3] == "**/":
                out.append("(?:.*/)?")
                i += 3
                continue
            if glob[i : i + 2] == "**":
                out.append(".*")
                i += 2
                continue
            out.append("[^/]*")
        elif c == "?":
            out.append("[^/]")
        else:
            out.append(re.escape(c))
        i += 1
    body = "".join(out)
    if "/" not in glob:
        body = "(?:.*/)?" + body
    return re.compile(body)


class Excluder:
    """Matches a path when the glob matches it or any of its parent directories."""

    def __init__(self, globs: Iterable[str]):
        self._res = [_glob_to_regex(g.strip("/")) for g in globs]

    def __call__(self, path: str) -> bool:
        if not self._res:
            return False
        parts = path.split("/")
        prefixes = ["/".join(parts[: i + 1]) for i in range(len(parts))]
        return any(r.fullmatch(p) for r in self._res for p in prefixes)


class Repo:
    def __init__(
        self,
        root: str,
        exclude: Iterable[str] = (),
        exts: str = "",
        max_bytes: int = DEFAULT_MAX_BYTES,
    ):
        self.root = str(Path(root).resolve())
        self.head = self.git("rev-parse", "HEAD").strip()
        self.excluded = Excluder(exclude)
        self.exts = tuple("." + e.strip().lstrip(".") for e in exts.split(",") if e.strip())
        self.max_bytes = max_bytes
        self._all: list[str] | None = None

    def git(self, *args: str, check: bool = True) -> str:
        proc = subprocess.run(
            ["git", "-C", self.root, *args],
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
        )
        if check and proc.returncode not in (0, 1):
            sys.exit(f"git {' '.join(args)} failed: {proc.stderr.strip()}")
        return proc.stdout

    def all_files(self) -> list[str]:
        """Every non-excluded tracked file at HEAD, sorted."""
        if self._all is None:
            raw = self.git("ls-tree", "-r", "-z", "--name-only", "HEAD")
            self._all = sorted(p for p in raw.split("\0") if p and not self.excluded(p))
        return self._all

    def files(self, exts: tuple[str, ...] | None = None) -> list[str]:
        exts = self.exts if exts is None else exts
        return [p for p in self.all_files() if not exts or p.endswith(exts)]

    def blobs(self, paths: list[str], chunk: int = 1000) -> Iterator[tuple[str, str]]:
        """Yield `(path, text)` from the HEAD tree; oversized, binary and undecodable blobs are skipped."""
        for start in range(0, len(paths), chunk):
            batch = paths[start : start + chunk]
            stdin = "".join(f"HEAD:{p}\n" for p in batch).encode()
            proc = subprocess.run(
                ["git", "-C", self.root, "cat-file", "--batch"],
                input=stdin,
                capture_output=True,
                check=False,
            )
            buf, pos = proc.stdout, 0
            for path in batch:
                nl = buf.index(b"\n", pos)
                header = buf[pos:nl].split()
                pos = nl + 1
                if len(header) != 3 or header[1] != b"blob":
                    continue
                size = int(header[2])
                data = buf[pos : pos + size]
                pos += size + 1
                if size > self.max_bytes or b"\0" in data[:8192]:
                    continue
                try:
                    yield path, data.decode("utf-8")
                except UnicodeDecodeError:
                    continue

    def rng(self, name: str, seed: int) -> random.Random:
        return random.Random(f"{self.head}:{seed}:{name}")  # str seeds are hashed with sha512: stable across runs


def parse_python(src: str) -> ast.AST | None:
    try:
        return ast.parse(src)
    except (SyntaxError, ValueError, RecursionError, MemoryError):
        return None


def python_symbols(tree: ast.AST, *, assignments: bool = True) -> list[tuple[str, int]]:
    """`(name, 1-based line)` for everything basemind's Python outline lists.

    Module- and class-scope only: every def/async def/class, and every simple `Name` target of
    `=` / annotated-with-value `:` assignment, at module level or in a class body (including inside
    module-level `if`/`try`/`with`/loops). Function-local variables and nested definitions are
    implementation detail and are not indexed, so the walk never descends into a function or
    lambda body. Tuple-unpacking, bare annotations and `self.x` are not listed. A decorated def is
    reported at its `def` line, which is what `ast` calls `lineno`. `assignments=False` keeps only
    the def/class names.
    """
    out: list[tuple[str, int]] = []

    def visit(node: ast.AST) -> None:
        for child in ast.iter_child_nodes(node):
            if isinstance(child, (ast.FunctionDef, ast.AsyncFunctionDef)):
                out.append((child.name, child.lineno))
                continue  # body is function scope
            if isinstance(child, ast.Lambda):
                continue
            if isinstance(child, ast.ClassDef):
                out.append((child.name, child.lineno))
            elif not assignments:
                pass
            elif isinstance(child, ast.Assign):
                out.extend((t.id, t.lineno) for t in child.targets if isinstance(t, ast.Name))
            elif isinstance(child, ast.AnnAssign) and child.value is not None and isinstance(child.target, ast.Name):
                out.append((child.target.id, child.target.lineno))
            visit(child)

    visit(tree)
    return out


def shell_quote(s: str) -> str:
    return "'" + s.replace("'", "'\\''") + "'"


def git_grep_lines(repo: Repo, *args: str, pathspec: list[str] | None = None) -> list[tuple[str, int]]:
    """`(path, line)` of `git grep -n` matches in the HEAD tree, sorted."""
    cmd = ["grep", "-n", "-I", *args, "HEAD"]
    if pathspec:
        cmd += ["--", *pathspec]
    hits = []
    for line in repo.git(*cmd, check=False).splitlines():
        m = re.match(r"HEAD:(.*?):(\d+):", line)
        if m and not repo.excluded(m.group(1)):
            hits.append((m.group(1), int(m.group(2))))
    return sorted(hits)


def appears_outside(repo: Repo, needle: str, exts: tuple[str, ...]) -> bool:
    """True when fixed-string `needle` occurs in a tracked file of another language.

    basemind indexes every language, so a Python-derived gold is only complete for names that
    do not also show up elsewhere (markdown headings and TypeScript definitions are symbols too).
    """
    pathspec = [":(exclude)*" + e for e in exts]
    out = repo.git(
        "grep",
        "-l",
        "-I",
        "-F",
        "-e",
        needle,
        "HEAD",
        "--",
        ".",
        *pathspec,
        check=False,
    )
    return any(not repo.excluded(line.removeprefix("HEAD:")) for line in out.splitlines())


def emit(tasks: list[dict], out: str) -> None:
    lines = "".join(json.dumps(t, sort_keys=True, ensure_ascii=False) + "\n" for t in tasks)
    if out == "-":
        sys.stdout.write(lines)
    else:
        Path(out).write_text(lines, encoding="utf-8")
    print(f"wrote {len(tasks)} tasks", file=sys.stderr)


def sample(rng: random.Random, items: list, n: int) -> list:
    """Deterministic sample of up to `n` from a (sorted) list."""
    return rng.sample(items, min(n, len(items)))


def read_baseline_files(paths: Iterable[str], limit: int = 3) -> list[str]:
    seen: list[str] = []
    for p in paths:
        if p not in seen:
            seen.append(p)
    return seen[:limit]
