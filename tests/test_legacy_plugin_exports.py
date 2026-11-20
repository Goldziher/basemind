"""Public compatibility exports must remain complete and reject drift."""

import importlib.util
import json
import shutil
import subprocess
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


class GeminiContextFileTests(unittest.TestCase):
    def test_every_gemini_manifest_names_a_context_file_the_repository_ships(self):
        for manifest in ("gemini-extension.json", "plugin/gemini-extension.json"):
            name = json.loads((ROOT / manifest).read_text())["contextFileName"]
            shipped = (Path(manifest).parent / name).as_posix()
            tracked = subprocess.run(
                ["git", "ls-files", "--error-unmatch", shipped], cwd=ROOT, capture_output=True, check=False
            )
            self.assertEqual(tracked.returncode, 0, f"{manifest} names {shipped}, which is not tracked")


class StatusLineShippingTests(unittest.TestCase):
    """`/bm-statusline` resolves the installed plugin's `scripts/statusline.sh`."""

    def test_the_status_line_ships_in_the_plugin_and_the_marketplace_copy_follows_it(self) -> None:
        source = ROOT / "plugin/.ai-rulez/scripts/statusline.sh"
        self.assertEqual(EXPORTS.expected_files(ROOT)[".claude-plugin/statusline.sh"], source)
        shipped = ROOT / "plugin/scripts/statusline.sh"
        self.assertEqual(shipped.read_bytes(), source.read_bytes())
        self.assertIn("/scripts/statusline.sh", (ROOT / "plugin/commands/bm-statusline.md").read_text())
        self.assertNotIn("*/.claude-plugin/statusline.sh", (ROOT / "plugin/commands/bm-statusline.md").read_text())

    def test_the_installed_script_names_its_plugin_version_after_the_brand_mark(self) -> None:
        if shutil.which("jq") is None:
            self.skipTest("jq is required to read the manifest")
        mark = "\033[38;2;249;115;22m\u25c6\033[0m \033[1m\033[38;2;249;115;22mbasemind\033[0m"
        version = "  \033[1m\033[38;5;244mv7.8.9\033[0m  \033[38;5;240m\u2502\033[0m"
        cases = {
            "full": ({"BASEMIND_STATUSLINE": "full"}, f"{mark}{version}  3 files"),
            "compact": ({"BASEMIND_STATUSLINE": "compact"}, f"{mark}{version}  3 files"),
            "minimal": ({"BASEMIND_STATUSLINE": "minimal"}, f"{mark}  3 files"),
            "hidden": ({"BASEMIND_STATUSLINE": "full", "BASEMIND_STATUSLINE_VERSION": "0"}, f"{mark}  3 files"),
        }
        with tempfile.TemporaryDirectory() as temporary:
            plugin = Path(temporary) / "plugin"
            (plugin / ".claude-plugin").mkdir(parents=True)
            (plugin / ".claude-plugin/plugin.json").write_text('{"version": "7.8.9"}')
            (plugin / "scripts").mkdir()
            script = plugin / "scripts/statusline.sh"
            script.write_bytes((ROOT / "plugin/.ai-rulez/scripts/statusline.sh").read_bytes())
            stub = Path(temporary) / "bin"
            stub.mkdir()
            (stub / "basemind").write_text(f"#!/bin/bash\n[ \"$1\" = statusline ] && printf '%s\\n' '{mark}  3 files'\n")
            (stub / "basemind").chmod(0o755)
            work = Path(temporary) / "work"
            work.mkdir()
            payload = json.dumps({"workspace": {"current_dir": str(work)}})
            path = f"{stub}:/usr/bin:/bin:{Path(shutil.which('jq')).parent}"
            for case, (extra, expected) in cases.items():
                with self.subTest(case=case):
                    out = subprocess.run(
                        ["/bin/bash", str(script)],
                        input=payload,
                        env={"PATH": path, "COLUMNS": "200", **extra},
                        capture_output=True,
                        text=True,
                        timeout=60,
                        check=True,
                    ).stdout
                    self.assertEqual(out, expected)
