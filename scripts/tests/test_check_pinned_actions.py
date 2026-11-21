"""A publish workflow, or a local action it calls, that runs an action by tag fails the check."""

import contextlib
import importlib.util
import io
import tempfile
import unittest
from pathlib import Path
from types import ModuleType

SHA = "3d3c42e5aac5ba805825da76410c181273ba90b1"


def load_checker() -> ModuleType:
    """Load scripts/check_pinned_actions.py as a module."""
    root = Path(__file__).resolve().parents[2]
    spec = importlib.util.spec_from_file_location("check_pinned_actions", root / "scripts/check_pinned_actions.py")
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class PinnedActionsTests(unittest.TestCase):
    def setUp(self) -> None:
        self.module = load_checker()
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)

    def write(self, rel: str, body: str) -> None:
        path = self.root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(body, encoding="utf-8")

    def check(self, body: str, action: str = "") -> list[str]:
        self.write(".github/workflows/publish.yaml", body)
        if action:
            self.write(".github/actions/setup/action.yml", action)
        return self.module.problems(self.root, [".github/workflows/publish.yaml"])

    def test_sha_pins_with_their_tag_and_local_actions_pass(self) -> None:
        body = (
            "jobs:\n  a:\n    steps:\n"
            f"      - uses: actions/checkout@{SHA} # v7.0.1\n"
            f"      - uses: xberg-io/actions/setup-mise@{SHA} # v1.51.0\n"
            "      - uses: ./.github/actions/setup\n"
        )
        action = f"runs:\n  steps:\n    - uses: dtolnay/rust-toolchain@{SHA} # v1\n"
        self.assertEqual(self.check(body, action), [])

    def test_a_tag_a_short_sha_and_a_missing_tag_comment_are_reported(self) -> None:
        body = (
            "jobs:\n  a:\n    steps:\n"
            "      - uses: actions/checkout@v7\n"
            "      - uses: actions/checkout@3d3c42e # v7.0.1\n"
            f"      - uses: actions/checkout@{SHA}\n"
            f"    uses: xberg-io/actions/.github/workflows/reusable.yml@{SHA} # main\n"
        )
        self.assertEqual(
            self.check(body),
            [
                ".github/workflows/publish.yaml:4: actions/checkout@v7 is not pinned to a full commit SHA",
                ".github/workflows/publish.yaml:5: actions/checkout@3d3c42e is not pinned to a full commit SHA",
                ".github/workflows/publish.yaml:6: "
                f"actions/checkout@{SHA} needs the tag it pins in a comment (# v1.2.3)",
                ".github/workflows/publish.yaml:7: "
                f"xberg-io/actions/.github/workflows/reusable.yml@{SHA} needs the tag it pins in a comment (# v1.2.3)",
            ],
        )

    def test_quoted_pins_in_every_mapping_form_pass(self) -> None:
        body = (
            "jobs:\n  a:\n    steps:\n"
            f'      - uses: "actions/checkout@{SHA}" # v4\n'
            f"      - {{uses: 'actions/cache@{SHA}', with: {{key: a}}}} # v4.2.0\n"
            f'      - "uses": actions/setup-node@{SHA} # v5\n'
            "      # - uses: actions/checkout@v4\n"
        )
        self.assertEqual(self.check(body), [])

    def test_tags_in_flow_mappings_and_quoted_keys_are_reported(self) -> None:
        body = (
            "jobs:\n  a:\n    steps:\n"
            "      - {uses: actions/checkout@v7}\n"
            "      - {name: c, uses: actions/cache@v4} # v4\n"
            '      - "uses": actions/setup-node@v5\n'
            "      - 'uses': 'actions/upload-artifact@main'\n"
        )
        self.assertEqual(
            self.check(body),
            [
                ".github/workflows/publish.yaml:4: actions/checkout@v7 is not pinned to a full commit SHA",
                ".github/workflows/publish.yaml:5: actions/cache@v4 is not pinned to a full commit SHA",
                ".github/workflows/publish.yaml:6: actions/setup-node@v5 is not pinned to a full commit SHA",
                ".github/workflows/publish.yaml:7: actions/upload-artifact@main is not pinned to a full commit SHA",
            ],
        )

    def test_uses_text_in_comments_and_scalars_is_not_a_reference(self) -> None:
        body = (
            "jobs:\n  a:\n    steps:\n"
            "      - name: Checkout # uses: actions/checkout@v4\n"
            '      - run: echo "see uses: actions/cache@v4"\n'
            "      - {name: 'a, uses: actions/cache@v4', run: b}\n"
            "      - run: |\n"
            "          uses: actions/setup-node@v5\n"
            "\n"
            "          echo uses: actions/setup-node@v5\n"
            "        shell: bash\n"
            "      - uses: actions/checkout@v7 # uses: actions/checkout@v4\n"
        )
        self.assertEqual(
            self.check(body),
            [".github/workflows/publish.yaml:12: actions/checkout@v7 is not pinned to a full commit SHA"],
        )

    def test_a_reference_after_a_block_scalar_is_still_checked(self) -> None:
        body = (
            "jobs:\n  a:\n    steps:\n"
            "      - run: >-\n          echo a\n"
            "      - uses: actions/checkout@v7\n"
            "      - |\n        uses: actions/cache@v4\n"
            "      - uses: actions/cache@v4\n"
        )
        self.assertEqual(
            self.check(body),
            [
                ".github/workflows/publish.yaml:6: actions/checkout@v7 is not pinned to a full commit SHA",
                ".github/workflows/publish.yaml:9: actions/cache@v4 is not pinned to a full commit SHA",
            ],
        )

    def test_a_tag_inside_a_called_local_action_is_reported(self) -> None:
        body = "jobs:\n  a:\n    steps:\n      - uses: ./.github/actions/setup\n"
        action = "runs:\n  steps:\n    - uses: Swatinem/rust-cache@v2\n"
        self.assertEqual(
            self.check(body, action),
            [".github/actions/setup/action.yml:3: Swatinem/rust-cache@v2 is not pinned to a full commit SHA"],
        )

    def test_main_fails_on_an_unpinned_action_and_annotates_it(self) -> None:
        self.write(".github/workflows/publish.yaml", "jobs:\n  a:\n    steps:\n      - uses: actions/checkout@v7\n")
        self.module.ROOT = self.root
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = self.module.main([".github/workflows/publish.yaml"])
        self.assertEqual(code, 1)
        self.assertEqual(
            out.getvalue(),
            "::error::.github/workflows/publish.yaml:4: actions/checkout@v7 is not pinned to a full commit SHA\n",
        )

    def test_the_repository_publish_workflows_are_pinned(self) -> None:
        self.assertEqual(self.module.problems(self.module.ROOT, self.module.PUBLISH_WORKFLOWS), [])


if __name__ == "__main__":
    unittest.main()
