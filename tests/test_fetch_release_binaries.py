"""scripts/fetch-release-binaries.sh: the binaries it lists for the upgrade test.

GitHub is replaced by the fake `curl` of test_mcp_launch_download, serving a local directory laid
out like the release download URLs, so every case runs offline.
"""

import hashlib
import os
import subprocess
import sys
import tarfile
import tempfile
import textwrap
import unittest
from pathlib import Path

from test_mcp_launch_download import FAKE_CURL, host_triple

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "scripts/fetch-release-binaries.sh"


@unittest.skipIf(sys.platform == "win32", "the script runs under bash")
class FetchReleaseBinariesTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.github = self.root / "github"
        self.github.mkdir()
        self.bin = self.root / "bin"
        self.bin.mkdir()
        curl = self.bin / "curl"
        curl.write_text(FAKE_CURL.format(python=os.path.realpath(sys.executable)))
        curl.chmod(0o755)
        self.triple = host_triple()
        self.asset = f"basemind-{self.triple}.tar.gz"
        self.out = self.root / "out"

    def publish(self, version: str) -> None:
        """Lay out v`version`'s archive and checksums file like a GitHub release."""
        directory = self.github / "Goldziher/basemind/releases/download" / f"v{version}"
        directory.mkdir(parents=True)
        staging = self.root / f"stage-{version}"
        staging.mkdir()
        binary = staging / "basemind"
        binary.write_text(
            textwrap.dedent(
                f"""\
                #!/bin/sh
                if [ "$1" = "--version" ]; then echo "basemind {version}"; exit 0; fi
                """
            )
        )
        binary.chmod(0o755)
        archive = directory / self.asset
        with tarfile.open(archive, "w:gz") as bundle:
            bundle.add(binary, arcname="basemind")
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        (directory / f"basemind_{version}_checksums.txt").write_text(f"{digest}  {self.asset}\n")

    def fetch(self, *versions: str, latest: str) -> subprocess.CompletedProcess:
        env = {
            "PATH": f"{self.bin}{os.pathsep}/usr/bin{os.pathsep}/bin",
            "HOME": str(self.root / "home"),
            "FAKE_GITHUB": str(self.github),
            "FAKE_LATEST": latest,
        }
        return subprocess.run(
            ["bash", str(SCRIPT), str(self.out), self.triple, *versions],
            env=env,
            capture_output=True,
            text=True,
            timeout=120,
            check=False,
        )

    def test_lists_every_requested_version_in_order(self) -> None:
        self.publish("9.8.0")
        self.publish("9.9.0")
        result = self.fetch("9.8.0", "latest", latest="9.9.0")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            result.stdout.strip(),
            f"{self.out}/9.8.0/basemind:{self.out}/9.9.0/basemind",
        )

    def test_a_latest_that_resolves_to_a_listed_version_is_listed_once(self) -> None:
        self.publish("9.8.0")
        self.publish("9.9.0")
        result = self.fetch("9.8.0", "9.9.0", "latest", latest="9.9.0")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            result.stdout.strip(),
            f"{self.out}/9.8.0/basemind:{self.out}/9.9.0/basemind",
        )
        self.assertIn("9.9.0 already listed", result.stderr)

    def test_the_pinned_0_33_0_hash_rejects_another_archive(self) -> None:
        self.publish("0.33.0")
        result = self.fetch("0.33.0", latest="")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("does not match the hash this repository pins", result.stderr)


if __name__ == "__main__":
    unittest.main()
