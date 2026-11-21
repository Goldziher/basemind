"""The hand-written native-code notices must name the locked crate versions and match the packaging."""

import contextlib
import importlib.util
import io
import tempfile
import unittest
from dataclasses import dataclass, field
from pathlib import Path
from types import ModuleType

ROOT = Path(__file__).resolve().parents[2]
LINKED = ("licenses/a.txt", "licenses/b.txt")
VERSIONS = {"ort-sys": "2.0.0", "xberg-tesseract": "1.4.1", "tree-sitter-language-pack": "1.25.2"}
BUILT = {"libheif": ("1.23.7", "a" * 64), "libde265": ("1.1.3", "b" * 64)}
COMMIT = "c" * 40


def load_checker() -> ModuleType:
    """Load scripts/check_native_notices.py as a module."""
    spec = importlib.util.spec_from_file_location("check_native_notices", ROOT / "scripts/check_native_notices.py")
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def lock_text(versions: dict[str, list[str]]) -> str:
    """A Cargo.lock holding each crate at each of its versions."""
    return "version = 4\n" + "".join(
        f'\n[[package]]\nname = "{name}"\nversion = "{version}"\n'
        for name, crate_versions in versions.items()
        for version in crate_versions
    )


def built_notice(library: str, version: str, sha256: str, commit: str) -> str:
    """A licenses/<library>.txt for ``library`` ``version`` from the tarball ``sha256`` and vcpkg ``commit``."""
    return f"{library} {version}: a library\n  (sha256 {sha256})\n  vcpkg at commit {commit}\n"


def default_built_notices() -> dict[str, str]:
    """The notice of each library in ``BUILT`` that matches its build."""
    return {library: built_notice(library, version, sha256, COMMIT) for library, (version, sha256) in BUILT.items()}


@dataclass(frozen=True)
class Repo:
    """A repository whose Cargo.lock has ``locked``, whose ATTRIBUTIONS.md names each crate of
    ``named`` at its version and each notice of ``attributed``, whose package-release.sh ships
    ``shipped``, whose archive smoke test expects ``checked``, and whose ``present`` notices exist.
    build-libheif.sh pins each library of ``built`` at its version and sha256, install-libheif pins
    vcpkg at ``vcpkg``, ATTRIBUTIONS.md names each label of ``built_named`` at its version, and
    ``built_notices`` holds the text of each ``licenses/<library>.txt``."""

    locked: dict[str, list[str]] = field(default_factory=lambda: {name: [v] for name, v in VERSIONS.items()})
    named: dict[str, str] = field(default_factory=lambda: dict(VERSIONS))
    shipped: tuple[str, ...] = LINKED
    checked: tuple[str, ...] = LINKED
    present: tuple[str, ...] = LINKED
    attributed: tuple[str, ...] = LINKED
    built: dict[str, tuple[str, str]] = field(default_factory=lambda: dict(BUILT))
    vcpkg: str = COMMIT
    built_named: dict[str, str] = field(default_factory=lambda: {name: v for name, (v, _) in BUILT.items()})
    built_notices: dict[str, str] = field(default_factory=default_built_notices)

    def write(self, root: Path) -> Path:
        """Write the repository under ``root`` and return ``root``."""
        (root / "scripts").mkdir(parents=True)
        (root / "licenses").mkdir()
        (root / "Cargo.lock").write_text(lock_text(self.locked), encoding="utf-8")
        (root / "ATTRIBUTIONS.md").write_text(
            "".join(f"- **Checked against**: `{name}` {version}\n" for name, version in self.named.items())
            + "".join(f"- **Licence text**: `{notice}`\n" for notice in self.attributed)
            + "".join(f"- **Checked against**: {label} {v}, built\n" for label, v in self.built_named.items()),
            encoding="utf-8",
        )
        (root / "scripts" / "package-release.sh").write_text(
            'cp LICENSE "$STAGING_DIR/"\nLINKED_NOTICES=(\n'
            + "".join(f"  {notice}\n" for notice in self.shipped)
            + ")\nmkdir -p x\n",
            encoding="utf-8",
        )
        (root / "scripts" / "smoke_release_archive.py").write_text(
            f"LINKED_NOTICES = {self.checked!r}\n", encoding="utf-8"
        )
        for notice in self.present:
            (root / notice).write_text("licence text\n", encoding="utf-8")
        (root / "scripts" / "build-libheif.sh").write_text(
            "set -euo pipefail\n\n"
            + "".join(
                f"{name.upper()}_VERSION={version}\n{name.upper()}_SHA256={sha256}\n"
                for name, (version, sha256) in self.built.items()
            ),
            encoding="utf-8",
        )
        action = root / ".github" / "actions" / "install-libheif" / "action.yml"
        action.parent.mkdir(parents=True)
        action.write_text(f'        Add-Content -Value "BASEMIND_VCPKG_COMMIT={self.vcpkg}"\n', encoding="utf-8")
        for library, notice in self.built_notices.items():
            (root / "licenses" / f"{library}.txt").write_text(notice, encoding="utf-8")
        return root


