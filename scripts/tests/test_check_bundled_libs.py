"""A release archive may bundle only the native libraries allowlisted for its platform, and every
allowlisted library must have its licence notice in the repository."""

import subprocess
import tempfile
import unittest
from pathlib import Path

CHECK = Path(__file__).resolve().parents[1] / "check-bundled-libs.sh"


def run_check(system: str, staging: Path, check: Path = CHECK) -> subprocess.CompletedProcess[str]:
    return subprocess.run(["bash", str(check), system, str(staging)], capture_output=True, text=True, check=False)


def repo_copy(root: Path, *, notices: tuple[str, ...], attributed: tuple[str, ...], extra_macos: str = "") -> Path:
    """A repository holding the check script, ``licenses/<name>.txt`` for each of ``notices`` and an
    ATTRIBUTIONS.md naming ``attributed``; ``extra_macos`` is added to the macOS allowlist."""
    script = root / "scripts" / CHECK.name
    script.parent.mkdir(parents=True)
    source = CHECK.read_text(encoding="utf-8")
    if extra_macos:
        assert "ALLOWED_MACOS=''" in source
        source = source.replace("ALLOWED_MACOS=''", f"ALLOWED_MACOS='\n{extra_macos}\n'", 1)
    script.write_text(source, encoding="utf-8")
    (root / "licenses").mkdir()
    for name in notices:
        (root / "licenses" / f"{name}.txt").write_text("licence text\n", encoding="utf-8")
    (root / "ATTRIBUTIONS.md").write_text(
        "".join(f"- `licenses/{name}.txt`\n" for name in attributed), encoding="utf-8"
    )
    return script


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

    def test_an_allowlisted_library_without_its_notice_fails(self) -> None:
        cases = (
            ((), ("libstdc++.so.6",), ("licenses/libstdc++.so.6.txt is missing",)),
            (("libstdc++.so.6",), (), ("ATTRIBUTIONS.md does not name licenses/libstdc++.so.6.txt",)),
            (
                (),
                (),
                (
                    "licenses/libstdc++.so.6.txt is missing",
                    "ATTRIBUTIONS.md does not name licenses/libstdc++.so.6.txt",
                ),
            ),
        )
        for index, (notices, attributed, problems) in enumerate(cases):
            with self.subTest(notices=notices, attributed=attributed):
                script = repo_copy(self.staging / f"repo-{index}", notices=notices, attributed=attributed)
                result = run_check("linux", self.staging / "empty", script)
                self.assertEqual(result.returncode, 1)
                self.assertEqual(result.stdout, "")
                self.assertEqual(
                    result.stderr,
                    f"error: libraries allowlisted in {script} lack a licence notice:\n"
                    + "".join(f"  {problem}\n" for problem in problems)
                    + "Add each library's licence text as licenses/<library>.txt and name it in ATTRIBUTIONS.md.\n",
                )

    def test_a_new_entry_on_another_platform_needs_its_own_notice(self) -> None:
        """An entry cannot be added without its notice, even when this platform does not bundle it."""
        script = repo_copy(
            self.staging / "repo",
            notices=("libstdc++.so.6",),
            attributed=("libstdc++.so.6",),
            extra_macos="libomp.dylib",
        )
        result = run_check("linux", self.staging / "empty", script)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(
            result.stderr,
            f"error: libraries allowlisted in {script} lack a licence notice:\n"
            "  licenses/libomp.dylib.txt is missing\n"
            "  ATTRIBUTIONS.md does not name licenses/libomp.dylib.txt\n"
            "Add each library's licence text as licenses/<library>.txt and name it in ATTRIBUTIONS.md.\n",
        )

    def test_a_copy_with_every_notice_passes(self) -> None:
        script = repo_copy(
            self.staging / "repo",
            notices=("libstdc++.so.6", "libomp.dylib"),
            attributed=("libstdc++.so.6", "libomp.dylib"),
            extra_macos="libomp.dylib",
        )
        self.bundle("lib/libomp.dylib")
        result = run_check("macos", self.staging, script)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "✓ Bundled macos libraries are all allowlisted\n")

    def test_an_unknown_system_is_a_usage_error(self) -> None:
        result = run_check("freebsd", self.staging)
        self.assertEqual(result.returncode, 2)
        self.assertEqual(result.stderr, "Unknown system: freebsd (expected linux, macos or windows)\n")


if __name__ == "__main__":
    unittest.main()
