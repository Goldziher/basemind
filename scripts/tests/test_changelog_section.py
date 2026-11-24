"""A release's notes are exactly its CHANGELOG section, and a release without one is refused."""

import contextlib
import importlib.util
import io
import os
import tempfile
import unittest
import unittest.mock
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


class CeilingTests(unittest.TestCase):
    """An oversized section needs a curated summary; a curated summary replaces the section."""

    def setUp(self) -> None:
        self.module = load_script()
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.changelog = self.root / "CHANGELOG.md"
        self.changelog.write_text(CHANGELOG, encoding="utf-8")

    def run_main(self, argv: list[str], ceiling: str | None) -> tuple[int, str, str]:
        env = {} if ceiling is None else {self.module.CEILING_ENV: ceiling}
        out, err = io.StringIO(), io.StringIO()
        with (
            unittest.mock.patch.dict(os.environ, env, clear=False),
            contextlib.redirect_stdout(out),
            contextlib.redirect_stderr(err),
        ):
            if ceiling is None:
                os.environ.pop(self.module.CEILING_ENV, None)
            code = self.module.main(argv)
        return code, out.getvalue(), err.getvalue()

    def write_curated(self, version: str, text: str) -> None:
        directory = self.root / "docs" / "release-notes"
        directory.mkdir(parents=True, exist_ok=True)
        (directory / f"{version}.md").write_text(text, encoding="utf-8")

    def test_a_section_over_the_ceiling_is_refused_with_the_curation_advice(self) -> None:
        size = len(self.module.section(CHANGELOG, "0.34.0"))
        code, out, err = self.run_main(["0.34.0", str(self.changelog)], str(size - 1))
        self.assertEqual((code, out), (1, ""))
        self.assertEqual(
            err,
            f"::error::CHANGELOG.md section for 0.34.0 is {size} characters, over the {size - 1} ceiling: "
            "curate the release summary by hand in docs/release-notes/0.34.0.md "
            "and keep the long list in CHANGELOG.md\n",
        )

    def test_a_section_at_the_ceiling_is_printed(self) -> None:
        size = len(self.module.section(CHANGELOG, "0.1.0"))
        self.assertEqual(
            self.run_main(["0.1.0", str(self.changelog)], str(size)),
            (0, "### Added\n\n- First release.\n", ""),
        )

    def test_a_curated_summary_replaces_an_oversized_section(self) -> None:
        self.write_curated("0.34.0", "\nHeadlines.\n\n")
        self.assertEqual(self.run_main(["0.34.0", str(self.changelog)], "10"), (0, "Headlines.\n", ""))

    def test_an_empty_curated_summary_is_refused(self) -> None:
        self.write_curated("0.34.0", "\n")
        self.assertEqual(
            self.run_main(["0.34.0", str(self.changelog)], None),
            (1, "", "::error::0.34.0.md is empty\n"),
        )

    def test_the_default_ceiling_is_sixty_thousand_characters(self) -> None:
        self.assertEqual(self.module.DEFAULT_CEILING, 60_000)
        long_changelog = "## [1.0.0]\n\n" + "- x\n" * 20_000 + "\n## [0.9.0]\n"
        self.changelog.write_text(long_changelog, encoding="utf-8")
        code, _, err = self.run_main(["1.0.0", str(self.changelog)], None)
        self.assertEqual(code, 1)
        self.assertIn("over the 60000 ceiling", err)

    def test_a_bad_ceiling_is_refused(self) -> None:
        for raw in ("abc", "0", "-5"):
            with self.subTest(raw=raw):
                self.assertEqual(
                    self.run_main(["0.1.0", str(self.changelog)], raw),
                    (1, "", f"::error::RELEASE_NOTES_CEILING must be a positive integer, got {raw!r}\n"),
                )


if __name__ == "__main__":
    unittest.main()
