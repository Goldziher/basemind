"""Verify that every shipped version surface carries one release version.

Usage:
    python3 scripts/check_release_versions.py <version>   # e.g. 0.34.0 or 0.34.0-rc.1

Every surface is required: a missing file or an unreadable version is a failure, never a skip, so a
renamed manifest cannot silently drop out of the check. Python packages use the PEP 440 form
(``0.34.0rc1``); everything else uses the Cargo form. Used by ``scripts/release-bump.sh`` after it
rewrites the tree and by the ``meta`` job of ``.github/workflows/publish.yaml`` before anything
publishes.
"""

from __future__ import annotations

import json
import re
import sys
from collections.abc import Callable
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
VERSION_RE = re.compile(r"^\d+\.\d+\.\d+(-rc\.\d+)?$")


def _json_version(path: Path) -> str | None:
    value = json.loads(path.read_text(encoding="utf-8")).get("version")
    return value if isinstance(value, str) else None


def _line_version(pattern: str) -> Callable[[Path], str | None]:
    compiled = re.compile(pattern, re.MULTILINE)

    def read(path: Path) -> str | None:
        match = compiled.search(path.read_text(encoding="utf-8"))
        return match.group(1) if match else None

    return read


def _plugin_table_version(path: Path) -> str | None:
    match = re.search(r'\[plugin\][^\[]*?\nversion = "([^"]*)"', path.read_text(encoding="utf-8"), re.DOTALL)
    return match.group(1) if match else None


TOML_VERSION = _line_version(r"""^version = ["']([^"']+)["']""")
PY_VERSION = _line_version(r"""^__version__ = ["']([^"']+)["']""")
YAML_VERSION = _line_version(r"""^version: ["']?([^"'\s]+)["']?""")

# (path, reader, uses the PEP 440 form)
SURFACES: list[tuple[str, Callable[[Path], str | None], bool]] = [
    ("Cargo.toml", TOML_VERSION, False),
    ("package.json", _json_version, False),
    ("npm-package/package.json", _json_version, False),
    ("opencode-plugin/package.json", _json_version, False),
    ("pip-package/pyproject.toml", TOML_VERSION, True),
    ("pip-package/basemind/__init__.py", PY_VERSION, True),
    (".claude-plugin/plugin.json", _json_version, False),
    (".codex-plugin/plugin.json", _json_version, False),
    (".cursor-plugin/plugin.json", _json_version, False),
    ("gemini-extension.json", _json_version, False),
    ("kimi.plugin.json", _json_version, False),
    ("plugin/.ai-rulez/config.plugins.toml", _plugin_table_version, False),
    ("plugin/package.json", _json_version, False),
    ("plugin/plugin.json", _json_version, False),
    ("plugin/gemini-extension.json", _json_version, False),
    ("plugin/kimi.plugin.json", _json_version, False),
    ("plugin/.claude-plugin/plugin.json", _json_version, False),
    ("plugin/.codex-plugin/plugin.json", _json_version, False),
    ("plugin/.cursor-plugin/plugin.json", _json_version, False),
    ("plugin/.factory-plugin/plugin.json", _json_version, False),
    ("plugin/.hermes/package/pyproject.toml", TOML_VERSION, True),
    ("plugin/.hermes/package/src/basemind_hermes_plugin/__init__.py", PY_VERSION, False),
    ("plugin/.hermes/package/src/basemind_hermes_plugin/plugin.yaml", YAML_VERSION, False),
    ("plugin/.hermes/plugins/basemind/__init__.py", PY_VERSION, False),
    ("plugin/.hermes/plugins/basemind/plugin.yaml", YAML_VERSION, False),
]

# Marketplace indexes carry a version only when ai-rulez emits one; when present it must match.
OPTIONAL_JSON = [".claude-plugin/marketplace.json", ".agents/plugins/marketplace.json"]


def check(root: Path, version: str) -> list[str]:
    """Every disagreement with ``version`` under ``root``, as printable lines."""
    python_version = version.replace("-rc.", "rc")
    problems: list[str] = []
    for rel, read, pep440 in SURFACES:
        path = root / rel
        want = python_version if pep440 else version
        if not path.is_file():
            problems.append(f"{rel}: missing (expected version {want})")
            continue
        got = read(path)
        if got != want:
            problems.append(f"{rel}: expected {want}, got {got}")
    for rel in OPTIONAL_JSON:
        path = root / rel
        if not path.is_file():
            continue
        for plugin in json.loads(path.read_text(encoding="utf-8")).get("plugins", []):
            got = plugin.get("version")
            if got is not None and got != version:
                problems.append(f"{rel}: plugin {plugin.get('name')} expected {version}, got {got}")
    return problems


def main(argv: list[str]) -> int:
    if len(argv) != 1 or not VERSION_RE.match(argv[0]):
        print("usage: check_release_versions.py <MAJOR.MINOR.PATCH[-rc.N]>", file=sys.stderr)
        return 2
    problems = check(ROOT, argv[0])
    for line in problems:
        print(f"::error::{line}")
    if problems:
        print(f"{len(problems)} of {len(SURFACES)} version surfaces disagree with {argv[0]}")
        return 1
    print(f"all {len(SURFACES)} version surfaces carry {argv[0]}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
