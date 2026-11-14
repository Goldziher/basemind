"""Exercise the PreToolUse guard: Bash search commands are nudged once per agent, other commands never."""

import json
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
HOOK = ROOT / "plugin/.ai-rulez/hooks/pre-tool-guard"


class PreToolGuardTests(unittest.TestCase):
    def setUp(self) -> None:
        if shutil.which("jq") is None:
            self.skipTest("jq is required")
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.tmp = temporary.name

    def run_guard(self, tool: str, tool_input: dict, **extra: str) -> str:
        payload = {"session_id": "s1", "tool_name": tool, "tool_input": tool_input, **extra}
        output = subprocess.run(
            ["/bin/bash", str(HOOK)],
            input=json.dumps(payload),
            env={**os.environ, "TMPDIR": self.tmp, "BASEMIND_GUARD": "nudge"},
            capture_output=True,
            text=True,
            timeout=15,
            check=True,
        )
        return output.stdout

    def bash(self, command: str, **extra: str) -> str:
        return self.run_guard("Bash", {"command": command}, **extra)

    def test_search_and_history_commands_name_the_replacement(self) -> None:
        cases = {
            "rg foo src/": "mode=grep",
            "cd src && grep -rn foo .": "mode=grep",
            "git blame src/lib.rs": "mode=blame_symbol",
            "git -C repo log --oneline": "mode=touching",
            "git show HEAD~1": "mode=show",
            "find . -name '*.rs'": "mode=find",
        }
        for command, expected in cases.items():
            with self.subTest(command=command):
                out = self.bash(command, agent_id=command)
                decision = json.loads(out)["hookSpecificOutput"]
                self.assertEqual(decision["permissionDecision"], "allow")
                self.assertIn(expected, decision["additionalContext"])

    def test_other_bash_commands_are_ignored(self) -> None:
        for command in ("ls -la", "cargo test | grep FAIL", "git status", "echo rg"):
            with self.subTest(command=command):
                self.assertEqual(self.bash(command), "")

    def test_nudge_fires_once_per_agent_and_kind(self) -> None:
        self.assertNotEqual(self.bash("rg a", agent_id="a1"), "")
        self.assertEqual(self.bash("grep b", agent_id="a1"), "")
        self.assertNotEqual(self.bash("rg a", agent_id="a2"), "")
        self.assertNotEqual(self.bash("git blame x", agent_id="a1"), "")

    def test_redirect_mode_never_denies_bash(self) -> None:
        env = {**os.environ, "TMPDIR": self.tmp, "BASEMIND_GUARD": "redirect"}
        payload = json.dumps({"session_id": "s", "tool_name": "Bash", "tool_input": {"command": "rg x"}})
        out = subprocess.run(
            ["/bin/bash", str(HOOK)], input=payload, env=env, capture_output=True, text=True, check=True
        ).stdout
        self.assertEqual(json.loads(out)["hookSpecificOutput"]["permissionDecision"], "allow")

    def test_grep_tool_still_nudges(self) -> None:
        out = self.run_guard("Grep", {"pattern": "x"})
        self.assertIn("mode=symbols", json.loads(out)["hookSpecificOutput"]["additionalContext"])

    def test_manifest_routes_bash_to_the_guard(self) -> None:
        manifest = json.loads((ROOT / "plugin/hooks/hooks.json").read_text())
        matchers = [entry["matcher"] for entry in manifest["hooks"]["PreToolUse"]]
        self.assertIn("Bash", matchers[0].split("|"))


if __name__ == "__main__":
    unittest.main()
