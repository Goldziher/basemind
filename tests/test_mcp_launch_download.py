"""The launcher's download path: checksum verification, the release-window fallback, a release
mirror and platforms.

GitHub and the mirror are replaced by a fake `curl` on PATH that serves files from a local directory
laid out like the release download URLs, so every case runs offline and deterministically.
"""

import json
import os
import platform
import shutil
import subprocess
import sys
import tarfile
import tempfile
import textwrap
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LAUNCHER = ROOT / "plugin/.ai-rulez/scripts/mcp-launch.sh"
GITHUB_RELEASES = "Goldziher/basemind/releases/download"
MIRROR = "https://mirror.example/basemind"

FAKE_CURL = r'''#!{python}
"""Serve curl's -o / -w '%{{url_effective}}' requests from FAKE_GITHUB."""
import os, shutil, sys
args = sys.argv[1:]
out = None
write_out = None
url = None
i = 0
while i < len(args):
    a = args[i]
    if a == "-o":
        out = args[i + 1]; i += 2; continue
    if a == "-w":
        write_out = args[i + 1]; i += 2; continue
    if a == "--retry":
        i += 2; continue
    if a.startswith("-"):
        i += 1; continue
    url = a; i += 1
root = os.environ["FAKE_GITHUB"]
if os.environ.get("FAKE_OFFLINE"):
    sys.exit(6)
with open(os.path.join(root, "requests.log"), "a") as log:
    log.write(url + "\n")
if url.endswith("/releases/latest") and write_out:
    latest = os.environ.get("FAKE_LATEST", "")
    if not latest:
        sys.exit(22)
    sys.stdout.write("https://github.com/Goldziher/basemind/releases/tag/v" + latest)
    sys.exit(0)
path = url.split("://", 1)[1].split("/", 1)[1].split("?", 1)[0]
source = os.path.join(root, path)
if not os.path.isfile(source):
    sys.exit(22)
if out and out != "/dev/null":
    shutil.copyfile(source, out)
'''


def host_triple() -> str:
    machine = platform.machine().lower()
    if sys.platform == "darwin":
        return "aarch64-apple-darwin"
    return "aarch64-unknown-linux-gnu" if machine in ("aarch64", "arm64") else "x86_64-unknown-linux-gnu"


