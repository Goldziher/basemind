"""A launcher that finds the managed binary installed once it holds the install lock releases it.

`exec` replaces the shell without running the EXIT trap, so the lock must be dropped before the exec.
A lock left behind makes the next start wait 120 s before it breaks it.
"""

import json
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LAUNCHER = ROOT / "plugin/.ai-rulez/scripts/mcp-launch.sh"
VERSION = "9.9.9"

# Reports a stale version on its first probe and the pinned one afterwards, as when another launch
# finishes its install between this launch's first check and its acquiring the lock. A real run
# records whether the lock exists at the moment of the exec.
LATE_BINARY = """#!/bin/sh
if [ "$1" = "--version" ]; then
  if [ -e "$FAKE_PROBED" ]; then
    echo "basemind $FAKE_VERSION"
  else
    : >"$FAKE_PROBED"
    echo "basemind 0.0.0"
  fi
  exit 0
fi
if [ -d "$FAKE_LOCK" ]; then echo held >"$FAKE_RESULT"; else echo released >"$FAKE_RESULT"; fi
"""


@unittest.skipIf(sys.platform == "win32", "the launcher runs under bash")
class LockReleaseTests(unittest.TestCase):
    def test_exec_of_a_binary_installed_while_waiting_leaves_no_lock(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            plugin = root / "plugin"
            (plugin / ".claude-plugin").mkdir(parents=True)
            (plugin / ".claude-plugin/plugin.json").write_text(json.dumps({"version": VERSION}))
            (plugin / "scripts").mkdir()
            launcher = plugin / "scripts/mcp-launch.sh"
            shutil.copyfile(LAUNCHER, launcher)
            versions = root / "cache/basemind/versions"
            (versions / VERSION).mkdir(parents=True)
            binary = versions / VERSION / "basemind"
            binary.write_text(LATE_BINARY)
            binary.chmod(0o755)
            lock = versions / f".lock-{VERSION}"
            result = root / "result"
            env = {
                "PATH": "/usr/bin:/bin",
                "HOME": str(root / "home"),
                "XDG_CACHE_HOME": str(root / "cache"),
                "FAKE_PROBED": str(root / "probed"),
                "FAKE_VERSION": VERSION,
                "FAKE_LOCK": str(lock),
                "FAKE_RESULT": str(result),
            }
            completed = subprocess.run(
                ["bash", str(launcher), "serve"],
                env=env,
                capture_output=True,
                text=True,
                timeout=30,
                check=False,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertEqual(result.read_text().strip(), "released")
            self.assertFalse(lock.exists(), f"the exec left {lock}")


if __name__ == "__main__":
    unittest.main()
