"""Every generated plugin manifest starts the MCP server through a command its host can launch.

A host spawns the manifest's command without a shell, and Windows cannot run a shell script itself,
so the manifests name the polyglot `scripts/mcp-launch.cmd`: cmd.exe runs its batch section, which
finds Git Bash, and Unix runs it through its shebang. Both forward every argument to
`mcp-launch.sh`.
"""

import json
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
PLUGIN = ROOT / "plugin"
SHIM = "scripts/mcp-launch.cmd"
SOURCE_SHIM = PLUGIN / ".ai-rulez" / SHIM

# Each generated manifest, with the plugin-root prefix its host resolves the shim against.
MANIFESTS = (
    (".claude-plugin/plugin.json", "${CLAUDE_PLUGIN_ROOT}/"),
    (".cursor-plugin/plugin.json", "./"),
    (".mcp.json", "./"),
    ("mcp.json", "./"),
    ("kimi.plugin.json", "./"),
    ("gemini-extension.json", "${extensionPath}/"),
)

# Stands in for mcp-launch.sh: one line per argument, then an exit status the shim must pass on.
STUB_LAUNCHER = """#!/usr/bin/env bash
for arg in "$@"; do printf 'arg:[%s]\\n' "$arg"; done
exit 7
"""


def server_command_line(manifest: str) -> list[str]:
    """The basemind server's command followed by its arguments, as the manifest declares them."""
    server = json.loads((PLUGIN / manifest).read_text())["mcpServers"]["basemind"]
    return [server["command"], *server["args"]]


def shim_with_stub_launcher(directory: Path) -> Path:
    """Copy the shipped shim into `directory`/scripts beside a stub launcher; return the shim."""
    scripts = directory / "scripts"
    scripts.mkdir()
    shim = scripts / "mcp-launch.cmd"
    shutil.copyfile(PLUGIN / SHIM, shim)
    launcher = scripts / "mcp-launch.sh"
    launcher.write_text(STUB_LAUNCHER, newline="\n")
    for path in (shim, launcher):
        path.chmod(0o755)
    return shim


def forwarded_args(stdout: str) -> list[str]:
    """The arguments the stub launcher received, ignoring any other output line."""
    return [line[len("arg:[") : -1] for line in stdout.splitlines() if line.startswith("arg:[")]


class ManifestTests(unittest.TestCase):
    def test_every_manifest_starts_the_shim_with_serve(self) -> None:
        for manifest, root in MANIFESTS:
            with self.subTest(manifest=manifest):
                self.assertEqual(server_command_line(manifest), [f"{root}{SHIM}", "serve"])

    def test_the_opencode_bundle_starts_the_shim_with_serve(self) -> None:
        bundle = json.loads((PLUGIN / ".opencode/ai-rulez-bundle.json").read_text())
        self.assertEqual(bundle["mcp"]["basemind"]["command"], [f"${{PLUGIN_ROOT}}/{SHIM}", "serve"])

    def test_the_shim_ships_executable_and_matches_its_source(self) -> None:
        shipped = PLUGIN / SHIM
        self.assertEqual(shipped.read_bytes(), SOURCE_SHIM.read_bytes())
        tracked = subprocess.run(
            ["git", "ls-files", "--stage", SHIM], cwd=PLUGIN, capture_output=True, text=True, check=True
        )
        self.assertEqual(tracked.stdout.split()[0], "100755")

    def test_the_shim_starts_with_a_shebang_and_runs_the_launcher_beside_it(self) -> None:
        text = SOURCE_SHIM.read_text()
        self.assertEqual(text.splitlines()[:3], ["#!/usr/bin/env bash", ": << 'CMDBLOCK'", "@echo off"])
        self.assertIn('"%GIT_BASH%" "%~dp0mcp-launch.sh" %*\n', text)
        self.assertIn("Git for Windows", text)
        self.assertNotIn(
            b"\r", SOURCE_SHIM.read_bytes(), "bash reads the Unix section, so the file keeps LF line endings"
        )


@unittest.skipIf(sys.platform == "win32", "the Unix section runs through the shebang")
class UnixShimTests(unittest.TestCase):
    def test_the_host_execs_the_shim_directly_and_every_argument_reaches_the_launcher(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            shim = shim_with_stub_launcher(Path(temporary))
            self.assertTrue(shim.stat().st_mode & stat.S_IXUSR)
            result = subprocess.run(
                [shim, "serve", "--root", "/work/with space", ""], capture_output=True, text=True, check=False
            )
        self.assertEqual(result.stdout, "arg:[serve]\narg:[--root]\narg:[/work/with space]\narg:[]\n")
        self.assertEqual(result.stderr, "")
        self.assertEqual(result.returncode, 7)


@unittest.skipUnless(sys.platform == "win32", "the batch section runs only under cmd.exe")
class WindowsShimTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)
        self.shim = shim_with_stub_launcher(self.directory)
        self.empty = self.directory / "empty"
        self.empty.mkdir()
        system32 = Path(os.environ["SystemRoot"]) / "System32"
        # Only cmd.exe's own directory on PATH, and every Git install location pointed at an empty
        # directory, so a test that wants Git Bash names it explicitly.
        self.bare_env = {
            "SystemRoot": os.environ["SystemRoot"],
            "ComSpec": os.environ["ComSpec"],
            "PATH": str(system32),
            "PATHEXT": os.environ.get("PATHEXT", ".COM;.EXE;.BAT;.CMD"),
            "ProgramFiles": str(self.empty),
            "ProgramFiles(x86)": str(self.empty),
            "LOCALAPPDATA": str(self.empty),
            "TEMP": str(self.directory),
            "TMP": str(self.directory),
        }

    def git_bash(self) -> str:
        """The runner's Git Bash, which every GitHub Windows image installs."""
        for base in (os.environ.get("ProgramFiles"), os.environ.get("ProgramFiles(x86)")):
            candidate = Path(base or "") / "Git" / "bin" / "bash.exe"
            if candidate.is_file():
                return str(candidate)
        self.fail("no Git Bash under Program Files")

    def run_shim(self, env: dict[str, str]) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [str(self.shim), "serve", "--root", r"C:\work\with space"],
            capture_output=True,
            text=True,
            env=env,
            check=False,
        )

    def test_the_installed_git_bash_runs_the_launcher_with_every_argument(self) -> None:
        result = self.run_shim(os.environ.copy())
        self.assertEqual(forwarded_args(result.stdout), ["serve", "--root", r"C:\work\with space"])
        self.assertEqual(result.returncode, 7)

    def test_claude_code_git_bash_path_is_used_when_no_install_location_holds_bash(self) -> None:
        result = self.run_shim({**self.bare_env, "CLAUDE_CODE_GIT_BASH_PATH": self.git_bash()})
        self.assertEqual(forwarded_args(result.stdout), ["serve", "--root", r"C:\work\with space"])
        self.assertEqual(result.returncode, 7)

    def test_a_missing_git_bash_fails_with_a_message_naming_git_for_windows(self) -> None:
        result = self.run_shim(self.bare_env)
        self.assertEqual(forwarded_args(result.stdout), [])
        self.assertIn(
            "basemind-launch: error: the basemind MCP server needs bash from Git for Windows",
            result.stderr,
        )
        self.assertEqual(result.returncode, 1)


if __name__ == "__main__":
    unittest.main()
