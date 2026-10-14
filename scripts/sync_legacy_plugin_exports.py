"""Keep supported legacy package resources derived from the curated plugin."""

import argparse
import shutil
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TREES = (".codex-plugin", ".cursor-plugin", "opencode-plugin")


def reject_redirects(root: Path, destination: Path) -> None:
    """Reject redirected ancestors before any write or obsolete-file removal."""
    for path in (destination, *destination.parents):
        if path == root.parent:
            break
        if path.is_symlink():
            raise ValueError(f"Symlink export destination: {path}")


def sync_files(root: Path, files: dict[str, Path], *, check: bool) -> bool:
    """Compare every owned byte; leave all unowned files untouched."""
    fresh = True
    for name, source in files.items():
        destination = root / name
        reject_redirects(root, destination)
        if source.is_symlink():
            raise ValueError(f"Symlink export source: {source}")
        if not destination.is_file() or destination.read_bytes() != source.read_bytes():
            if check:
                print(f"Stale compatibility export: {name}")
                fresh = False
            else:
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(source, destination)
    return fresh


def expected_files(root: Path) -> dict[str, Path]:
    """Declare exports from the canonical native public resources."""
    skills = sorted((root / "skills").glob("*/SKILL.md"))
    commands = sorted((root / "plugin/commands").glob("*.md"))
    if len(skills) != 10 or len(commands) != 6:
        raise ValueError(f"Expected 10 public skills and 6 commands, found {len(skills)}/{len(commands)}")
    files = {f"commands/{path.name}": path for path in commands}
    for tree in TREES:
        for path in skills:
            files[f"{tree}/skills/{path.parent.name}/SKILL.md"] = path
        for path in commands:
            files[f"{tree}/commands/{path.name}"] = path
    for path in (root / "plugin/.ai-rulez/hooks").iterdir():
        if path.is_file():
            files[f"hooks/{path.name}"] = path
    files["scripts/mcp-launch.sh"] = root / "plugin/.ai-rulez/scripts/mcp-launch.sh"
    files["opencode-plugin/.pi/extensions/basemind.ts"] = root / ".pi/extensions/basemind.ts"
    for tree in TREES[:2]:
        for name in ("session-start", "inbox-notify", "run-hook.cmd"):
            files[f"{tree}/hooks/{name}"] = root / "plugin/.ai-rulez/hooks" / name
    return files


def main() -> int:
    """Check or refresh owned compatibility resources and a retired skill identity."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    fresh = sync_files(ROOT, expected_files(ROOT), check=args.check)
    for tree in TREES:
        old = ROOT / tree / "skills/multi-agent-room/SKILL.md"
        reject_redirects(ROOT, old)
        if old.exists():
            if args.check:
                print(f"Retired skill identity: {old.relative_to(ROOT)}")
                fresh = False
            else:
                old.unlink()
                if not any(old.parent.iterdir()):
                    old.parent.rmdir()
    print(f"Checked {len(expected_files(ROOT))} compatibility resources")
    return 0 if fresh else 1


if __name__ == "__main__":
    raise SystemExit(main())
