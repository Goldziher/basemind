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
3. a linked notice is missing from ``licenses/`` or not named in ``ATTRIBUTIONS.md``;
4. a library ``scripts/build-libheif.sh`` pins (libheif, libde265) is not named at that version in
   ``ATTRIBUTIONS.md``, or its ``licenses/<library>.txt`` does not open with that version or does not
   name the source tarball's sha256 and the vcpkg commit Windows builds it from.
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
# A version token: it ends in a letter or digit, so a sentence's closing period is not part of it.
VERSION_TOKEN = r"(\d[0-9A-Za-z.+-]*[0-9A-Za-z])"
BUILT_PIN = re.compile(r"^(?P<name>[A-Z0-9]+)_(?P<key>VERSION|SHA256)=(?P<value>\S+)$", re.MULTILINE)
VCPKG_COMMIT = re.compile(r"BASEMIND_VCPKG_COMMIT=(?P<commit>[0-9a-f]{40})")


def locked_versions(lock: str, crate: str) -> list[str]:
    """Every version of ``crate`` in the Cargo.lock text ``lock``, in lock order."""
    return [package["version"] for package in tomllib.loads(lock)["package"] if package["name"] == crate]


def attributed_versions(attributions: str, crate: str) -> set[str]:
    """Every whole version the ATTRIBUTIONS.md text ``attributions`` names ``crate`` at, as
    "`<crate>` <version>"."""
    return set(re.findall(rf"`{re.escape(crate)}` {VERSION_TOKEN}", attributions))


def named_library_versions(attributions: str, library: str) -> set[str]:
    """Every whole version the ATTRIBUTIONS.md text ``attributions`` names ``library`` at, as
    "<library> <version>" (so "xberg-libheif 1.4.2" does not name libheif)."""
    return set(re.findall(rf"(?<![\w-]){re.escape(library)} {VERSION_TOKEN}", attributions))


def built_pins(script: str) -> dict[str, tuple[str, str]]:
    """Each library the build-libheif.sh text ``script`` pins, as ``{library: (version, sha256)}``."""
    pins: dict[str, dict[str, str]] = {}
    for match in BUILT_PIN.finditer(script):
        pins.setdefault(match["name"].lower(), {})[match["key"]] = match["value"]
    return {library: (pin["VERSION"], pin["SHA256"]) for library, pin in pins.items()}


def vcpkg_commit(action: str) -> str:
    """The vcpkg commit the install-libheif action text ``action`` builds Windows' libheif from."""
    match = VCPKG_COMMIT.search(action)
    if match is None:
        raise ValueError("install-libheif pins no BASEMIND_VCPKG_COMMIT")
    return match["commit"]


def notice_problems(library: str, version: str, sha256: str, commit: str, notice: str) -> list[str]:
    """Each way the ``licenses/<library>.txt`` text ``notice`` misstates the build of ``library``
    ``version`` from the tarball with ``sha256`` (Linux, macOS) and vcpkg ``commit`` (Windows)."""
    checks = (
        (notice.startswith(f"{library} {version}:"), f'does not open with "{library} {version}:"'),
        (sha256 in notice, f"does not name the {library} {version} tarball's sha256 {sha256}"),
        (commit in notice, f"does not name vcpkg commit {commit}"),
    )
    return [f"licenses/{library}.txt {problem}" for holds, problem in checks if not holds]


def built_problems(root: Path, attributions: str) -> list[str]:
    """Each way the notices of the libraries build-libheif.sh pins under ``root`` are stale."""
    pins = built_pins((root / "scripts" / "build-libheif.sh").read_text(encoding="utf-8"))
    commit = vcpkg_commit((root / ".github/actions/install-libheif/action.yml").read_text(encoding="utf-8"))
    found = []
    for library, (version, sha256) in pins.items():
        if version not in named_library_versions(attributions, library):
            found.append(f"build-libheif.sh builds {library} {version}, ATTRIBUTIONS.md does not name it so")
        notice_path = root / "licenses" / f"{library}.txt"
        notice = notice_path.read_text(encoding="utf-8") if notice_path.is_file() else ""
        found.extend(notice_problems(library, version, sha256, commit, notice))
    return found


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
        named = attributed_versions(attributions, crate)
        found.extend(
            f"Cargo.lock has {crate} {version}, ATTRIBUTIONS.md does not name `{crate}` {version}"
            for version in versions
            if version not in named
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
    found.extend(built_problems(root, attributions))
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
