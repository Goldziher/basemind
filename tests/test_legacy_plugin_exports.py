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
