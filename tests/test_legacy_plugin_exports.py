"""Public compatibility exports must remain complete and reject drift."""

import importlib.util
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location("legacy_exports", ROOT / "scripts/sync_legacy_plugin_exports.py")
EXPORTS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(EXPORTS)


class LegacyExportsTests(unittest.TestCase):
    def test_changed_bytes_fail_check_then_sync_repairs(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source"
            target = root / "target"
            source.write_bytes(b"canonical")
            target.write_bytes(b"stale")
            self.assertFalse(EXPORTS.sync_files(root, {"target": source}, check=True))
            self.assertTrue(EXPORTS.sync_files(root, {"target": source}, check=False))
            self.assertEqual(target.read_bytes(), b"canonical")

    def test_symlink_destination_cannot_overwrite_external_file(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source"
            outside = root / "outside"
            source.write_bytes(b"canonical")
            outside.write_bytes(b"preserved")
            (root / "target").symlink_to(outside)
            with self.assertRaises(ValueError):
                EXPORTS.sync_files(root, {"target": source}, check=False)
            self.assertEqual(outside.read_bytes(), b"preserved")

    def test_every_hook_script_is_exported_to_the_root_hooks_directory(self) -> None:
        files = EXPORTS.expected_files(ROOT)
        for source in (ROOT / "plugin/.ai-rulez/hooks").iterdir():
            self.assertEqual(files[f"hooks/{source.name}"], source)
        self.assertIn("hooks/agent-as", files)

    def test_the_launcher_and_the_notifying_hooks_reach_every_harness_tree(self) -> None:
        files = EXPORTS.expected_files(ROOT)
        self.assertEqual(files["scripts/mcp-launch.sh"], ROOT / "plugin/.ai-rulez/scripts/mcp-launch.sh")
        for tree in (".codex-plugin", ".cursor-plugin"):
            for name in ("session-start", "inbox-notify", "run-hook.cmd"):
                self.assertIn(f"{tree}/hooks/{name}", files)
        self.assertIn(".codex-plugin/hooks/agent-as", files)
        self.assertNotIn(".cursor-plugin/hooks/agent-as", files, "Cursor applies no input agent-as rewrites")
