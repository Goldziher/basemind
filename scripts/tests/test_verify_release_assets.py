"""Drive .github/scripts/verify-release-assets.sh against a stand-in download helper."""

import hashlib
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[2] / ".github" / "scripts" / "verify-release-assets.sh"
VERSION = "1.2.3"
ARCHIVES = (
    "basemind-x86_64-unknown-linux-gnu.tar.gz",
    "basemind-aarch64-unknown-linux-gnu.tar.gz",
    "basemind-aarch64-apple-darwin.tar.gz",
    "basemind-x86_64-pc-windows-msvc.zip",
)

FAKE_HELPER = """#!/usr/bin/env bash
set -euo pipefail
[ "$1" = download ] || exit 2
[ -f "$FAKE_DRAFT/$3" ] || { echo "not found: $3" >&2; exit 1; }
cp "$FAKE_DRAFT/$3" "$4"
"""


class VerifyReleaseAssets(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        root = Path(self.tmp.name)
        self.draft = root / "draft"
        self.draft.mkdir()
        self.helper = root / "helper.sh"
        self.helper.write_text(FAKE_HELPER)
        for name in ARCHIVES:
            (self.draft / name).write_bytes(name.encode())
        self.write_checksums()

    def write_checksums(self, skip: tuple[str, ...] = ()) -> None:
        lines = [f"{hashlib.sha256((self.draft / n).read_bytes()).hexdigest()}  {n}" for n in ARCHIVES if n not in skip]
        (self.draft / f"basemind_{VERSION}_checksums.txt").write_text("\n".join(lines) + "\n")

    def run_script(self) -> subprocess.CompletedProcess[str]:
        env = {**os.environ, "RELEASE_ASSET_SH": str(self.helper), "FAKE_DRAFT": str(self.draft)}
        return subprocess.run(
            ["bash", str(SCRIPT), "42", VERSION], env=env, capture_output=True, text=True, check=False
        )

    def test_matching_archives_pass(self) -> None:
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("verified 4 archives", result.stdout)

    def test_stale_checksums_for_a_replaced_archive_fail(self) -> None:
        (self.draft / ARCHIVES[0]).write_bytes(b"new build")
        result = self.run_script()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("do not match", result.stderr)

    def test_archive_missing_from_checksums_fails(self) -> None:
        self.write_checksums(skip=(ARCHIVES[3],))
        result = self.run_script()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(f"does not list {ARCHIVES[3]}", result.stderr)

    def test_missing_archive_on_draft_fails(self) -> None:
        (self.draft / ARCHIVES[1]).unlink()
        self.assertNotEqual(self.run_script().returncode, 0)


if __name__ == "__main__":
    unittest.main()
