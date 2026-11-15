"""The launcher never stops another cached version's processes, and prunes only long-unused versions."""

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
DAY = 24 * 3600
MINUTE = 60
# Past the launcher's default of 14 unused days.
LONG_AGO = 30 * DAY


def alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    return True


def age(path: Path, seconds: int) -> None:
    """Set `path`'s modification time to `seconds` ago."""
    then = time.time() - seconds
    os.utime(path, (then, then), follow_symlinks=False)


def wait_until_started_from(pid: int, binary: Path) -> None:
    """Block until `pid` has exec'd `binary`: a forked shell lists its own arguments until then."""
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        out = subprocess.run(["ps", "-ww", "-o", "args=", "-p", str(pid)], capture_output=True, text=True, check=False)
        if out.stdout.strip().startswith(str(binary)):
            return
        time.sleep(0.02)
    raise AssertionError(f"process {pid} never showed {binary} as its command: {out.stdout!r}")


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
        path = self.versions / version
        path.mkdir(parents=True, exist_ok=True)
        return path

    @property
    def versions(self) -> Path:
        return self.cache / "basemind/versions"

    def other_version_binary(self, version: str, *, last_used_secs_ago: int | None = LONG_AGO) -> Path:
        """A cached `basemind` of another version, last launched `last_used_secs_ago` ago.

        `None` leaves the directory without a marker, as a launch that never completed would.
        """
        directory = self.version_dir(version)
        binary = directory / "basemind"
        # A symlink, not a copy: macOS kills a copied system binary at exec.
        binary.symlink_to(shutil.which("sleep"))
        if last_used_secs_ago is not None:
            (directory / ".last-used").write_text("")
            age(directory / ".last-used", last_used_secs_ago)
        return binary

    def session(self, binary: Path) -> subprocess.Popen:
        """A `serve`: a child of its host, which this test stands in for."""
        process = subprocess.Popen([binary, "300"])
        self.addCleanup(process.wait)
        self.addCleanup(process.kill)
        wait_until_started_from(process.pid, binary)
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
        wait_until_started_from(pid, binary)
        return pid

    def kill_all(self) -> None:
        for pid in self.pids:
            with contextlib.suppress(ProcessLookupError):
                os.kill(pid, signal.SIGKILL)

    def launch(
        self,
        *,
        cache: Path | None = None,
        path_prefix: Path | None = None,
        prune_after_days: str | None = None,
    ) -> None:
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
        env.pop("BASEMIND_PRUNE_AFTER_DAYS", None)
        if prune_after_days is not None:
            env["BASEMIND_PRUNE_AFTER_DAYS"] = prune_after_days
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

    def test_nothing_is_pruned_when_the_process_list_comes_back_empty(self) -> None:
        unused = self.other_version_binary(UNUSED).parent
        silent = self.root / "silent-ps"
        silent.mkdir()
        (silent / "ps").write_text("#!/bin/sh\nexit 0\n")
        (silent / "ps").chmod(0o755)

        self.launch(path_prefix=silent)

        self.assertTrue(unused.is_dir(), "an empty process list cannot show that a version is unused")

    def test_a_version_being_installed_is_not_pruned(self) -> None:
        locked = self.other_version_binary(UNUSED).parent
        (locked.parent / f".lock-{UNUSED}").mkdir()

        self.launch()

        self.assertTrue(locked.is_dir(), "a version whose install lock is held is left alone")

    def test_a_version_launched_within_the_threshold_is_kept_though_nothing_runs_from_it(self) -> None:
        """Another launcher is a shell until it execs: only its marker shows what it is about to run."""
        about_to_start = self.other_version_binary(IN_USE, last_used_secs_ago=0).parent
        last_week = self.other_version_binary(UNUSED, last_used_secs_ago=7 * DAY).parent

        self.launch()

        self.assertTrue(about_to_start.is_dir(), "a version another launcher just marked must survive")
        self.assertTrue(last_week.is_dir(), "the default threshold is 14 days")

    def test_the_threshold_is_configurable_in_days(self) -> None:
        last_week = self.other_version_binary(UNUSED, last_used_secs_ago=7 * DAY).parent
        yesterday = self.other_version_binary(IN_USE, last_used_secs_ago=DAY).parent

        self.launch(prune_after_days="3")

        self.assertFalse(last_week.exists(), "unused for longer than the configured 3 days")
        self.assertTrue(yesterday.is_dir())

    def test_a_version_without_a_marker_is_judged_by_its_own_age(self) -> None:
        fresh = self.other_version_binary(IN_USE, last_used_secs_ago=None).parent
        old = self.other_version_binary(UNUSED, last_used_secs_ago=None).parent
        age(old, LONG_AGO)

        self.launch()

        self.assertTrue(fresh.is_dir(), "a directory created a moment ago may not have been launched yet")
        self.assertFalse(old.exists())

    def test_every_launch_marks_its_own_version_as_used(self) -> None:
        marker = self.version_dir(CURRENT) / ".last-used"
        marker.write_text("")
        age(marker, LONG_AGO)

        self.launch()

        self.assertLess(time.time() - marker.stat().st_mtime, MINUTE, "the launch refreshes the marker")

    def test_the_cache_of_launchers_up_to_0_33_is_never_touched(self) -> None:
        """Those launchers kill and delete whatever else sits in `bin/`; this one stays out of it."""
        legacy = self.cache / "basemind/bin/0.33.0"
        legacy.mkdir(parents=True)
        (legacy / "basemind").write_text("old")
        age(legacy, LONG_AGO)

        self.launch()

        self.assertTrue((legacy / "basemind").is_file(), "bin/ belongs to the older launchers")
        self.assertFalse((self.cache / "basemind/bin" / CURRENT).exists(), "nothing is installed under bin/")

    def test_another_installs_lock_and_staging_directory_survive_a_launch(self) -> None:
        """Launching one version must not pull the lock and staging area from under another's install."""
        installer = subprocess.Popen(["sleep", "300"])
        self.addCleanup(installer.wait)
        self.addCleanup(installer.kill)
        lock = self.versions / f".lock-{IN_USE}"
        staging = self.versions / f".staging-{IN_USE}-{installer.pid}"
        slow_staging = self.versions / f".staging-{UNUSED}-{installer.pid}"
        for directory in (lock, staging, slow_staging):
            directory.mkdir()
        (staging / "basemind").write_text("half extracted")
        age(slow_staging, 60 * MINUTE)

        self.launch()

        self.assertTrue(lock.is_dir(), "an install lock taken a moment ago is in use")
        self.assertTrue((staging / "basemind").is_file(), "a staging directory of a live install is in use")
        self.assertTrue(slow_staging.is_dir(), "a slow install is still an install while its process lives")

    def test_abandoned_install_state_is_removed(self) -> None:
        finished = subprocess.Popen(["true"])
        finished.wait()
        old_lock = self.versions / f".lock-{IN_USE}"
        old_staging = self.versions / f".staging-{IN_USE}-{finished.pid}"
        fresh_staging = self.versions / f".staging-{UNUSED}-{finished.pid}"
        for directory in (old_lock, old_staging, fresh_staging):
            directory.mkdir()
        age(old_lock, 60 * MINUTE)
        age(old_staging, 60 * MINUTE)

        self.launch()

        self.assertFalse(old_lock.exists(), "a lock untouched for an hour is abandoned")
        self.assertFalse(old_staging.exists(), "so is a staging directory whose process is gone")
        self.assertTrue(fresh_staging.is_dir(), "nothing younger than ten minutes is removed")


class CachePathWithSpacesTests(OtherVersionsTests):
    """The same contract when the cache directory's path contains spaces."""

    CACHE_NAME = "cache with spaces"


if __name__ == "__main__":
    unittest.main()
