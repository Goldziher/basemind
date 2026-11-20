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
USES = re.compile(r"^\s*(?:-\s+)?uses:\s*(?P<ref>[^\s#]+)\s*(?P<comment>#.*)?$")
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
        for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
            match = USES.match(line)
            if match is None:
                continue
            ref, comment = match["ref"], (match["comment"] or "").strip()
            if ref.startswith("./"):
                queue.append(local_target(root, ref))
            elif not PINNED.match(ref):
                found.append(f"{rel}:{number}: {ref} is not pinned to a full commit SHA")
            elif not TAG_COMMENT.match(comment):
                found.append(f"{rel}:{number}: {ref} needs the tag it pins in a comment (# v1.2.3)")
    return found


def main(argv: list[str]) -> int:
    """Print every unpinned ``uses:`` as a GitHub error annotation; return 1 when there is one."""
    found = problems(ROOT, argv or PUBLISH_WORKFLOWS)
    for message in found:
        print(f"::error::{message}")
    return 1 if found else 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
