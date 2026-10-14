"""Verify real registry archives and reject missing bytes or false release success."""

import importlib.util
import json
import re
import subprocess
import tempfile
import unittest
import zipfile
from pathlib import Path
from types import ModuleType


def load_helper(name: str) -> ModuleType:
    """Load the shipping CLI helper rather than reimplementing its checks."""
    path = Path(__file__).resolve().parents[1] / f"{name}.py"
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class RegistryArchiveTests(unittest.TestCase):
    """Build the actual wheel and npm tarball once in isolated output storage."""

    @classmethod
    def setUpClass(cls) -> None:
        """Exercise the same package builders that release CI runs."""
        temporary = tempfile.TemporaryDirectory(prefix="basemind-registry-contract-")
        cls.addClassCleanup(temporary.cleanup)
        cls.directory = Path(temporary.name)
        cls.helper = load_helper("verify_plugin_distribution")
        root = Path(__file__).resolve().parents[2]
        subprocess.run(
            ["uv", "build", "--wheel", "--out-dir", temporary.name, "plugin/.hermes/package"],
            cwd=root,
            capture_output=True,
            check=True,
        )
        packed = subprocess.run(
            ["pnpm", "--dir", "plugin", "pack", "--json", "--pack-destination", temporary.name],
            cwd=root,
            capture_output=True,
            check=True,
        )
        cls.npm = cls.directory / json.loads(packed.stdout)["filename"]
        wheels = list(cls.directory.glob("*.whl"))
        assert len(wheels) == 1
        cls.wheel = wheels[0]

    def test_should_verify_all_real_registry_runtime_resources(self) -> None:
        """Both builders include their exact generated skill, prompt and adapter bytes."""
        self.assertEqual(self.helper.verify_wheel(self.wheel), 29)
        self.assertEqual(self.helper.verify_npm(self.npm), 27)

    def test_should_reject_a_wheel_missing_a_public_skill(self) -> None:
        """A valid package identity cannot mask a dropped runtime resource."""
        target = self.directory / "missing-skill.whl"
        with zipfile.ZipFile(self.wheel) as source, zipfile.ZipFile(target, "w") as output:
            missing = next(name for name in source.namelist() if name.endswith("/SKILL.md"))
            for name in source.namelist():
                if name != missing:
                    output.writestr(name, source.read(name))
        with self.assertRaisesRegex(ValueError, "Missing bundled resource"):
            self.helper.verify_wheel(target)

    def test_should_reject_a_wheel_carrying_the_wrong_version(self) -> None:
        """Registry idempotency must use the actual wheel metadata identity."""
        target = self.directory / "wrong-version.whl"
        with zipfile.ZipFile(self.wheel) as source, zipfile.ZipFile(target, "w") as output:
            for name in source.namelist():
                data = source.read(name)
                if name.endswith(".dist-info/METADATA"):
                    data = re.sub(rb"(?m)^Version: .*", b"Version: 0.0.0", data)
                output.writestr(name, data)
        with self.assertRaisesRegex(ValueError, "package name or version mismatch"):
            self.helper.verify_wheel(target)


class PluginReleaseResultTests(unittest.TestCase):
    """Every required publication must run and succeed before the report goes green."""

    def setUp(self) -> None:
        """Use the production verdict helper with exactly four expected jobs."""
        self.helper = load_helper("verify_plugin_release_results")
        self.results = {
            name: {"result": "success"}
            for name in ["publish_plugin_artifacts", "publish_plugin_npm", "publish_pypi_hermes", "publish_opencode"]
        }

    def test_should_accept_exactly_four_successful_jobs(self) -> None:
        """An idempotent publication succeeds after its artifact gates have run."""
        self.assertEqual(self.helper.verify_results(self.results), 4)

    def test_should_reject_skipped_required_publication(self) -> None:
        """A skipped registry job cannot count as successful distribution."""
        self.results["publish_plugin_npm"]["result"] = "skipped"
        with self.assertRaisesRegex(ValueError, "Incomplete plugin distribution: publish_plugin_npm"):
            self.helper.verify_results(self.results)

    def test_should_reject_failed_required_publication(self) -> None:
        """A green core release cannot hide a failed plugin wheel job."""
        self.results["publish_pypi_hermes"]["result"] = "failure"
        with self.assertRaisesRegex(ValueError, "Incomplete plugin distribution: publish_pypi_hermes"):
            self.helper.verify_results(self.results)

    def test_should_reject_a_report_that_omits_a_job(self) -> None:
        """Assert the expected count to prevent a zero-work or partial pass."""
        del self.results["publish_plugin_artifacts"]
        with self.assertRaisesRegex(ValueError, "Expected exactly four"):
            self.helper.verify_results(self.results)
