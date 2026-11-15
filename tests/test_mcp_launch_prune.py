"""The launcher never stops another cached version's processes, and prunes only unused versions."""

import contextlib
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
CURRENT = "9.9.9"
IN_USE = "9.9.8"
UNUSED = "9.9.7"


def alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    return True


@unittest.skipIf(sys.platform == "win32", "the process scan is Unix-only")
class OtherVersionsTests(unittest.TestCase):
    CACHE_NAME = "cache"

    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.cache = self.root / self.CACHE_NAME
        self.plugin = self.root / "plugin"
        (self.plugin / ".claude-plugin").mkdir(parents=True)
        (self.plugin / ".claude-plugin/plugin.json").write_text(json.dumps({"version": CURRENT}))
        (self.plugin / "scripts").mkdir()
        shutil.copyfile(LAUNCHER, self.plugin / "scripts/mcp-launch.sh")
        current = self.version_dir(CURRENT) / "basemind"
        current.write_text(f'#!/bin/sh\n[ "$1" = --version ] && echo "basemind {CURRENT}"\nexit 0\n')
        current.chmod(0o755)
        self.pids: list[int] = []
        self.addCleanup(self.kill_all)

    def version_dir(self, version: str) -> Path:
        path = self.cache / "basemind/bin" / version
        path.mkdir(parents=True, exist_ok=True)
        return path

    def other_version_binary(self, version: str, *, installed_secs_ago: int = 3600) -> Path:
        """A long-running process whose argv[0] is a cached `basemind` of another version."""
        directory = self.version_dir(version)
        binary = directory / "basemind"
        # A symlink, not a copy: macOS kills a copied system binary at exec.
        binary.symlink_to(shutil.which("sleep"))
        installed = time.time() - installed_secs_ago
        os.utime(directory, (installed, installed))
        return binary

    def session(self, binary: Path) -> subprocess.Popen:
        """A `serve`: a child of its host, which this test stands in for."""
        process = subprocess.Popen([binary, "300"])
        self.addCleanup(process.wait)
        self.addCleanup(process.kill)
        return process

    def daemon(self, binary: Path) -> int:
        """A daemon: started by a shell that exits at once, so it has no parent to speak for it."""
        out = subprocess.run(
            ["sh", "-c", '"$1" 300 >/dev/null 2>&1 & echo $!', "sh", str(binary)],
            capture_output=True,
            text=True,
            check=True,
        )
        pid = int(out.stdout.strip())
        self.pids.append(pid)
        return pid

    def kill_all(self) -> None:
        for pid in self.pids:
            with contextlib.suppress(ProcessLookupError):
                os.kill(pid, signal.SIGKILL)

    def launch(self, *, cache: Path | None = None, path_prefix: Path | None = None) -> None:
        env = {
            **os.environ,
            "HOME": str(self.root),
            "XDG_CACHE_HOME": str(cache or self.cache),
            "CLAUDE_PLUGIN_ROOT": str(self.plugin),
        }
        if path_prefix is not None:
            env["PATH"] = f"{path_prefix}{os.pathsep}{env['PATH']}"
        env.pop("BASEMIND_BIN", None)
        env.pop("BASEMIND_FORCE_VERSION", None)
        subprocess.run(
            ["/bin/bash", self.plugin / "scripts/mcp-launch.sh", "serve"],
            env=env,
            capture_output=True,
            text=True,
            timeout=30,
            check=True,
        )

    def test_another_versions_session_and_daemon_keep_running_and_keep_their_binary(self) -> None:
        binary = self.other_version_binary(IN_USE)
        session = self.session(binary)
        daemon = self.daemon(binary)

        self.launch()

        self.assertIsNone(session.poll(), "another version's session must not be stopped")
        self.assertTrue(alive(daemon), "another version's daemon must not be stopped")
        self.assertTrue(binary.is_symlink(), "a version with running processes keeps its binary")

    def test_a_daemon_with_no_session_left_is_not_stopped_either(self) -> None:
        binary = self.other_version_binary(IN_USE)
        daemon = self.daemon(binary)

        self.launch()

        self.assertTrue(alive(daemon), "the launcher stops no process; an idle daemon exits by itself")
        self.assertTrue(binary.is_symlink())

    def test_a_version_nothing_runs_from_is_pruned(self) -> None:
        unused = self.other_version_binary(UNUSED).parent

        self.launch()

        self.assertFalse(unused.exists(), "an unused version directory is removed")
        self.assertTrue(self.version_dir(CURRENT).is_dir())

    def test_a_version_started_through_the_resolved_cache_path_is_kept(self) -> None:
        link = self.root / "linked-cache"
        link.symlink_to(self.cache, target_is_directory=True)
        binary = self.other_version_binary(IN_USE)
        self.daemon(binary.parent.resolve() / binary.name)

        self.launch(cache=link)

        self.assertTrue(binary.is_symlink(), "a process named by the physical path still counts as in use")

    def test_nothing_is_pruned_when_the_process_list_cannot_be_read(self) -> None:
        unused = self.other_version_binary(UNUSED).parent
        broken = self.root / "broken-ps"
        broken.mkdir()
        (broken / "ps").write_text("#!/bin/sh\nexit 1\n")
        (broken / "ps").chmod(0o755)

        self.launch(path_prefix=broken)

        self.assertTrue(unused.is_dir(), "without a process list every version counts as in use")

    def test_a_version_being_installed_is_not_pruned(self) -> None:
        locked = self.other_version_binary(UNUSED).parent
        (locked.parent / f".lock-{UNUSED}").mkdir()
        fresh = self.other_version_binary(IN_USE, installed_secs_ago=0).parent

        self.launch()

        self.assertTrue(locked.is_dir(), "a version whose install lock is held is left alone")
        self.assertTrue(fresh.is_dir(), "a version installed a moment ago may not have started yet")


class CachePathWithSpacesTests(OtherVersionsTests):
    """The same contract when the cache directory's path contains spaces."""

    CACHE_NAME = "cache with spaces"


if __name__ == "__main__":
    unittest.main()
