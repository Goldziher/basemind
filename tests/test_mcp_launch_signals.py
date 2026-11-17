"""A launcher stopped by a signal mid-download releases its install lock.

A hook's time limit or a closing host signals the launcher's process group while `curl` runs. A
lock left behind makes the next start wait 120 s before it breaks it.
"""

import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LAUNCHER = ROOT / "plugin/.ai-rulez/scripts/mcp-launch.sh"
VERSION = "9.9.9"

# Stands in for curl: records that it started, then hangs as a stalled download does.
HANGING_CURL = """#!/bin/sh
: >"$FAKE_CURL_STARTED"
exec sleep 60
"""


@unittest.skipIf(sys.platform == "win32", "the launcher runs under bash")
class SignalTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        plugin = self.root / "plugin"
        (plugin / ".claude-plugin").mkdir(parents=True)
        (plugin / ".claude-plugin/plugin.json").write_text(json.dumps({"version": VERSION}))
        (plugin / "scripts").mkdir()
        self.launcher = plugin / "scripts/mcp-launch.sh"
        shutil.copyfile(LAUNCHER, self.launcher)
        self.launcher.chmod(0o755)
        fake_bin = self.root / "bin"
        fake_bin.mkdir()
        curl = fake_bin / "curl"
        curl.write_text(HANGING_CURL)
        curl.chmod(0o755)
        self.started = self.root / "curl-started"
        self.env = {
            "PATH": f"{fake_bin}:/usr/bin:/bin",
            "HOME": str(self.root / "home"),
            "XDG_CACHE_HOME": str(self.root / "cache"),
            "FAKE_CURL_STARTED": str(self.started),
        }
        self.lock = self.root / "cache/basemind/versions" / f".lock-{VERSION}"

    def stop_mid_download(self, sig: signal.Signals) -> int:
        process = subprocess.Popen(
            ["bash", str(self.launcher), "serve"],
            env=self.env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        self.addCleanup(lambda: process.poll() is None and os.killpg(process.pid, signal.SIGKILL))
        deadline = time.monotonic() + 20
        while not self.started.exists():
            self.assertLess(time.monotonic(), deadline, "the launcher never started its download")
            self.assertIsNone(process.poll(), "the launcher exited before downloading")
            time.sleep(0.05)
        self.assertTrue(self.lock.is_dir(), "the download runs under the install lock")
        os.killpg(process.pid, sig)
        return process.wait(timeout=20)

    def test_every_stopping_signal_releases_the_install_lock(self) -> None:
        for sig, code in ((signal.SIGTERM, 143), (signal.SIGINT, 130), (signal.SIGHUP, 129)):
            with self.subTest(signal=sig.name):
                self.started.unlink(missing_ok=True)
                self.assertEqual(self.stop_mid_download(sig), code)
                self.assertFalse(self.lock.exists(), f"{sig.name} left {self.lock}")


if __name__ == "__main__":
    unittest.main()
