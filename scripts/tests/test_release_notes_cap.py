"""The release-notes cap of publish.yaml keeps every real CHANGELOG section under GitHub's body limit."""

import contextlib
import importlib.util
import io
import os
import re
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
BODY_LIMIT = 125000
HEREDOC = re.compile(r"python3 - \"\$RUNNER_TEMP/release-notes\.md\" <<'PY'\n(?P<code>.*?)\n\s*PY\n", re.DOTALL)


def load_changelog_section():
    spec = importlib.util.spec_from_file_location("changelog_section", ROOT / "scripts" / "changelog_section.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def cap_script() -> str:
    """The inline script of the cap step, exactly as the workflow runs it."""
    workflow = (ROOT / ".github" / "workflows" / "publish.yaml").read_text(encoding="utf-8")
    step = workflow[workflow.index("- name: Cap the release notes at GitHub's body limit") :]
    found = HEREDOC.search(step)
    assert found, "the cap step no longer has an inline `python3 - ... <<'PY'` script"
    return textwrap.dedent(found.group("code"))


def capped(text: str, tmp: Path, version: str = "9.9.9") -> str:
    notes = tmp / "release-notes.md"
    notes.write_text(text, encoding="utf-8")
    env = {"VERSION": version, "REPO": "owner/repo"}
    with mock.patch.object(sys, "argv", ["-", str(notes)]), mock.patch.dict(os.environ, env):
        with contextlib.redirect_stdout(io.StringIO()):
            exec(compile(cap_script(), "publish.yaml:cap", "exec"), {})
    return notes.read_text(encoding="utf-8")


class ReleaseNotesCapTests(unittest.TestCase):
    def setUp(self) -> None:
        holder = tempfile.TemporaryDirectory()
        self.addCleanup(holder.cleanup)
        self.tmp = Path(holder.name)

    def test_a_short_section_is_left_alone(self) -> None:
        self.assertEqual(capped("### Added\n\n- one\n", self.tmp), "### Added\n\n- one\n")

    def test_an_oversize_section_is_cut_at_a_line_boundary_with_a_link_to_the_changelog(self) -> None:
        line = "- " + "x" * 98 + "\n"
        result = capped(line * 2000, self.tmp)
        self.assertLessEqual(len(result), BODY_LIMIT)
        body, _, tail = result.partition("\n\n_The notes are cut")
        self.assertEqual(set(body.split("\n")), {line.strip()})
        self.assertIn("https://github.com/owner/repo/blob/v9.9.9/CHANGELOG.md", tail)

    def test_every_real_changelog_section_fits_a_github_release_body(self) -> None:
        module = load_changelog_section()
        changelog = (ROOT / "CHANGELOG.md").read_text(encoding="utf-8")
        versions = re.findall(r"^## \[(\d+\.\d+\.\d+(?:-rc\.\d+)?)\]", changelog, re.MULTILINE)
        self.assertGreater(len(versions), 10)
        for version in versions:
            with self.subTest(version=version):
                try:
                    notes = module.section(changelog, version)
                except module.SectionError:
                    continue
                self.assertLessEqual(len(capped(notes, self.tmp, version)), BODY_LIMIT)


if __name__ == "__main__":
    unittest.main()