@unittest.skipIf(sys.platform == "win32", "the launcher runs under bash")
class DownloadTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.github = self.root / "github"
        self.bin = self.root / "bin"
        self.bin.mkdir()
        curl = self.bin / "curl"
        curl.write_text(FAKE_CURL.format(python=os.path.realpath(sys.executable)))
        curl.chmod(0o755)
        self.plugin = self.root / "plugin"
        (self.plugin / ".claude-plugin").mkdir(parents=True)
        self.triple = host_triple()
        self.asset = f"basemind-{self.triple}.tar.gz"
        self.releases: list[str] = []

    def pin(self, version: str) -> None:
        (self.plugin / ".claude-plugin/plugin.json").write_text(json.dumps({"version": version}))

    def publish(self, version: str, *, checksum: str | None = None, under: str = GITHUB_RELEASES) -> None:
        """Lay out v`version`'s archive and checksums file like a GitHub release, at the URL path `under`."""
        directory = self.github / under / f"v{version}"
        directory.mkdir(parents=True)
        staging = self.root / f"stage-{version}"
        staging.mkdir()
        binary = staging / "basemind"
        binary.write_text(
            textwrap.dedent(
                f"""\
                #!/bin/sh
                if [ "$1" = "--version" ]; then echo "basemind {version}"; exit 0; fi
                echo "ran {version} $*"
                """
            )
        )
        binary.chmod(0o755)
        archive = directory / self.asset
        with tarfile.open(archive, "w:gz") as bundle:
            bundle.add(binary, arcname="basemind")
        import hashlib

        digest = checksum or hashlib.sha256(archive.read_bytes()).hexdigest()
        (directory / f"basemind_{version}_checksums.txt").write_text(f"{digest}  {self.asset}\n")
        self.releases.insert(0, version)
        api = self.github / "repos/Goldziher/basemind/releases"
        api.parent.mkdir(parents=True, exist_ok=True)
        api.write_text(json.dumps([{"tag_name": f"v{v}"} for v in self.releases], indent=1))

    def requests(self) -> list[str]:
        log = self.github / "requests.log"
        return log.read_text().splitlines() if log.exists() else []

    def launch(
        self,
        *args: str,
        latest: str = "",
        offline: bool = False,
        mirror: str | None = None,
        extra: dict[str, str] | None = None,
    ) -> subprocess.CompletedProcess:
        env = {
            "PATH": f"{self.bin}{os.pathsep}/usr/bin{os.pathsep}/bin",
            "HOME": str(self.root / "home"),
            "XDG_CACHE_HOME": str(self.root / "cache"),
            "CLAUDE_PLUGIN_ROOT": str(self.plugin),
            "FAKE_GITHUB": str(self.github),
            "FAKE_LATEST": latest,
            "FAKE_OFFLINE": "1" if offline else "",
        }
        if mirror is not None:
            env["BASEMIND_RELEASE_BASE_URL"] = mirror
        env.update(extra or {})
        self.github.mkdir(parents=True, exist_ok=True)
        return subprocess.run(
            ["bash", str(LAUNCHER), *args], env=env, capture_output=True, text=True, timeout=120, check=False
        )

    def test_installs_the_pinned_release_after_verifying_its_checksum(self) -> None:
        self.pin("9.9.0")
        self.publish("9.9.0")
        result = self.launch("serve")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "ran 9.9.0 serve")
        self.assertIn("checksum verified", result.stderr)

    def test_a_checksum_mismatch_installs_nothing(self) -> None:
        self.pin("9.9.0")
        self.publish("9.9.0", checksum="0" * 64)
        result = self.launch("serve")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("checksum mismatch", result.stderr)
        self.assertNotIn("ran ", result.stdout)
        self.assertFalse((self.root / "cache/basemind/versions/9.9.0/basemind").exists())

    def test_the_release_window_falls_back_within_the_pinned_minor(self) -> None:
        self.pin("9.9.1")
        self.publish("9.8.7")
        self.publish("9.9.0")
        result = self.launch("serve", latest="9.9.0")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "ran 9.9.0 serve")

    def test_the_release_window_never_falls_back_to_another_minor(self) -> None:
        self.pin("9.9.0")
        self.publish("9.8.7")
        result = self.launch("serve", latest="9.8.7")
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("ran 9.8.7", result.stdout)
        self.assertIn("v9.9.0", result.stderr)

    def test_an_offline_first_run_blames_the_network_not_the_release(self) -> None:
        self.pin("9.9.0")
        self.publish("9.9.0")
        result = self.launch("serve", offline=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("could not reach GitHub", result.stderr)
        self.assertNotIn("release looks incomplete", result.stderr)

    def test_a_mirror_serves_the_install_and_github_is_never_asked(self) -> None:
        self.pin("9.9.0")
        self.publish("9.9.0", under="basemind")
        result = self.launch("serve", mirror=f"{MIRROR}/")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "ran 9.9.0 serve")
        self.assertIn("checksum verified", result.stderr)
        self.assertEqual(
            self.requests(),
            [f"{MIRROR}/v9.9.0/{self.asset}", f"{MIRROR}/v9.9.0/basemind_9.9.0_checksums.txt"],
        )

    def test_a_mirror_binary_whose_checksum_does_not_match_is_refused(self) -> None:
        self.pin("9.9.0")
        self.publish("9.9.0", checksum="0" * 64, under="basemind")
        result = self.launch("serve", mirror=MIRROR)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("checksum mismatch", result.stderr)
        self.assertNotIn("ran ", result.stdout)
        self.assertFalse((self.root / "cache/basemind/versions/9.9.0/basemind").exists())

    def test_a_mirror_without_the_release_names_the_layout_it_needs(self) -> None:
        self.pin("9.9.0")
        self.publish("9.9.0")
        result = self.launch("serve", mirror=MIRROR)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(
            f"the mirror BASEMIND_RELEASE_BASE_URL={MIRROR} does not serve basemind v9.9.0: it needs "
            f"v9.9.0/{self.asset} and v9.9.0/basemind_9.9.0_checksums.txt",
            result.stderr,
        )
        self.assertEqual([url for url in self.requests() if "github.com" in url], [])

    def test_a_mirror_that_is_not_a_url_is_refused_by_name(self) -> None:
        self.pin("9.9.0")
        result = self.launch("serve", mirror="mirror.example/basemind")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(
            'BASEMIND_RELEASE_BASE_URL="mirror.example/basemind" is not an https://, http:// or file:/// URL',
            result.stderr,
        )
        self.assertEqual(self.requests(), [])

    def cache(self, version: str) -> None:
        """Install v`version` in the launcher's cache, as an earlier launch would have."""
        directory = self.root / "cache/basemind/versions" / version
        directory.mkdir(parents=True)
        binary = directory / "basemind"
        binary.write_text(
            f'#!/bin/sh\nif [ "$1" = "--version" ]; then echo "basemind {version}"; exit 0; fi\necho "ran {version} $*"\n'
        )
        binary.chmod(0o755)

    def test_basemind_offline_downloads_nothing_and_names_what_is_missing(self) -> None:
        self.pin("9.9.0")
        self.publish("9.9.0")
        for value in ("1", "true", " YES ", "on", "On"):
            with self.subTest(value=value):
                result = self.launch("serve", extra={"BASEMIND_OFFLINE": value})
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(
                    f"BASEMIND_OFFLINE is set and basemind v9.9.0 is not installed, so {self.asset} was not "
                    "downloaded. Put the v9.9.0 binary at ",
                    result.stderr,
                )
                self.assertIn("set BASEMIND_BIN to a vendored binary", result.stderr)
                self.assertEqual(self.requests(), [])

    def test_basemind_offline_runs_a_cached_binary_of_the_pinned_minor(self) -> None:
        self.pin("9.9.1")
        self.publish("9.9.1")
        self.cache("9.9.0")
        result = self.launch("serve", extra={"BASEMIND_OFFLINE": "1"})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "ran 9.9.0 serve")
        self.assertEqual(self.requests(), [])

    def test_a_false_basemind_offline_downloads_as_usual(self) -> None:
        self.pin("9.9.0")
        self.publish("9.9.0")
        for value in ("", "  ", "0", "false", "No", "off", "OFF"):
            with self.subTest(value=value):
                shutil.rmtree(self.root / "cache", ignore_errors=True)
                result = self.launch("serve", extra={"BASEMIND_OFFLINE": value})
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.strip(), "ran 9.9.0 serve")

    def test_a_basemind_offline_value_that_is_not_a_boolean_is_refused_by_name(self) -> None:
        self.pin("9.9.0")
        self.publish("9.9.0")
        result = self.launch("serve", extra={"BASEMIND_OFFLINE": "maybe"})
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(
            'BASEMIND_OFFLINE="maybe" is not a boolean: use 1, true, yes, on or 0, false, no, off', result.stderr
        )
        self.assertEqual(self.requests(), [])

    def test_an_unsupported_linux_architecture_is_refused_by_name(self) -> None:
        uname = self.bin / "uname"
        uname.write_text('#!/bin/sh\ncase "$1" in -s) echo Linux ;; -m) echo riscv64 ;; esac\n')
        uname.chmod(0o755)
        self.pin("9.9.0")
        result = self.launch("serve")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Linux on riscv64 is not supported", result.stderr)


if __name__ == "__main__":
    unittest.main()
