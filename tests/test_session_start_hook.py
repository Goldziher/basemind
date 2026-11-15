"""Exercise SessionStart with local command fixtures."""

import json
import os
import re
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
HOOK = ROOT / "plugin/.ai-rulez/hooks/session-start"

VERB_TOOLS = (
    "code_outline",
    "code_symbols",
    "code_read",
    "code_grep",
    "code_search",
    "code_refs",
    "find_files",
    "docs_search",
    "git_log",
    "git_blame",
    "git_diff",
    "memory_save",
    "memory_search",
    "index_status",
    "index_rescan",
)
# Still mode-dispatched, and advertised only by a build with agent coordination.
COORDINATION_TOOLS = ("agents",)
# A routed call: a tool name followed by its JSON arguments.
ROUTED_CALL = re.compile(r"\b([a-z]+_[a-z]+) (\{[^{}]*\})")
# The surface ADR-0018 removed: the mode-dispatched `code` / `git` / `memory` / `admin` tools.
REMOVED_SPELLING = re.compile(
    r"\b(?:code|git|memory|admin) (?:tool|modes?)\b|\b(?:code|git|memory|admin) *\{|mode=|__(?:code|git|memory|admin)\b"
)


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
        self.assertIn("always loaded, so call them directly with no tool search", result)
        self.assertIn(", ".join((*VERB_TOOLS, *COORDINATION_TOOLS)), result)
        self.assertIn("mcp__plugin_basemind_basemind__<tool>", result)
        self.assertNotIn("ToolSearch", result)
        self.assertIn('code_grep {"pattern":"cache_root("}', result)
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
        self.assertNotIn("always loaded", empty)
        failed = self.invoke("", exit_code=1)
        self.assertIn("launcher could not run `basemind status` (exit 1", failed)
        unreadable = self.invoke("not JSON")
        self.assertIn("unreadable status", unreadable)

    def test_tool_names_are_exactly_the_advertised_surface(self) -> None:
        declared = re.search(r'^TOOL_NAMES="([^"]*)"$', HOOK.read_text(), re.MULTILINE)
        self.assertIsNotNone(declared, "the hook must declare TOOL_NAMES")
        self.assertEqual(declared.group(1).split(), [*VERB_TOOLS, *COORDINATION_TOOLS])

    def test_routing_names_only_verb_tools(self) -> None:
        result = self.invoke('{"file_count":631,"schema_version":33}')
        calls = ROUTED_CALL.findall(result)
        self.assertGreaterEqual(len(calls), 12, result)
        for name, arguments in calls:
            self.assertIn(name, VERB_TOOLS, result)
            self.assertIsInstance(json.loads(arguments), dict)
        self.assertIsNone(REMOVED_SPELLING.search(result), result)

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
