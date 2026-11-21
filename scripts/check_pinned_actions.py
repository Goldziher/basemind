"""Fail when a workflow that can publish references an action by a movable tag or branch.

Usage:
    python3 scripts/check_pinned_actions.py [WORKFLOW ...]

The publish workflows hold ``id-token: write`` and registry publish rights, so a force-pushed tag
of any action they run would publish with those rights. Every ``uses:`` in them, and in the local
actions and reusable workflows they call (followed transitively), must name a full commit SHA with
the tag it was resolved from in a trailing comment: ``owner/repo@<40 hex> # v1.2.3``. Dependabot
keeps those pins current. With no arguments the publish workflows are checked.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PUBLISH_WORKFLOWS = [
    ".github/workflows/publish.yaml",
    ".github/workflows/release-build.yaml",
    ".github/workflows/plugin-artifacts.yaml",
]
USES = re.compile(
    r"""(?:^|[\s{,])(?P<key>(?P<quote>["']?)uses(?P=quote))\s*:\s*"""
    r"""(?:"(?P<dq>[^"]*)"|'(?P<sq>[^']*)'|(?P<bare>[^\s#,}]+))"""
)
QUOTED = re.compile(r""""(?:\\.|[^"\\])*"|'(?:''|[^'])*'""")
SCALAR_START = " \t[{,:"
BLOCK_SCALAR = re.compile(r"(?:^|:\s|-\s)\s*[|>][1-9+-]{0,2}$")
NODE_START = re.compile(r"^(?:\s*-(?=\s))*\s*")
PINNED = re.compile(r"^[\w.-]+/[\w./-]+@[0-9a-f]{40}$")
TAG_COMMENT = re.compile(r"^#\s*v?\d[\w.-]*$")


def local_target(root: Path, ref: str) -> Path:
    """Return the file a local ``uses: ./...`` runs: a workflow file, or an action's action.yml."""
    path = root / ref
    if path.is_file():
        return path
    for name in ("action.yml", "action.yaml"):
        if (path / name).is_file():
            return path / name
    raise FileNotFoundError(f"{ref}: no workflow file or action.yml")


def problems(root: Path, files: list[str]) -> list[str]:
    """Return one message per unpinned ``uses:`` in ``files`` and the local files they call."""
    found: list[str] = []
    queue = [root / f for f in files]
    seen: set[Path] = set()
    while queue:
        path = queue.pop(0)
        if path in seen:
            continue
        seen.add(path)
        rel = path.relative_to(root).as_posix()
        for number, ref, comment in references(path.read_text(encoding="utf-8")):
            problem = check_reference(root, ref, comment, queue)
            if problem:
                found.append(f"{rel}:{number}: {problem}")
    return found


def references(text: str) -> list[tuple[int, str, str]]:
    """Return each ``uses`` value in ``text``, unquoted, with its line number and trailing comment.

    Block (``- uses: x``), flow (``- {uses: x}``) and quoted-key (``"uses": x``) forms are read.
    Only a mapping key counts: ``uses:`` in a comment, a quoted scalar or a block scalar
    (``run: |``) is text.
    """
    found = []
    scalar_indent: int | None = None
    for number, line in enumerate(text.splitlines(), start=1):
        if scalar_indent is not None:
            if not line.strip() or len(line) - len(line.lstrip()) > scalar_indent:
                continue
            scalar_indent = None
        code, comment, quoted = split_comment(line)
        found.extend((number, ref, comment) for ref in mapping_uses(code, quoted))
        if BLOCK_SCALAR.search(code.rstrip()):
            scalar_indent = block_parent_indent(line, code)
    return found


def split_comment(line: str) -> tuple[str, str, list[tuple[int, int]]]:
    """Return ``line``'s code, its trailing comment (``""`` for none) and the spans of its quoted scalars."""
    quoted: list[tuple[int, int]] = []
    pos = 0
    while pos < len(line):
        before = line[pos - 1] if pos else " "
        if line[pos] == "#" and before.isspace():
            return line[:pos], line[pos:].strip(), quoted
        match = QUOTED.match(line, pos) if line[pos] in "\"'" and before in SCALAR_START else None
        if match:
            quoted.append(match.span())
            pos = match.end()
        else:
            pos += 1
    return line, "", quoted


def mapping_uses(code: str, quoted: list[tuple[int, int]]) -> list[str]:
    """Return the value of each ``uses`` key in ``code`` that is not inside a quoted scalar."""
    refs = []
    for match in USES.finditer(code):
        start, end = match.span("key")
        if any(lo < start < hi or (lo == start and hi != end) for lo, hi in quoted):
            continue
        refs.append(next(value for value in (match["dq"], match["sq"], match["bare"]) if value is not None))
    return refs


def block_parent_indent(line: str, code: str) -> int:
    """Return the indent a line must exceed to belong to the block scalar ``line`` opens.

    That is the column of the key that holds it (``- run: |``), or of the dash for a sequence entry
    that is itself the scalar (``- |``).
    """
    node = NODE_START.match(code)
    if code[node.end() :].startswith(("|", ">")):
        return len(line) - len(line.lstrip())
    return node.end()


def check_reference(root: Path, ref: str, comment: str, queue: list[Path]) -> str:
    """Return the problem with one ``uses`` reference, or ``""``; queue a local target to check."""
    if ref.startswith("./"):
        queue.append(local_target(root, ref))
        return ""
    if not PINNED.match(ref):
        return f"{ref} is not pinned to a full commit SHA"
    if not TAG_COMMENT.match(comment):
        return f"{ref} needs the tag it pins in a comment (# v1.2.3)"
    return ""


def main(argv: list[str]) -> int:
    """Print every unpinned ``uses:`` as a GitHub error annotation; return 1 when there is one."""
    found = problems(ROOT, argv or PUBLISH_WORKFLOWS)
    for message in found:
        print(f"::error::{message}")
    return 1 if found else 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
