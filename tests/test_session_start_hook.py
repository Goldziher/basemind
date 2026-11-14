"""Exercise SessionStart with local command fixtures."""

import json
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
HOOK = ROOT / "plugin/.ai-rulez/hooks/session-start"


class SessionStartTests(unittest.TestCase):
    def setUp(self) -> None:
        jq = shutil.which("jq")
        if jq is None:
            self.skipTest("jq is required")
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        for command in ("jq", "dirname", "sed", "grep", "wc", "tr"):
            (self.bin / command).symlink_to(shutil.which(command))
        hooks = self.root / "hooks"
        hooks.mkdir()
        self.hook = hooks / "session-start"
        shutil.copyfile(HOOK, self.hook)
        scripts = self.root / "scripts"
        scripts.mkdir()
        self.executable(
            scripts / "mcp-launch.sh",
            """case "$1" in
agents) printf '%s' "$INBOX_RESULT" ;;
status)
  printf '%s\\n' "$@" > "$HOME/status-args"
  printf '%s' "$STATUS_RESULT"
  exit "${STATUS_EXIT:-0}"
  ;;
esac""",
        )

    @staticmethod
    def executable(path: Path, body: str) -> None:
        path.write_text(f"#!/bin/bash\n{body}\n")
        path.chmod(0o755)

    def invoke(self, response: str, *, inbox: str = "", exit_code: int = 0) -> str:
        env = {
            **os.environ,
            "HOME": str(self.root),
            "PATH": str(self.bin),
            "CLAUDE_PLUGIN_ROOT": str(self.root),
            "CLAUDE_PROJECT_DIR": "/some/project with space & ampersand",
            "STATUS_RESULT": response,
            "STATUS_EXIT": str(exit_code),
            "INBOX_RESULT": inbox,
        }
        env.pop("CURSOR_PLUGIN_ROOT", None)
        env.pop("COPILOT_CLI", None)
        output = subprocess.run(
            ["/bin/bash", self.hook],
            cwd=self.root,
            env=env,
            capture_output=True,
            text=True,
            timeout=15,
            check=True,
        )
        payload = json.loads(output.stdout)["hookSpecificOutput"]
        self.assertEqual(payload["hookEventName"], "SessionStart")
        return payload["additionalContext"]

    def test_indexed_workspace_is_verified_without_coreutils_timeout(self) -> None:
        result = self.invoke('{"file_count":631,"schema_version":33}')
        self.assertIn("basemind verified: 631 files indexed (schema 33).", result)
        self.assertIn("single ToolSearch call", result)
        self.assertIn("select:mcp__plugin_basemind_basemind__code,mcp__plugin_basemind_basemind__git", result)
        self.assertIn("code grep for exact matches in indexed code", result)
        self.assertIn("Jupyter notebooks", result)
        args = (self.root / "status-args").read_text().splitlines()
        self.assertEqual(
            args,
            ["status", "--root", "/some/project with space & ampersand", "--json"],
        )

    def test_failures_name_what_failed(self) -> None:
        empty = self.invoke('{"file_count":0,"schema_version":33}')
        self.assertIn("index is empty", empty)
        self.assertIn("basemind scan", empty)
        self.assertNotIn("ToolSearch", empty)
        failed = self.invoke("", exit_code=1)
        self.assertIn("launcher could not run `basemind status` (exit 1", failed)
        unreadable = self.invoke("not JSON")
        self.assertIn("unreadable status", unreadable)

    def test_timeout_is_reported_as_such(self) -> None:
        self.executable(self.bin / "timeout", "exit 124")
        self.assertIn("timed out", self.invoke(""))

    def test_malformed_inbox_leaves_startup_context_intact(self) -> None:
        self.executable(self.bin / "timeout", 'shift; exec "$@"')
        context = self.invoke('{"file_count":5,"schema_version":33}', inbox="not JSON")
        self.assertIn("basemind verified", context)
        self.assertNotIn("No messages in your inbox", context)

    def test_session_start_hook_runs_on_resume(self) -> None:
        manifest = json.loads((ROOT / "plugin/hooks/hooks.json").read_text())
        matchers = {source for entry in manifest["hooks"]["SessionStart"] for source in entry["matcher"].split("|")}
        self.assertIn("resume", matchers)


if __name__ == "__main__":
    unittest.main()