class ProblemsTests(unittest.TestCase):
    def setUp(self) -> None:
        self.checker = load_checker()
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def test_the_repository_notices_are_current(self) -> None:
        self.assertEqual(self.checker.problems(ROOT), [])

    def test_a_consistent_repository_has_no_problems(self) -> None:
        self.assertEqual(self.checker.problems(Repo().write(self.root / "repo")), [])

    def test_a_locked_version_attributions_does_not_name_is_stale(self) -> None:
        cases = (
            (
                {"ort-sys": ["2.0.1"], "xberg-tesseract": ["1.4.1"], "tree-sitter-language-pack": ["1.25.2"]},
                ["Cargo.lock has ort-sys 2.0.1, ATTRIBUTIONS.md does not name `ort-sys` 2.0.1"],
            ),
            (
                {"ort-sys": ["2.0.0"], "xberg-tesseract": ["1.4.1", "1.5.0"], "tree-sitter-language-pack": ["1.25.2"]},
                ["Cargo.lock has xberg-tesseract 1.5.0, ATTRIBUTIONS.md does not name `xberg-tesseract` 1.5.0"],
            ),
            (
                {"ort-sys": ["2.0.0"], "xberg-tesseract": ["1.4.1"]},
                ["tree-sitter-language-pack is not in Cargo.lock; drop it from PINNED and ATTRIBUTIONS.md"],
            ),
        )
        for index, (locked, expected) in enumerate(cases):
            with self.subTest(locked=locked):
                repo = Repo(locked=locked).write(self.root / f"repo-{index}")
                self.assertEqual(self.checker.problems(repo), expected)

    def test_a_version_is_named_only_by_the_whole_token(self) -> None:
        cases = (
            ("2.0.0", "2.0.0-rc.15", ["Cargo.lock has ort-sys 2.0.0, ATTRIBUTIONS.md does not name `ort-sys` 2.0.0"]),
            ("2.0.0-rc.15", "2.0.0-rc.15", []),
            (
                "2.0.0-rc.1",
                "2.0.0-rc.15",
                ["Cargo.lock has ort-sys 2.0.0-rc.1, ATTRIBUTIONS.md does not name `ort-sys` 2.0.0-rc.1"],
            ),
            ("2.0.0", "2.0.0.", []),
        )
        for index, (locked, named, expected) in enumerate(cases):
            with self.subTest(locked=locked, named=named):
                repo = Repo(
                    locked={**{name: [v] for name, v in VERSIONS.items()}, "ort-sys": [locked]},
                    named={**VERSIONS, "ort-sys": named},
                ).write(self.root / f"repo-{index}")
                self.assertEqual(self.checker.problems(repo), expected)

    def test_packaging_and_smoke_lists_must_agree(self) -> None:
        repo = Repo(checked=("licenses/a.txt",)).write(self.root / "repo")
        self.assertEqual(
            self.checker.problems(repo),
            [
                "package-release.sh ships ['licenses/a.txt', 'licenses/b.txt'], "
                "the archive smoke test expects ['licenses/a.txt']"
            ],
        )

    def test_a_shipped_notice_must_exist_and_be_attributed(self) -> None:
        cases = (
            (("licenses/a.txt",), LINKED, ["licenses/b.txt is missing"]),
            (LINKED, ("licenses/b.txt",), ["ATTRIBUTIONS.md does not name licenses/a.txt"]),
            (
                (),
                (),
                [
                    "licenses/a.txt is missing",
                    "ATTRIBUTIONS.md does not name licenses/a.txt",
                    "licenses/b.txt is missing",
                    "ATTRIBUTIONS.md does not name licenses/b.txt",
                ],
            ),
        )
        for index, (present, attributed, expected) in enumerate(cases):
            with self.subTest(present=present, attributed=attributed):
                repo = Repo(present=present, attributed=attributed).write(self.root / f"repo-{index}")
                self.assertEqual(self.checker.problems(repo), expected)

    def test_a_built_library_attributions_does_not_name_is_stale(self) -> None:
        stale = "build-libheif.sh builds libheif 1.23.7, ATTRIBUTIONS.md does not name it so"
        cases = (
            ({"libheif": "1.23.6", "libde265": "1.1.3"}, [stale]),
            ({"libheif": "1.23.70", "libde265": "1.1.3"}, [stale]),
            ({"xberg-libheif": "1.23.7", "libde265": "1.1.3"}, [stale]),
            ({"libheif": "1.23.7.", "libde265": "1.1.3"}, []),
        )
        for index, (named, expected) in enumerate(cases):
            with self.subTest(named=named):
                repo = Repo(built_named=named).write(self.root / f"repo-{index}")
                self.assertEqual(self.checker.problems(repo), expected)

    def test_a_built_library_notice_must_match_its_build(self) -> None:
        version, sha256 = BUILT["libheif"]
        cases = (
            (
                built_notice("libheif", "1.23.6", sha256, COMMIT),
                COMMIT,
                ['licenses/libheif.txt does not open with "libheif 1.23.7:"'],
            ),
            (
                built_notice("libheif", version, "d" * 64, COMMIT),
                COMMIT,
                [f"licenses/libheif.txt does not name the libheif 1.23.7 tarball's sha256 {sha256}"],
            ),
            (
                built_notice("libheif", version, sha256, COMMIT),
                "e" * 40,
                [f"licenses/{library}.txt does not name vcpkg commit {'e' * 40}" for library in BUILT],
            ),
        )
        for index, (notice, vcpkg, expected) in enumerate(cases):
            with self.subTest(expected=expected):
                notices = {**default_built_notices(), "libheif": notice}
                repo = Repo(built_notices=notices, vcpkg=vcpkg).write(self.root / f"repo-{index}")
                self.assertEqual(self.checker.problems(repo), expected)

    def test_a_missing_built_library_notice_fails_every_check(self) -> None:
        notices = {"libheif": default_built_notices()["libheif"]}
        repo = Repo(built_notices=notices).write(self.root / "repo")
        self.assertEqual(
            self.checker.problems(repo),
            [
                'licenses/libde265.txt does not open with "libde265 1.1.3:"',
                f"licenses/libde265.txt does not name the libde265 1.1.3 tarball's sha256 {'b' * 64}",
                f"licenses/libde265.txt does not name vcpkg commit {COMMIT}",
            ],
        )

    def test_built_pins_pairs_each_version_with_its_sha256(self) -> None:
        script = "LIBDE265_VERSION=1.1.3\nLIBDE265_SHA256=abc\nLIBHEIF_VERSION=1.23.7\nLIBHEIF_SHA256=def\njobs=4\n"
        self.assertEqual(self.checker.built_pins(script), {"libde265": ("1.1.3", "abc"), "libheif": ("1.23.7", "def")})

    def test_the_real_build_script_pins_libheif_and_libde265(self) -> None:
        pins = self.checker.built_pins((ROOT / "scripts/build-libheif.sh").read_text(encoding="utf-8"))
        self.assertEqual(sorted(pins), ["libde265", "libheif"])

    def test_an_action_without_the_vcpkg_commit_is_an_error(self) -> None:
        with self.assertRaisesRegex(ValueError, "^install-libheif pins no BASEMIND_VCPKG_COMMIT$"):
            self.checker.vcpkg_commit("BASEMIND_VCPKG_COMMIT=main\n")

    def test_a_package_script_without_the_array_is_an_error(self) -> None:
        with self.assertRaisesRegex(ValueError, "^package-release.sh defines no LINKED_NOTICES array$"):
            self.checker.shell_notices('cp LICENSE "$STAGING_DIR/"\n')

    def test_the_real_package_script_and_smoke_test_ship_the_same_notices(self) -> None:
        shipped = self.checker.shell_notices((ROOT / "scripts/package-release.sh").read_text(encoding="utf-8"))
        self.assertEqual(
            shipped,
            (
                "licenses/onnxruntime.txt",
                "licenses/onnxruntime-ThirdPartyNotices.txt",
                "licenses/tesseract.txt",
                "licenses/leptonica.txt",
            ),
        )
        self.assertEqual(self.checker.smoke_notices(ROOT), shipped)


