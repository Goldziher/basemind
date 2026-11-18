"""A crate crates.io has never seen stops the release before anything publishes."""

import contextlib
import importlib.util
import io
import unittest
from pathlib import Path
from types import ModuleType


def load_checker() -> ModuleType:
    """Load scripts/check_vendored_crates.py as a module."""
    root = Path(__file__).resolve().parents[2]
    spec = importlib.util.spec_from_file_location("check_vendored_crates", root / "scripts/check_vendored_crates.py")
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class NewCrateTests(unittest.TestCase):
    """Every vendored crate is unpublished at its version; only basemind-agents is unknown to crates.io."""

    def setUp(self) -> None:
        self.module = load_checker()
        self.module.CRATES = ["stack-graphs", "basemind-agents"]
        names = {"stack-graphs": "basemind-stack-graphs", "basemind-agents": "basemind-agents"}
        self.module.drift = lambda crate_dir: (names[crate_dir], "0.34.0", None)
        self.module.crate_exists = lambda name: name != "basemind-agents"

    def run_main(self, argv: list[str]) -> tuple[int, str]:
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = self.module.main(argv)
        return code, out.getvalue()

    def test_release_refuses_a_crate_crates_io_does_not_have(self) -> None:
        code, out = self.run_main(["--release"])
        self.assertEqual(code, 1)
        self.assertIn("::error::basemind-agents: crates.io has no such crate", out)
        self.assertIn("basemind-stack-graphs 0.34.0: not on crates.io yet", out)

    def test_ci_only_warns_about_a_crate_crates_io_does_not_have(self) -> None:
        code, out = self.run_main([])
        self.assertEqual(code, 0)
        self.assertIn("::warning::basemind-agents: crates.io has no such crate", out)

    def test_an_existing_crate_at_a_new_version_passes_the_release(self) -> None:
        self.module.crate_exists = lambda _name: True
        code, out = self.run_main(["--release"])
        self.assertEqual(code, 0)
        self.assertEqual(out.count("not on crates.io yet; the release publishes it"), 2)


if __name__ == "__main__":
    unittest.main()
