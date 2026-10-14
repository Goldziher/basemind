"""Verify required capability registration with the installed Hermes PluginManager."""

import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
PROBE = """
import json
import os
from hermes_cli.plugins import PluginManager
from tools.registry import registry

manager = PluginManager()
manager.discover_and_load()
loaded = manager._plugins["basemind"]
assert loaded.manifest.source == "user"
if os.environ["INVALID_SKILL"] == "true" or os.environ["MISSING_SKILL"] == "true":
    assert not loaded.enabled, "Incomplete required registration must disable the plugin"
    expected_error = "invalid:skill" if os.environ["INVALID_SKILL"] == "true" else "SKILL.md"
    assert expected_error in loaded.error, loaded.error
    assert manager.list_plugin_skills("basemind") == [], "Failed plugin must not advertise partial skills"
    assert registry.get_tool_names_for_toolset("basemind") == [], "Failed plugin must not advertise MCP tools"
    if os.environ["INVALID_SKILL"] == "true":
        assert "Invalid Hermes skill name" in loaded.error, loaded.error
    print(json.dumps({"enabled": loaded.enabled, "error": loaded.error}))
else:
    assert loaded.enabled and loaded.error is None, loaded.error
    names = manager.list_plugin_skills("basemind")
    assert len(names) == 10, names
    assert len(loaded.commands_registered) == 6, loaded.commands_registered
    assert loaded.tools_registered == ["basemind_call"], loaded.tools_registered
    print(json.dumps({"skills": len(names), "commands": 6, "tools": 1}))
"""


class NativeHermesRegistrationTests(unittest.TestCase):
    """Load the actual generated plugin in independent native SDK subprocesses."""

    def probe(self, invalid_skill: bool, missing_skill: bool = False) -> dict[str, object]:
        with tempfile.TemporaryDirectory(prefix="basemind-native-registration-") as temporary:
            home = Path(temporary)
            plugin = home / "plugins/basemind"
            shutil.copytree(ROOT / "plugin/.hermes/plugins/basemind", plugin)
            if invalid_skill:
                (plugin / "skills/basemind").rename(plugin / "skills/invalid:skill")
                skill = plugin / "skills/invalid:skill/SKILL.md"
                body = skill.read_text()
                self.assertEqual(body.count("\nname: basemind\n"), 1)
                skill.write_text(body.replace("\nname: basemind\n", "\nname: invalid:skill\n"))
            if missing_skill:
                (plugin / "skills/basemind-stats/SKILL.md").unlink()
            (home / "config.yaml").write_text("plugins:\n  enabled: [basemind]\n")
            bundled = home / "empty-bundled"
            bundled.mkdir()
            environment = os.environ | {
                "HERMES_HOME": str(home),
                "HERMES_BUNDLED_PLUGINS": str(bundled),
                "HERMES_ENABLE_PROJECT_PLUGINS": "true",
                "INVALID_SKILL": str(invalid_skill).lower(),
                "MISSING_SKILL": str(missing_skill).lower(),
            }
            result = subprocess.run(
                [sys.executable, "-c", PROBE],
                cwd=home,
                env=environment,
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            return json.loads(result.stdout)

    def test_should_register_all_required_capabilities(self) -> None:
        self.assertEqual(self.probe(False), {"skills": 10, "commands": 6, "tools": 1})

    def test_should_report_invalid_required_skill_in_native_plugin_error(self) -> None:
        result = self.probe(True)
        self.assertEqual(result["enabled"], False)
        self.assertIn("invalid:skill", result["error"])

    def test_should_report_missing_required_skill_in_native_plugin_error(self) -> None:
        result = self.probe(False, missing_skill=True)
        self.assertEqual(result["enabled"], False)
        self.assertIn("SKILL.md", result["error"])


if __name__ == "__main__":
    unittest.main()