class MainTests(unittest.TestCase):
    def setUp(self) -> None:
        self.checker = load_checker()
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)

    def run_main(self, root: Path) -> tuple[int, str, str]:
        self.checker.ROOT = root
        stdout, stderr = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            code = self.checker.main()
        return code, stdout.getvalue(), stderr.getvalue()

    def test_current_notices_pass(self) -> None:
        code, stdout, stderr = self.run_main(Repo().write(Path(self.tmp.name) / "repo"))
        self.assertEqual(
            (code, stdout, stderr), (0, "✓ Native notices match Cargo.lock and the release packaging\n", "")
        )

    def test_stale_notices_fail_with_every_problem(self) -> None:
        repo = Repo(named={**VERSIONS, "ort-sys": "1.0.0"}, present=("licenses/a.txt",)).write(
            Path(self.tmp.name) / "repo"
        )
        code, stdout, stderr = self.run_main(repo)
        self.assertEqual(code, 1)
        self.assertEqual(stdout, "")
        self.assertEqual(
            stderr,
            "error: the native-code notices are stale or missing:\n"
            "  Cargo.lock has ort-sys 2.0.0, ATTRIBUTIONS.md does not name `ort-sys` 2.0.0\n"
            "  licenses/b.txt is missing\n"
            "Re-check each entry against the upstream source, then update ATTRIBUTIONS.md and licenses/.\n",
        )


if __name__ == "__main__":
    unittest.main()
