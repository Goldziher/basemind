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
# What a fake `basemind` is run with: keep running, as a `serve` or a daemon would.
RUN_FOR_A_WHILE = ["-c", "import time; time.sleep(300)"]


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


def fake_basemind(binary: Path) -> None:
    """Make `binary` a long-running stand-in for a `basemind`, started as `binary RUN_FOR_A_WHILE`.

    A symlink to this test's Python, which ignores the name it runs under. Not `sleep`: the uutils
    coreutils of Ubuntu 26.04 refuse to run under a name other than their own, and macOS kills a
    copied system binary at exec. A macOS framework build's `python3` re-executes the interpreter
    inside its `Python.app`, which would show that path as the command, so that one is linked.
    """
    framework_app = Path(sys.base_prefix) / "Resources/Python.app/Contents/MacOS/Python"
    binary.symlink_to(framework_app if framework_app.is_file() else os.path.realpath(sys.executable))


def windows_path(path: Path) -> str:
    """`path` as `ps -W` prints a Windows executable: on a drive, backslashes, in a case of its own."""
    return ("C:" + str(path).replace("/", "\\")).upper()


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
        fake_basemind(binary)
        if last_used_secs_ago is not None:
            (directory / ".last-used").write_text("")
            age(directory / ".last-used", last_used_secs_ago)
        return binary

    def session(self, binary: Path) -> subprocess.Popen:
        """A `serve`: a child of its host, which this test stands in for."""
        process = subprocess.Popen([binary, *RUN_FOR_A_WHILE])
        self.addCleanup(process.wait)
        self.addCleanup(process.kill)
        wait_until_started_from(process.pid, binary)
        return process

    def daemon(self, binary: Path) -> int:
        """A daemon: started by a shell that exits at once, so it has no parent to speak for it."""
        out = subprocess.run(
            ["sh", "-c", '"$@" >/dev/null 2>&1 & echo $!', "sh", str(binary), *RUN_FOR_A_WHILE],
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
        args: tuple[str, ...] = ("serve",),
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
            ["/bin/bash", self.plugin / "scripts/mcp-launch.sh", *args],
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

    def test_an_agent_command_from_a_hook_prunes_nothing(self) -> None:
        unused = self.other_version_binary(UNUSED).parent

        self.launch(args=("agent", "hook", "name"))

        self.assertTrue(unused.exists(), "the hooks' agent commands leave pruning to other launches")

    def test_a_symlinked_version_directory_is_left_alone_and_its_target_survives(self) -> None:
        build = self.root / "a-build-directory"
        build.mkdir()
        (build / "basemind").write_text("built by hand\n")
        (build / ".last-used").write_text("")
        for path in (build / ".last-used", build / "basemind", build):
            age(path, LONG_AGO)
        self.versions.mkdir(parents=True, exist_ok=True)
        link = self.versions / UNUSED
        link.symlink_to(build, target_is_directory=True)
        age(link, LONG_AGO)

        self.launch()

        self.assertTrue((build / "basemind").is_file(), "the launcher never deletes through a symlink")
        self.assertTrue(link.is_symlink(), "a version directory it did not create is not its to remove")

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

    def legacy_binary(self, version: str, *, installed_secs_ago: int) -> Path:
        """A version a launcher up to 0.33 installed under `bin/`, which keeps no marker."""
        directory = self.cache / "basemind/bin" / version
        directory.mkdir(parents=True)
        binary = directory / "basemind"
        fake_basemind(binary)
        age(directory, installed_secs_ago)
        return binary

    def test_nothing_is_installed_into_the_cache_of_launchers_up_to_0_33(self) -> None:
        """Those launchers kill and delete whatever else sits in `bin/`; this one puts nothing there."""
        recent = self.legacy_binary("0.33.0", installed_secs_ago=7 * DAY)

        self.launch()

        self.assertTrue(recent.is_symlink(), "a version installed within the threshold may be in use")
        self.assertFalse((self.cache / "basemind/bin" / CURRENT).exists(), "nothing is installed under bin/")

    def test_an_abandoned_version_of_a_launcher_up_to_0_33_is_removed(self) -> None:
        abandoned = self.legacy_binary("0.33.0", installed_secs_ago=LONG_AGO).parent
        installing = self.legacy_binary("0.32.0", installed_secs_ago=LONG_AGO).parent
        (installing.parent / ".lock-0.32.0").mkdir()

        self.launch()

        self.assertFalse(abandoned.exists(), "an old version under bin/ that nothing runs from is reclaimed")
        self.assertTrue(installing.is_dir(), "one whose install lock is held is left to its launcher")

    def test_a_version_of_a_launcher_up_to_0_33_that_still_runs_is_kept(self) -> None:
        binary = self.legacy_binary("0.33.0", installed_secs_ago=LONG_AGO)
        daemon = self.daemon(binary)

        self.launch()

        self.assertTrue(binary.is_symlink(), "a 0.33 daemon still runs from it")
        self.assertTrue(alive(daemon), "and is not stopped")

    def git_bash(self, *, processes: list[Path] | None, cygpath: bool = True) -> Path:
        """A directory to put first on PATH so the launcher finds itself under Git Bash.

        `uname -s` names MSYS. `ps` refuses every POSIX option, as MSYS ps does, and for `-W` prints
        `processes` the way it lists Windows executables, or fails when that is `None`. `cygpath -m`
        maps a path to a drive, as the real one does.
        """
        tools = self.root / "git-bash"
        tools.mkdir()
        current = self.version_dir(CURRENT)
        shutil.copy2(current / "basemind", current / "basemind.exe")
        header = "      PID    PPID    PGID     WINPID   TTY         UID    STIME COMMAND"
        rows = [
            f"     {4000 + n}       1    {4000 + n}      {9000 + n}  ?         197609   Nov 15 {windows_path(path)}"
            for n, path in enumerate(processes or [])
        ]
        rows.append("I    5000    4000    5000       9900  pty0      197609 10:22:33 /usr/bin/bash")
        (tools / "ps-W.txt").write_text("\n".join([header, *rows]) + "\n")
        listing = f'cat "{tools}/ps-W.txt"' if processes is not None else "exit 1"
        scripts = {
            "uname": f'#!/bin/sh\n[ "$1" = -s ] && echo MINGW64_NT-10.0-19045 && exit 0\nexec {shutil.which("uname")} "$@"\n',
            "ps": f'#!/bin/sh\n[ "$1" = -W ] || exit 1\n{listing}\n',
        }
        if cygpath:
            scripts["cygpath"] = '#!/bin/sh\n[ "$1" = -m ] || exit 1\nprintf \'C:%s\\n\' "$2"\n'
        for name, body in scripts.items():
            (tools / name).write_text(body)
            (tools / name).chmod(0o755)
        return tools

    def test_git_bash_prunes_an_unused_version_and_keeps_one_a_process_runs_from(self) -> None:
        """MSYS ps rejects `-A -ww -o args=`; without a listing of its own nothing would ever be pruned."""
        in_use = self.other_version_binary(IN_USE)
        unused = self.other_version_binary(UNUSED).parent
        tools = self.git_bash(processes=[in_use.parent / "basemind.exe"])

        self.launch(path_prefix=tools)

        self.assertFalse(unused.exists(), "a version no Windows process runs from is removed")
        self.assertTrue(in_use.is_symlink(), "one that `ps -W` shows a process running from is kept")

    def test_git_bash_prunes_nothing_when_its_process_list_cannot_be_read(self) -> None:
        unused = self.other_version_binary(UNUSED).parent
        tools = self.git_bash(processes=None)

        self.launch(path_prefix=tools)

        self.assertTrue(unused.is_dir(), "without a process list every version counts as in use")

    def test_git_bash_prunes_nothing_when_a_path_cannot_be_converted(self) -> None:
        unused = self.other_version_binary(UNUSED).parent
        tools = self.git_bash(processes=[], cygpath=False)

        self.launch(path_prefix=tools)

        self.assertTrue(unused.is_dir(), "a cache path with no Windows spelling cannot be matched")

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


class CachePathWithBackslashTests(OtherVersionsTests):
    """The same contract when the cache directory's name holds a backslash, which awk would unescape."""

    CACHE_NAME = "ca\\che\\new"

    @unittest.skip("a Windows path has no backslash in a name: there it separates directories")
    def test_git_bash_prunes_an_unused_version_and_keeps_one_a_process_runs_from(self) -> None:
        pass


@unittest.skipIf(sys.platform == "win32", "the launcher runs under bash")
class HostDetectionTests(unittest.TestCase):
    """The launcher names the harness in BASEMIND_HOST before it execs the binary."""

    HARNESS_VARIABLES = (
        "BASEMIND_HOST",
        "CURSOR_PLUGIN_ROOT",
        "COPILOT_CLI",
        "CLAUDECODE",
        "CLAUDE_PLUGIN_ROOT",
        "GEMINI_CLI",
        "PLUGIN_ROOT",
        "CODEX_PLUGIN_ROOT",
    )

    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        plugin = self.root / "plugin"
        (plugin / ".claude-plugin").mkdir(parents=True)
        (plugin / ".claude-plugin/plugin.json").write_text(json.dumps({"version": CURRENT}))
        (plugin / "scripts").mkdir()
        shutil.copyfile(LAUNCHER, plugin / "scripts/mcp-launch.sh")
        self.launcher = plugin / "scripts/mcp-launch.sh"
        self.binary = self.root / "basemind"
        self.binary.write_text(
            f'#!/bin/sh\n[ "$1" = --version ] && echo "basemind {CURRENT}" && exit 0\nprintf "%s" "${{BASEMIND_HOST-unset}}"\n'
        )
        self.binary.chmod(0o755)

    def host(self, parent: str = "/bin/bash", **harness: str) -> str:
        """The BASEMIND_HOST the binary sees when `parent` starts the launcher with only `harness` set."""
        env = {key: value for key, value in os.environ.items() if key not in self.HARNESS_VARIABLES}
        env.update(
            HOME=str(self.root), XDG_CACHE_HOME=str(self.root / "cache"), BASEMIND_BIN=str(self.binary), **harness
        )
        return subprocess.run(
            # The trailing `:` keeps `parent` from exec-ing the launcher, so it stays its parent.
            [parent, "-c", '/bin/bash "$0" serve; :', str(self.launcher)],
            env=env,
            capture_output=True,
            text=True,
            timeout=30,
            check=True,
        ).stdout

    def test_each_harness_is_named(self) -> None:
        self.assertEqual(self.host(CURSOR_PLUGIN_ROOT="/p"), "cursor")
        self.assertEqual(self.host(COPILOT_CLI="1"), "copilot")
        self.assertEqual(self.host(CLAUDECODE="1"), "claude")
        self.assertEqual(self.host(CLAUDE_PLUGIN_ROOT=str(self.root / "plugin")), "claude")
        self.assertEqual(self.host(GEMINI_CLI="1"), "gemini")

    def test_a_harness_that_shares_claudes_variable_is_named_for_itself(self) -> None:
        plugin = str(self.root / "plugin")
        self.assertEqual(self.host(CLAUDE_PLUGIN_ROOT=plugin, CURSOR_PLUGIN_ROOT="/p"), "cursor")
        self.assertEqual(self.host(CLAUDE_PLUGIN_ROOT=plugin, COPILOT_CLI="1"), "copilot")

    def test_claude_code_and_codex_name_one_host_for_serve_and_the_hooks(self) -> None:
        plugin = str(self.root / "plugin")
        # Observed: Claude Code gives hooks and its MCP server CLAUDECODE and CLAUDE_PLUGIN_ROOT.
        self.assertEqual(self.host(CLAUDECODE="1", CLAUDE_PLUGIN_ROOT=plugin), "claude")
        # Codex 0.169 gives plugin hooks PLUGIN_ROOT and CLAUDE_PLUGIN_ROOT, and starts its MCP
        # servers itself with neither, so `serve` is known by its parent.
        self.assertEqual(self.host(PLUGIN_ROOT=plugin, CLAUDE_PLUGIN_ROOT=plugin), "codex")
        self.assertEqual(self.host(CODEX_PLUGIN_ROOT=plugin), "codex")
        codex = self.root / "bin" / "codex"
        codex.parent.mkdir()
        codex.symlink_to("/bin/bash")
        self.assertEqual(self.host(parent=str(codex)), "codex")

    def test_codex_started_from_claude_code_names_codex_for_its_hooks(self) -> None:
        plugin = str(self.root / "plugin")
        # Codex hooks inherit the environment Codex was started in, CLAUDECODE included.
        self.assertEqual(self.host(CLAUDECODE="1", PLUGIN_ROOT=plugin, CLAUDE_PLUGIN_ROOT=plugin), "codex")
        self.assertEqual(self.host(CLAUDECODE="1", CODEX_PLUGIN_ROOT=plugin), "codex")

    def test_no_harness_leaves_the_host_unset(self) -> None:
        self.assertEqual(self.host(), "unset")

    def test_an_explicit_host_wins(self) -> None:
        self.assertEqual(self.host(BASEMIND_HOST="mine", CLAUDECODE="1", CURSOR_PLUGIN_ROOT="/p"), "mine")


@unittest.skipIf(sys.platform == "win32" or shutil.which("node") is None, "needs node and a Unix shell")
class McpJsonFallbackTests(unittest.TestCase):
    """The Codex config's fallback, used when no installed plugin holds codex-mcp-launch.mjs."""

    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        server = json.loads((ROOT / ".mcp.json").read_text())["mcpServers"]["basemind"]
        self.command = [shutil.which("node"), *server["args"]]
        (self.root / "codex-home").mkdir()
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.work = self.root / "work"
        (self.work / "scripts").mkdir(parents=True)
        report = '#!/bin/sh\nprintf "%s %s" "$(basename "$0")" "${BASEMIND_HOST-unset}"\n'
        for stub in (self.work / "scripts/mcp-launch.sh", self.bin / "basemind"):
            stub.write_text(report)
            stub.chmod(0o755)

    def host(self, *, launcher: bool, **harness: str) -> str:
        if not launcher:
            (self.work / "scripts/mcp-launch.sh").unlink()
        env = {key: value for key, value in os.environ.items() if key not in HostDetectionTests.HARNESS_VARIABLES}
        env.update(
            CODEX_HOME=str(self.root / "codex-home"),
            PATH=f"{self.bin}{os.pathsep}{os.environ['PATH']}",
            **harness,
        )
        return subprocess.run(
            self.command, cwd=self.work, env=env, capture_output=True, text=True, timeout=30, check=True
        ).stdout

    def test_codex_is_named_through_the_launcher(self) -> None:
        self.assertEqual(self.host(launcher=True), "mcp-launch.sh codex")

    def test_codex_is_named_for_a_bare_binary(self) -> None:
        self.assertEqual(self.host(launcher=False), "basemind codex")

    def test_claude_code_running_the_development_config_is_named_claude(self) -> None:
        self.assertEqual(self.host(launcher=True, CLAUDECODE="1"), "mcp-launch.sh claude")

    def test_an_explicit_host_wins(self) -> None:
        self.assertEqual(self.host(launcher=False, BASEMIND_HOST="mine"), "basemind mine")

    def test_the_installed_codex_launcher_names_the_same_host(self) -> None:
        script = (ROOT / "scripts/codex-mcp-launch.mjs").as_uri()
        probe = f'import("{script}").then(m => process.stdout.write(m.launchEnvironment().BASEMIND_HOST))'

        def host(**harness: str) -> str:
            env = {key: value for key, value in os.environ.items() if key not in HostDetectionTests.HARNESS_VARIABLES}
            env.update(harness)
            node = [shutil.which("node"), "-e", probe]
            return subprocess.run(node, env=env, capture_output=True, text=True, timeout=30, check=True).stdout

        self.assertEqual(host(), "codex")
        self.assertEqual(host(CLAUDECODE="1"), "claude")
        self.assertEqual(host(BASEMIND_HOST="mine", CLAUDECODE="1"), "mine")


@unittest.skipIf(sys.platform == "win32" or shutil.which("node") is None, "needs node")
class McpJsonCachedLauncherTests(unittest.TestCase):
    """Which installed plugin's codex-mcp-launch.mjs the Codex config runs."""

    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        server = json.loads((ROOT / ".mcp.json").read_text())["mcpServers"]["basemind"]
        self.command = [shutil.which("node"), *server["args"]]
        self.codex_home = self.root / "codex-home"
        self.codex_home.mkdir()
        self.work = self.root / "work"
        self.work.mkdir()

    def install(self, marketplace: str, version: str, *, modified_ago: int = 0) -> Path:
        """Cache a plugin `version` from `marketplace` whose launcher prints where it came from."""
        launcher = (
            self.codex_home / "plugins/cache" / marketplace / "basemind" / version / "scripts/codex-mcp-launch.mjs"
        )
        launcher.parent.mkdir(parents=True)
        label = f"{marketplace} {version}"
        launcher.write_text(f'export function run(args) {{ process.stdout.write("{label} " + args.join(" ")); }}\n')
        age(launcher, modified_ago)
        return launcher

    def configure(self, text: str) -> None:
        (self.codex_home / "config.toml").write_text(text)

    def launch(self) -> subprocess.CompletedProcess[str]:
        env = {key: value for key, value in os.environ.items() if key not in HostDetectionTests.HARNESS_VARIABLES}
        env.pop("BASEMIND_CODEX_HOME", None)
        env["CODEX_HOME"] = str(self.codex_home)
        return subprocess.run(
            self.command, cwd=self.work, env=env, capture_output=True, text=True, timeout=30, check=True
        )

    def assert_runs(self, launcher: Path, label: str) -> None:
        result = self.launch()
        self.assertEqual(result.stdout, f"{label} serve")
        self.assertEqual(result.stderr, f"basemind: launching {launcher}\n")

    def test_the_highest_version_runs_rather_than_the_newest_file(self) -> None:
        highest = self.install("basemind", "0.10.0", modified_ago=DAY)
        self.install("basemind", "0.9.0")
        self.assert_runs(highest, "basemind 0.10.0")

    def test_a_release_outranks_its_prerelease(self) -> None:
        release = self.install("basemind", "1.0.0", modified_ago=DAY)
        self.install("basemind", "1.0.0-rc.1")
        self.assert_runs(release, "basemind 1.0.0")

    def clear_cache(self) -> None:
        shutil.rmtree(self.codex_home / "plugins/cache", ignore_errors=True)

    def test_prereleases_rank_by_their_identifiers_rather_than_file_age(self) -> None:
        for higher, lower in (
            ("1.0.0-rc.2", "1.0.0-rc.1"),
            ("1.0.0-rc.10", "1.0.0-rc.2"),
            ("1.0.0-rc.1.1", "1.0.0-rc.1"),
            ("1.0.0-beta", "1.0.0-alpha"),
            ("1.0.0-alpha", "1.0.0-1"),
        ):
            with self.subTest(higher=higher, lower=lower):
                self.clear_cache()
                expected = self.install("basemind", higher, modified_ago=DAY)
                self.install("basemind", lower)
                self.assert_runs(expected, f"basemind {higher}")

    def test_without_a_config_only_the_basemind_marketplace_is_searched(self) -> None:
        expected = self.install("basemind", "1.0.0", modified_ago=DAY)
        self.install("foreign", "9.9.9")
        self.assert_runs(expected, "basemind 1.0.0")

    def test_the_enabled_plugins_marketplace_wins_over_a_higher_stale_copy(self) -> None:
        self.install("basemind", "9.9.9")
        enabled = self.install("curated", "1.0.0", modified_ago=DAY)
        self.configure('model = "m"\n\n[plugins."basemind@curated"]\nenabled = true\n')
        self.assert_runs(enabled, "curated 1.0.0")

    def test_a_disabled_plugin_is_passed_over(self) -> None:
        self.install("curated", "9.9.9")
        enabled = self.install("basemind", "1.0.0", modified_ago=DAY)
        self.configure(
            '[plugins."basemind@curated"]\nenabled = false\n\n[plugins."basemind@basemind"]\nenabled = true\n'
        )
        self.assert_runs(enabled, "basemind 1.0.0")

    def test_a_quoted_enabled_key_disables_the_plugin(self) -> None:
        for key in ('"enabled"', "'enabled'"):
            with self.subTest(key=key):
                self.clear_cache()
                self.install("curated", "9.9.9")
                enabled = self.install("basemind", "1.0.0", modified_ago=DAY)
                self.configure(
                    f'[plugins."basemind@curated"]\n{key} = false\n\n[plugins."basemind@basemind"]\nenabled = true\n'
                )
                self.assert_runs(enabled, "basemind 1.0.0")

    def test_table_shaped_text_inside_a_multiline_string_is_not_a_table(self) -> None:
        for quotes in ("'''", '"""'):
            for config in (
                f'developer_instructions = {quotes}\n[plugins."basemind@curated"]\nenabled = true\n{quotes}\n\n'
                '[plugins."basemind@curated"]\nenabled = false\n\n[plugins."basemind@basemind"]\nenabled = true\n',
                f'[plugins."basemind@curated"]\nnote = {quotes}\n[example]\n{quotes}\nenabled = false\n\n'
                '[plugins."basemind@basemind"]\nenabled = true\n',
            ):
                with self.subTest(config=config):
                    self.clear_cache()
                    self.install("curated", "9.9.9")
                    enabled = self.install("basemind", "1.0.0", modified_ago=DAY)
                    self.configure(config)
                    self.assert_runs(enabled, "basemind 1.0.0")

    def test_when_every_configured_plugin_is_disabled_no_cached_launcher_runs(self) -> None:
        self.install("basemind", "1.0.0")
        self.install("curated", "1.0.0")
        local = self.work / "scripts/mcp-launch.sh"
        local.parent.mkdir()
        local.write_text('#!/bin/sh\nprintf "local %s" "$*"\n')
        local.chmod(0o755)
        self.configure('[plugins."basemind@curated"]\nenabled = false\n')
        result = self.launch()
        self.assertEqual(result.stdout, "local serve")
        self.assertEqual(result.stderr, "")

    def test_the_highest_version_of_the_enabled_plugin_runs(self) -> None:
        self.install("basemind", "9.9.9")
        highest = self.install("curated", "2.0.0", modified_ago=DAY)
        self.install("curated", "1.5.0")
        self.configure('[plugins."basemind@curated"]\nenabled = true\n[other]\nenabled = false\n')
        self.assert_runs(highest, "curated 2.0.0")


if __name__ == "__main__":
    unittest.main()
