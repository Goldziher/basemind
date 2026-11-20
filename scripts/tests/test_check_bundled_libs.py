"""A release archive may bundle only the native libraries allowlisted for its platform."""

import subprocess
import tempfile
import unittest
from pathlib import Path

CHECK = Path(__file__).resolve().parents[1] / "check-bundled-libs.sh"


def run_check(system: str, staging: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(["bash", str(CHECK), system, str(staging)], capture_output=True, text=True, check=False)


class BundledLibsTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.staging = Path(temporary.name)

    def bundle(self, *relative: str, staging: Path | None = None) -> None:
        root = staging or self.staging
        (root / "lib").mkdir(parents=True, exist_ok=True)
        for name in relative:
            (root / name).write_bytes(b"")

    def test_allowlisted_and_empty_bundles_pass(self) -> None:
        cases = (
            ("linux", ("lib/libstdc++.so.6",)),
            ("linux", ()),
            ("macos", ()),
            ("windows", ("basemind.exe", "LICENSE")),
        )
        for system, files in cases:
            with self.subTest(system=system, files=files):
                staging = self.staging / f"{system}-{len(files)}"
                self.bundle(*files, staging=staging)
                result = run_check(system, staging)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout, f"✓ Bundled {system} libraries are all allowlisted\n")
                self.assertEqual(result.stderr, "")

    def test_an_unlisted_library_fails_and_is_named_next_to_an_allowed_one(self) -> None:
        self.bundle("lib/libstdc++.so.6", "lib/libheif.so.1", "lib/libaom.so.3")
        result = run_check("linux", self.staging)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, "")
        self.assertEqual(
            result.stderr,
            f"error: the linux release would bundle libraries that are not on the allowlist in {CHECK}:\n"
            "  libaom.so.3\n"
            "  libheif.so.1\n"
            "Remove the dependency that pulls them in, or allowlist each one after a licence check.\n",
        )

    def test_the_allowlist_is_per_platform(self) -> None:
        self.bundle("lib/libstdc++.so.6")
        result = run_check("macos", self.staging)
        self.assertEqual(result.returncode, 1)
        self.assertIn("\n  libstdc++.so.6\n", result.stderr)

    def test_a_soname_bump_is_not_covered_by_the_old_entry(self) -> None:
        self.bundle("lib/libstdc++.so.7")
        result = run_check("linux", self.staging)
        self.assertEqual(result.returncode, 1)
        self.assertIn("\n  libstdc++.so.7\n", result.stderr)

    def test_a_windows_dll_next_to_the_binary_fails(self) -> None:
        self.bundle("basemind.exe", "heif.dll")
        result = run_check("windows", self.staging)
        self.assertEqual(result.returncode, 1)
        self.assertIn("\n  heif.dll\n", result.stderr)

    def test_an_unknown_system_is_a_usage_error(self) -> None:
        result = run_check("freebsd", self.staging)
        self.assertEqual(result.returncode, 2)
        self.assertEqual(result.stderr, "Unknown system: freebsd (expected linux, macos or windows)\n")


if __name__ == "__main__":
    unittest.main()
