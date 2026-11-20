"""Print one version's CHANGELOG section, the body of its GitHub release.

Usage:
    python3 scripts/changelog_section.py <version> [<changelog>]   # e.g. 0.34.0

The section is everything under ``## [<version>]`` up to the next ``##`` heading or the
version comparison links at the end of the file, without the heading itself. A version with no
section, or an empty one, is a failure: a release must not publish without its notes. Used by the
``create_release`` job of ``.github/workflows/publish.yaml``.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
VERSION_RE = re.compile(r"^\d+\.\d+\.\d+(-rc\.\d+)?$")
SECTION_END_RE = re.compile(r"^(## |\[\d+\.\d+\.\d+[^\]]*\]: )")


class SectionError(Exception):
    """The CHANGELOG has no usable section for the requested version."""


def section(changelog: str, version: str) -> str:
    """The body of ``version``'s section in ``changelog``, stripped of surrounding blank lines."""
    heading = re.compile(rf"^## \[{re.escape(version)}\]")
    lines = changelog.splitlines()
    start = next((i for i, line in enumerate(lines) if heading.match(line)), None)
    if start is None:
        raise SectionError(f"CHANGELOG.md has no section for {version}")
    body: list[str] = []
    for line in lines[start + 1 :]:
        if SECTION_END_RE.match(line):
            break
        body.append(line)
    text = "\n".join(body).strip("\n")
    if not text.strip():
        raise SectionError(f"CHANGELOG.md section for {version} is empty")
    return text + "\n"


def main(argv: list[str]) -> int:
    if len(argv) not in (1, 2) or not VERSION_RE.match(argv[0]):
        print("usage: changelog_section.py <MAJOR.MINOR.PATCH[-rc.N]> [<changelog>]", file=sys.stderr)
        return 2
    path = Path(argv[1]) if len(argv) == 2 else ROOT / "CHANGELOG.md"
    try:
        sys.stdout.write(section(path.read_text(encoding="utf-8"), argv[0]))
    except SectionError as error:
        print(f"::error::{error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
