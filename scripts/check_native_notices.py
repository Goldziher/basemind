"""Fail when the hand-written notices for native code in the release binary are stale or missing.

Usage:
    python3 scripts/check_native_notices.py

cargo-about writes ``THIRD_PARTY_LICENSES`` from crate metadata, so it never sees the native code
some crates link into the binary: ONNX Runtime through ``ort-sys`` and Tesseract with Leptonica
through ``xberg-tesseract``. ``ATTRIBUTIONS.md`` covers that code by hand, and each such entry names
the crate that brings it in at the version it was checked against, as "`<crate>` <version>". This
fails when:

1. a crate in ``PINNED`` is locked at a version ``ATTRIBUTIONS.md`` does not name, so a bump that can
   change the native code or its licence gets the entry re-checked;
2. ``scripts/package-release.sh`` and ``scripts/smoke_release_archive.py`` disagree on the linked
   notices every archive ships;
3. a linked notice is missing from ``licenses/`` or not named in ``ATTRIBUTIONS.md``.
"""

from __future__ import annotations

import importlib.util
import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
# Crates whose ATTRIBUTIONS.md entry describes code or data cargo-about does not see.
PINNED = ("ort-sys", "xberg-tesseract", "tree-sitter-language-pack")
SHELL_LIST = re.compile(r"^LINKED_NOTICES=\(\n(?P<body>.*?)\n\)$", re.MULTILINE | re.DOTALL)


def locked_versions(lock: str, crate: str) -> list[str]:
    """Every version of ``crate`` in the Cargo.lock text ``lock``, in lock order."""
    return [package["version"] for package in tomllib.loads(lock)["package"] if package["name"] == crate]


def shell_notices(script: str) -> tuple[str, ...]:
    """The ``LINKED_NOTICES`` array of the package-release.sh text ``script``."""
    match = SHELL_LIST.search(script)
    if match is None:
        raise ValueError("package-release.sh defines no LINKED_NOTICES array")
    return tuple(match["body"].split())


def smoke_notices(root: Path) -> tuple[str, ...]:
    """``LINKED_NOTICES`` of the archive smoke test under ``root``."""
    path = root / "scripts" / "smoke_release_archive.py"
    spec = importlib.util.spec_from_file_location("smoke_release_archive", path)
    if spec is None or spec.loader is None:
        raise ValueError(f"cannot load {path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return tuple(module.LINKED_NOTICES)


def problems(root: Path) -> list[str]:
    """Each way the native notices under ``root`` are stale or missing, in check order."""
    attributions = (root / "ATTRIBUTIONS.md").read_text(encoding="utf-8")
    lock = (root / "Cargo.lock").read_text(encoding="utf-8")
    found = []
    for crate in PINNED:
        versions = locked_versions(lock, crate)
        if not versions:
            found.append(f"{crate} is not in Cargo.lock; drop it from PINNED and ATTRIBUTIONS.md")
        found.extend(
            f"Cargo.lock has {crate} {version}, ATTRIBUTIONS.md does not name `{crate}` {version}"
            for version in versions
            if f"`{crate}` {version}" not in attributions
        )
    shipped = shell_notices((root / "scripts" / "package-release.sh").read_text(encoding="utf-8"))
    checked = smoke_notices(root)
    if shipped != checked:
        found.append(f"package-release.sh ships {list(shipped)}, the archive smoke test expects {list(checked)}")
    for notice in shipped:
        if not (root / notice).is_file():
            found.append(f"{notice} is missing")
        if notice not in attributions:
            found.append(f"ATTRIBUTIONS.md does not name {notice}")
    return found


def main() -> int:
    """Print every problem with the native notices and fail when there is one."""
    found = problems(ROOT)
    if not found:
        print("✓ Native notices match Cargo.lock and the release packaging")
        return 0
    print("error: the native-code notices are stale or missing:", file=sys.stderr)
    for problem in found:
        print(f"  {problem}", file=sys.stderr)
    print(
        "Re-check each entry against the upstream source, then update ATTRIBUTIONS.md and licenses/.", file=sys.stderr
    )
    return 1


if __name__ == "__main__":
    sys.exit(main())
