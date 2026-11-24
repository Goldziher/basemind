"""Print one version's CHANGELOG section, the body of its GitHub release.

Usage:
    python3 scripts/changelog_section.py <version> [<changelog>]   # e.g. 0.34.0

A hand-written summary at ``docs/release-notes/<version>.md`` (beside the changelog's directory)
replaces the section as the release body, so a large release keeps its long list in CHANGELOG.md
only. Without one, a section over ``RELEASE_NOTES_CEILING`` characters (default 60000, well under
GitHub's 125000-character body limit) is a failure that asks the author to write that summary.

The section is everything under ``## [<version>]`` up to the next ``##`` heading or the
version comparison links at the end of the file, without the heading itself. A version with no
section, or an empty one, is a failure: a release must not publish without its notes. Used by the
``create_release`` job of ``.github/workflows/publish.yaml``.
"""

from __future__ import annotations

import os
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
VERSION_RE = re.compile(r"^\d+\.\d+\.\d+(-rc\.\d+)?$")
DEFAULT_CEILING = 60_000
CEILING_ENV = "RELEASE_NOTES_CEILING"
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


def release_notes(changelog_path: Path, version: str, ceiling: int) -> str:
    """The curated summary for ``version`` if one exists, else its CHANGELOG section within ``ceiling``."""
    curated = changelog_path.parent / "docs" / "release-notes" / f"{version}.md"
    if curated.is_file():
        text = curated.read_text(encoding="utf-8").strip("\n")
        if not text.strip():
            raise SectionError(f"{curated.name} is empty")
        return text + "\n"
    text = section(changelog_path.read_text(encoding="utf-8"), version)
    if len(text) > ceiling:
        raise SectionError(
            f"CHANGELOG.md section for {version} is {len(text)} characters, over the {ceiling} ceiling: "
            f"curate the release summary by hand in docs/release-notes/{version}.md "
            "and keep the long list in CHANGELOG.md"
        )
    return text


def ceiling_from_env() -> int:
    """The ceiling from ``RELEASE_NOTES_CEILING``, or the default when unset."""
    raw = os.environ.get(CEILING_ENV)
    if raw is None:
        return DEFAULT_CEILING
    if not raw.isdigit() or int(raw) == 0:
        raise SectionError(f"{CEILING_ENV} must be a positive integer, got {raw!r}")
    return int(raw)


def main(argv: list[str]) -> int:
    if len(argv) not in (1, 2) or not VERSION_RE.match(argv[0]):
        print("usage: changelog_section.py <MAJOR.MINOR.PATCH[-rc.N]> [<changelog>]", file=sys.stderr)
        return 2
    path = Path(argv[1]) if len(argv) == 2 else ROOT / "CHANGELOG.md"
    try:
        sys.stdout.write(release_notes(path, argv[0], ceiling_from_env()))
    except SectionError as error:
        print(f"::error::{error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
