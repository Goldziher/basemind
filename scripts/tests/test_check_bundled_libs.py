"""A release archive may bundle only the native libraries allowlisted for its platform, never x265, and
every allowlisted library must have its licence notice in the repository."""

import subprocess
import tempfile
import unittest
from pathlib import Path

CHECK = Path(__file__).resolve().parents[1] / "check-bundled-libs.sh"
LICENSES = CHECK.parents[1] / "licenses"
NOTICES = ("libde265.txt", "libheif.txt", "libstdc++.so.6.txt")
X265_ADVICE = "Build libheif without x265 (scripts/build-libheif.sh, .github/actions/install-libheif).\n"


def run_check(system: str, staging: Path, check: Path = CHECK) -> subprocess.CompletedProcess[str]:
    return subprocess.run(["bash", str(check), system, str(staging)], capture_output=True, text=True, check=False)


def repo_copy(
    root: Path, *, notices: tuple[str, ...] = NOTICES, attributed: tuple[str, ...] = NOTICES, extra_macos: str = ""
) -> Path:
    """A repository holding the check script, ``licenses/<notice>`` for each of ``notices`` and an
    ATTRIBUTIONS.md naming ``attributed``; ``extra_macos`` is added to the macOS allowlist."""
    script = root / "scripts" / CHECK.name
    script.parent.mkdir(parents=True)
    source = CHECK.read_text(encoding="utf-8")
    if extra_macos:
        assert "ALLOWED_MACOS='\n" in source
        source = source.replace("ALLOWED_MACOS='\n", f"ALLOWED_MACOS='\n{extra_macos}\n", 1)
    script.write_text(source, encoding="utf-8")
    (root / "licenses").mkdir()
    for name in notices:
        (root / "licenses" / name).write_text(f"{name} licence text\n", encoding="utf-8")
    (root / "ATTRIBUTIONS.md").write_text("".join(f"- `licenses/{name}`\n" for name in attributed), encoding="utf-8")
    return script


def x265_error(system: str, *found: str) -> str:
    return (
        f"error: the {system} release would ship x265, a GPL HEVC encoder basemind must not distribute:\n"
        + "".join(f"  {entry}\n" for entry in found)
        + X265_ADVICE
    )


class BundledLibsTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.staging = Path(temporary.name)

    def bundle(self, *relative: str, staging: Path | None = None, content: bytes = b"") -> None:
        root = staging or self.staging
        (root / "lib").mkdir(parents=True, exist_ok=True)
        for name in relative:
            (root / name).write_bytes(content)

    def test_allowlisted_and_empty_bundles_pass_and_ship_each_notice(self) -> None:
        cases = (
            (
                "linux",
                ("lib/libstdc++.so.6", "lib/libheif.so.1", "lib/libde265.so.0"),
                {
                    "libstdc++.so.6.txt": "libstdc++.so.6.txt",
                    "libheif.so.1.txt": "libheif.txt",
                    "libde265.so.0.txt": "libde265.txt",
                },
            ),
            ("linux", (), {}),
            (
                "macos",
                ("lib/libheif.1.dylib", "lib/libde265.0.dylib"),
                {"libheif.1.dylib.txt": "libheif.txt", "libde265.0.dylib.txt": "libde265.txt"},
            ),
            ("macos", (), {}),
            (
                "windows",
                ("basemind.exe", "LICENSE", "heif.dll", "libde265.dll"),
                {"heif.dll.txt": "libheif.txt", "libde265.dll.txt": "libde265.txt"},
            ),
        )
        for index, (system, files, shipped) in enumerate(cases):
            with self.subTest(system=system, files=files):
                staging = self.staging / f"{system}-{index}"
                self.bundle(*files, staging=staging)
                result = run_check(system, staging)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout, f"✓ Bundled {system} libraries are all allowlisted\n")
                self.assertEqual(result.stderr, "")
                licences = staging / "licenses"
                found = sorted(path.name for path in licences.iterdir()) if licences.is_dir() else []
                self.assertEqual(found, sorted(shipped))
                for archived, notice in shipped.items():
                    self.assertEqual((licences / archived).read_bytes(), (LICENSES / notice).read_bytes())

    def test_an_unlisted_library_fails_and_is_named_next_to_an_allowed_one(self) -> None:
        self.bundle("lib/libstdc++.so.6", "lib/libheif.so.1", "lib/libaom.so.3", "lib/libdav1d.so.7")
        result = run_check("linux", self.staging)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, "")
        self.assertEqual(
            result.stderr,
            f"error: the linux release would bundle libraries that are not on the allowlist in {CHECK}:\n"
            "  libaom.so.3\n"
            "  libdav1d.so.7\n"
            "Remove the dependency that pulls them in, or allowlist each one after a licence check.\n",
        )
        self.assertFalse((self.staging / "licenses").exists())

    def test_the_allowlist_is_per_platform(self) -> None:
        cases = (("macos", "lib/libstdc++.so.6"), ("linux", "lib/libheif.1.dylib"), ("linux", "lib/heif.dll"))
        for index, (system, library) in enumerate(cases):
            with self.subTest(system=system, library=library):
                staging = self.staging / f"{system}-{index}"
                self.bundle(library, staging=staging)
                result = run_check(system, staging)
                self.assertEqual(result.returncode, 1)
                self.assertIn(f"would bundle libraries that are not on the allowlist in {CHECK}:\n", result.stderr)
                self.assertIn(f"\n  {Path(library).name}\n", result.stderr)

    def test_a_soname_bump_is_not_covered_by_the_old_entry(self) -> None:
        for library in ("lib/libstdc++.so.7", "lib/libheif.so.2", "lib/libde265.so.1"):
            with self.subTest(library=library):
                staging = self.staging / Path(library).name
                self.bundle(library, staging=staging)
                result = run_check("linux", staging)
                self.assertEqual(result.returncode, 1)
                self.assertIn(f"\n  {Path(library).name}\n", result.stderr)

    def test_a_windows_dll_next_to_the_binary_fails(self) -> None:
        self.bundle("basemind.exe", "heif.dll", "zlib1.dll")
        result = run_check("windows", self.staging)
        self.assertEqual(result.returncode, 1)
        self.assertIn("\n  zlib1.dll\n", result.stderr)
        self.assertNotIn("heif.dll", result.stderr)

    def test_an_x265_library_is_rejected_by_name(self) -> None:
        cases = (
            ("linux", "lib/libx265.so.215", "lib/libx265.so.215 (an x265 library)"),
            ("macos", "lib/libx265.215.dylib", "lib/libx265.215.dylib (an x265 library)"),
            ("windows", "libx265.dll", "libx265.dll (an x265 library)"),
            ("windows", "X265.DLL", "X265.DLL (an x265 library)"),
        )
        for index, (system, library, entry) in enumerate(cases):
            with self.subTest(system=system, library=library):
                staging = self.staging / f"{system}-{index}"
                self.bundle(library, staging=staging)
                result = run_check(system, staging)
                self.assertEqual(result.returncode, 1)
                self.assertEqual(result.stdout, "")
                self.assertEqual(result.stderr, x265_error(system, entry))

    def test_x265_linked_statically_into_the_binary_is_rejected(self) -> None:
        cases = (
            ("linux", "basemind", b"\x00x265 (build %d) - %s:%s - H.265/HEVC codec - \x00", "x265 (build"),
            ("macos", "basemind", b"\x00x265 (build %d)\x00", "x265 (build"),
            ("windows", "basemind.exe", b"MZ\x00x265 (build %d)\x00", "x265 (build"),
            ("linux", "basemind", b"\x00x265 HEVC encoder\x00", "x265 HEVC encoder"),
        )
        for index, (system, binary, content, marker) in enumerate(cases):
            with self.subTest(system=system, marker=marker):
                staging = self.staging / f"{system}-{index}"
                self.bundle(binary, staging=staging, content=content)
                result = run_check(system, staging)
                self.assertEqual(result.returncode, 1)
                self.assertEqual(result.stdout, "")
                self.assertEqual(result.stderr, x265_error(system, f'{binary} (contains "{marker}")'))

    def test_an_allowlisted_libheif_built_with_x265_is_rejected(self) -> None:
        self.bundle("lib/libheif.so.1", content=b"\x00x265 HEVC encoder\x00")
        self.bundle("basemind", content=b"\x7fELF")
        result = run_check("linux", self.staging)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stderr, x265_error("linux", 'lib/libheif.so.1 (contains "x265 HEVC encoder")'))

    def test_a_binary_that_only_mentions_x265_passes(self) -> None:
        self.bundle("basemind", content=b"\x00x265\x00HEVC\x00libx265.so\x00")
        result = run_check("linux", self.staging)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_an_allowlisted_library_without_its_notice_fails(self) -> None:
        cases = (
            (
                ("libde265.txt", "libheif.txt"),
                NOTICES,
                ("licenses/libstdc++.so.6.txt is missing",),
            ),
            (
                NOTICES,
                ("libde265.txt", "libstdc++.so.6.txt"),
                ("ATTRIBUTIONS.md does not name licenses/libheif.txt",),
            ),
            (
                (),
                (),
                (
                    "licenses/libde265.txt is missing",
                    "ATTRIBUTIONS.md does not name licenses/libde265.txt",
                    "licenses/libheif.txt is missing",
                    "ATTRIBUTIONS.md does not name licenses/libheif.txt",
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
                    + "Add each library's licence text under licenses/ and name it in ATTRIBUTIONS.md.\n",
                )

    def test_a_new_entry_on_another_platform_needs_its_own_notice(self) -> None:
        """An entry cannot be added without its notice, even when this platform does not bundle it."""
        script = repo_copy(self.staging / "repo", extra_macos="libomp.dylib libomp.txt")
        result = run_check("linux", self.staging / "empty", script)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(
            result.stderr,
            f"error: libraries allowlisted in {script} lack a licence notice:\n"
            "  licenses/libomp.txt is missing\n"
            "  ATTRIBUTIONS.md does not name licenses/libomp.txt\n"
            "Add each library's licence text under licenses/ and name it in ATTRIBUTIONS.md.\n",
        )

    def test_a_copy_with_every_notice_passes_and_ships_the_mapped_notice(self) -> None:
        script = repo_copy(
            self.staging / "repo",
            notices=(*NOTICES, "libomp.txt"),
            attributed=(*NOTICES, "libomp.txt"),
            extra_macos="libomp.dylib libomp.txt",
        )
        self.bundle("lib/libomp.dylib")
        result = run_check("macos", self.staging, script)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "✓ Bundled macos libraries are all allowlisted\n")
        self.assertEqual(
            (self.staging / "licenses" / "libomp.dylib.txt").read_text(encoding="utf-8"), "libomp.txt licence text\n"
        )

    def test_an_unknown_system_is_a_usage_error(self) -> None:
        result = run_check("freebsd", self.staging)
        self.assertEqual(result.returncode, 2)
        self.assertEqual(result.stderr, "Unknown system: freebsd (expected linux, macos or windows)\n")


if __name__ == "__main__":
    unittest.main()
