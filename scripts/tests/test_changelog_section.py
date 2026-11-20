"""A release's notes are exactly its CHANGELOG section, and a release without one is refused."""

import contextlib
import importlib.util
import io
import tempfile
import unittest
from pathlib import Path
from types import ModuleType

CHANGELOG = """# Changelog

<!-- markdownlint-disable MD024 -->

## [Unreleased]

### Added

- Something not yet released.

## [0.34.0] - 2026-11-20

A minor release.

### Added

- `code impact` ([#121]).

[#121]: https://github.com/Goldziher/basemind/issues/121

## [0.34.0-rc.1] - 2026-11-18

### Fixed

- A release candidate fix.

## [0.3.4] - 2026-01-02

## [0.1.0] - 2026-01-01

### Added

- First release.

[0.34.0]: https://github.com/Goldziher/basemind/compare/v0.33.0...v0.34.0
[0.1.0]: https://github.com/Goldziher/basemind/releases/tag/v0.1.0
"""


def load_script() -> ModuleType:
    """Load scripts/changelog_section.py as a module."""
    root = Path(__file__).resolve().parents[2]
    spec = importlib.util.spec_from_file_location("changelog_section", root / "scripts/changelog_section.py")
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class SectionTests(unittest.TestCase):
    """The section runs from below its heading to the next heading or the comparison links."""

    def setUp(self) -> None:
        self.module = load_script()

    def test_a_middle_section_stops_at_the_next_heading_and_keeps_its_link_definitions(self) -> None:
        self.assertEqual(
            self.module.section(CHANGELOG, "0.34.0"),
            "A minor release.\n\n### Added\n\n- `code impact` ([#121]).\n\n"
            "[#121]: https://github.com/Goldziher/basemind/issues/121\n",
        )

    def test_a_release_candidate_is_not_confused_with_its_final_version(self) -> None:
        self.assertEqual(self.module.section(CHANGELOG, "0.34.0-rc.1"), "### Fixed\n\n- A release candidate fix.\n")

    def test_the_last_section_stops_at_the_comparison_links(self) -> None:
        self.assertEqual(self.module.section(CHANGELOG, "0.1.0"), "### Added\n\n- First release.\n")

    def test_a_version_without_a_section_is_refused(self) -> None:
        with self.assertRaisesRegex(self.module.SectionError, r"^CHANGELOG.md has no section for 0\.35\.0$"):
            self.module.section(CHANGELOG, "0.35.0")

    def test_a_version_prefix_does_not_match_a_longer_version(self) -> None:
        with self.assertRaisesRegex(self.module.SectionError, r"^CHANGELOG.md has no section for 0\.3\.0$"):
            self.module.section(CHANGELOG, "0.3.0")

    def test_an_empty_section_is_refused(self) -> None:
        with self.assertRaisesRegex(self.module.SectionError, r"^CHANGELOG.md section for 0\.3\.4 is empty$"):
            self.module.section(CHANGELOG, "0.3.4")


class MainTests(unittest.TestCase):
    """The command line prints the notes, or an error annotation and a failing exit code."""

    def setUp(self) -> None:
        self.module = load_script()
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.changelog = Path(directory.name) / "CHANGELOG.md"
        self.changelog.write_text(CHANGELOG, encoding="utf-8")

    def run_main(self, argv: list[str]) -> tuple[int, str, str]:
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = self.module.main(argv)
        return code, out.getvalue(), err.getvalue()

    def test_prints_the_section_of_the_named_changelog(self) -> None:
        self.assertEqual(
            self.run_main(["0.1.0", str(self.changelog)]),
            (0, "### Added\n\n- First release.\n", ""),
        )

    def test_a_missing_section_fails_with_an_error_annotation_and_no_notes(self) -> None:
        self.assertEqual(
            self.run_main(["0.35.0", str(self.changelog)]),
            (1, "", "::error::CHANGELOG.md has no section for 0.35.0\n"),
        )

    def test_a_malformed_version_is_a_usage_error(self) -> None:
        code, out, err = self.run_main(["v0.1.0", str(self.changelog)])
        self.assertEqual((code, out), (2, ""))
        self.assertIn("usage: changelog_section.py", err)

    def test_defaults_to_the_repository_changelog(self) -> None:
        code, out, err = self.run_main(["0.33.0"])
        self.assertEqual((code, err), (0, ""))
        self.assertTrue(out.startswith("This minor bumps the persisted blob"), out[:80])
        self.assertNotIn("## [0.32.0]", out)


if __name__ == "__main__":
    unittest.main()
