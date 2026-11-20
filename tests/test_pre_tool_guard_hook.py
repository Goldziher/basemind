"""Exercise the PreToolUse guard: Bash search commands are nudged once per agent, other commands never."""

import json
import os
import re
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
HOOK = ROOT / "plugin/.ai-rulez/hooks/pre-tool-guard"

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
# A suggested call: a tool name followed by its JSON arguments.
SUGGESTED_CALL = re.compile(r"\b([a-z]+_[a-z]+) (\{[^{}]*\})")
# The surface ADR-0018 removed: a `mode` argument, and the `code` / `git` / `memory` / `admin` tools.
REMOVED_SPELLING = re.compile(r"\bmode\b|\b(?:code|git|memory|admin) (?:tool|modes?)\b|\b(?:code|git|memory|admin) *\{")


class PreToolGuardTests(unittest.TestCase):
    def setUp(self) -> None:
        if shutil.which("jq") is None:
            self.skipTest("jq is required")
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.tmp = temporary.name
        # session-start's verdict that the workspace is indexed, for the sessions these tests use.
        self.private = Path(self.tmp) / f"basemind-{os.getuid()}"
        (self.private / "indexed").mkdir(parents=True)
        self.private.chmod(0o700)
        for session in ("s", "s1"):
            (self.private / "indexed" / session).touch()

    def run_guard(self, tool: str, tool_input: dict, **extra: str) -> str:
        payload = {"session_id": "s1", "tool_name": tool, "tool_input": tool_input, **extra}
        output = subprocess.run(
            ["/bin/bash", str(HOOK)],
            input=json.dumps(payload),
            env={**os.environ, "TMPDIR": self.tmp, "XDG_RUNTIME_DIR": "", "BASEMIND_GUARD": "nudge"},
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
            "rg foo src/": 'code_grep {"pattern":"…"}',
            "cd src && grep -rn foo .": 'code_grep {"pattern":"…"}',
            "git blame src/lib.rs": 'git_blame {"path":"…"}',
            "git -C repo log --oneline": 'git_log {"path":"…"}',
            "git show HEAD~1": 'git_log {"rev":"…"}',
            "git diff": 'git_diff {"path":"…"}',
            "git -C repo diff HEAD~1 -- src/lib.rs": 'git_diff {"path":"…"}',
            "git status": "git_diff {} (the working-tree status",
            "cd repo && git status --short": "git_diff {} (the working-tree status",
            "find . -name '*.rs'": 'find_files {"query":"…"}',
        }
        for command, expected in cases.items():
            with self.subTest(command=command):
                out = self.bash(command, agent_id=command)
                decision = json.loads(out)["hookSpecificOutput"]
                self.assertNotIn("permissionDecision", decision)
                self.assertIn(expected, decision["additionalContext"])

    def assert_names_only_verb_tools(self, suggestion: str) -> None:
        calls = SUGGESTED_CALL.findall(suggestion)
        self.assertTrue(calls, f"no tool call in: {suggestion}")
        for name, arguments in calls:
            self.assertIn(name, VERB_TOOLS, suggestion)
            self.assertIsInstance(json.loads(arguments), dict)
        self.assertIsNone(REMOVED_SPELLING.search(suggestion), suggestion)
        self.assertIn("already loaded", suggestion)

    def test_every_suggestion_is_a_verb_tool_call(self) -> None:
        commands = (
            "rg foo src/",
            "git blame src/lib.rs",
            "git log --oneline",
            "git show HEAD~1",
            "git diff src/lib.rs",
            "git status",
            "find . -name '*.rs'",
        )
        suggestions = [self.bash(command, agent_id=f"bash-{index}") for index, command in enumerate(commands)]
        suggestions.append(self.run_guard("Grep", {"pattern": "x"}, agent_id="grep"))
        suggestions.append(self.run_guard("Glob", {"pattern": "**/*.rs"}, agent_id="glob"))
        self.assertEqual(len(suggestions), 9)
        for out in suggestions:
            decision = json.loads(out)["hookSpecificOutput"]
            with self.subTest(suggestion=decision["additionalContext"]):
                self.assert_names_only_verb_tools(decision["additionalContext"])

    def test_redirect_reason_is_a_verb_tool_call(self) -> None:
        env = {**os.environ, "TMPDIR": self.tmp, "XDG_RUNTIME_DIR": "", "BASEMIND_GUARD": "redirect"}
        for tool, tool_input in (("Grep", {"pattern": "x"}), ("Glob", {"pattern": "**/*.rs"})):
            payload = json.dumps({"session_id": "s", "tool_name": tool, "tool_input": tool_input})
            out = subprocess.run(
                ["/bin/bash", str(HOOK)], input=payload, env=env, capture_output=True, text=True, check=True
            ).stdout
            decision = json.loads(out)["hookSpecificOutput"]
            with self.subTest(tool=tool):
                self.assertEqual(decision["permissionDecision"], "deny")
                self.assert_names_only_verb_tools(decision["permissionDecisionReason"])

    def test_grep_tool_pattern_is_carried_into_the_suggested_call(self) -> None:
        pattern = 'fn \\w+_call\\("x"'
        out = self.run_guard("Grep", {"pattern": pattern})
        context = json.loads(out)["hookSpecificOutput"]["additionalContext"]
        expected = "code_grep " + json.dumps({"pattern": pattern, "regex": True}, separators=(",", ":"))
        self.assertIn(expected, context)

    def grep_call(self, tool_input: dict, cwd: Path, agent: str) -> dict:
        out = self.run_guard("Grep", tool_input, cwd=str(cwd), agent_id=agent)
        context = json.loads(out)["hookSpecificOutput"]["additionalContext"]
        arguments = context[context.index("code_grep {") + len("code_grep ") :]
        return json.JSONDecoder().raw_decode(arguments)[0]

    def test_grep_tool_path_and_glob_scope_the_suggested_call(self) -> None:
        repo = Path(self.tmp) / "repo"
        (repo / "src" / "mcp").mkdir(parents=True)
        (repo / "src" / "lib.rs").touch()
        (repo / "Cargo.toml").touch()
        outside = Path(self.tmp) / "outside"
        outside.mkdir()
        cases = (
            ({"path": "src/mcp"}, "src/mcp/"),
            ({"path": "./src/"}, "src/"),
            ({"path": str(repo / "src" / "lib.rs")}, "src/lib.rs"),
            ({"path": "Cargo.toml"}, "Cargo.toml"),
            ({"glob": "*.rs"}, "**/*.rs"),
            ({"path": "src", "glob": "*.{rs,toml}"}, "src/**/*.{rs,toml}"),
            ({"path": "src", "glob": "mcp/*.rs"}, "src/mcp/*.rs"),
            ({"path": "src/lib.rs", "glob": "*.rs"}, "src/lib.rs"),
            ({"path": str(repo)}, None),
            ({"path": "."}, None),
            ({"path": str(outside)}, None),
            ({"path": "missing/"}, None),
            ({"glob": "!*.lock"}, None),
        )
        for index, (scope, path) in enumerate(cases):
            with self.subTest(scope=scope):
                expected = {"pattern": "fn open", "regex": True}
                if path is not None:
                    expected["path"] = path
                call = self.grep_call({"pattern": "fn open", **scope}, repo, f"scope-{index}")
                self.assertEqual(call, expected)

    def test_grep_tool_path_is_relative_to_the_git_top_level(self) -> None:
        if shutil.which("git") is None:
            self.skipTest("git is required")
        repo = Path(self.tmp) / "repo"
        (repo / "src" / "mcp").mkdir(parents=True)
        subprocess.run(["git", "init", "-q", str(repo)], check=True)
        call = self.grep_call({"pattern": "x", "path": "mcp"}, repo / "src", "toplevel")
        self.assertEqual(call, {"pattern": "x", "regex": True, "path": "src/mcp/"})

    def test_other_bash_commands_are_ignored(self) -> None:
        commands = (
            "ls -la",
            "cargo test | grep FAIL",
            "echo rg",
            "git diff --cached",
            "git diff --staged src/lib.rs",
            "git diff --no-index a.txt b.txt",
            "git stash list | git status-report",
        )
        for command in commands:
            with self.subTest(command=command):
                self.assertEqual(self.bash(command, agent_id=command), "")

    def test_nudge_fires_once_per_agent_and_kind(self) -> None:
        self.assertNotEqual(self.bash("rg a", agent_id="a1"), "")
        self.assertEqual(self.bash("grep b", agent_id="a1"), "")
        self.assertNotEqual(self.bash("rg a", agent_id="a2"), "")
        self.assertNotEqual(self.bash("git blame x", agent_id="a1"), "")

    def test_markers_stay_in_a_private_directory_and_never_follow_a_planted_link(self) -> None:
        self.assertNotEqual(self.bash("rg a", agent_id="own"), "")
        private = self.private
        self.assertEqual(private.stat().st_mode & 0o777, 0o700)
        self.assertTrue(any((private / "guard").iterdir()), "the marker is in the private directory")

        shutil.rmtree(private)
        elsewhere = Path(self.tmp) / "elsewhere"
        (elsewhere / "indexed").mkdir(parents=True)
        (elsewhere / "indexed" / "s1").touch()
        private.symlink_to(elsewhere)
        self.assertEqual(self.bash("rg a", agent_id="planted"), "", "a planted verdict is not read")
        self.assertEqual(list((elsewhere / "indexed").iterdir()), [elsewhere / "indexed" / "s1"])
        self.assertFalse((elsewhere / "guard").exists(), "nothing is written through the link")

    def test_a_session_without_a_verified_index_is_not_steered(self) -> None:
        (self.private / "indexed" / "s1").unlink()
        self.assertEqual(self.bash("rg a"), "")
        self.assertEqual(self.run_guard("Grep", {"pattern": "x"}), "")
        env = {**os.environ, "TMPDIR": self.tmp, "XDG_RUNTIME_DIR": "", "BASEMIND_GUARD": "redirect"}
        payload = json.dumps({"session_id": "unscanned", "tool_name": "Grep", "tool_input": {"pattern": "x"}})
        out = subprocess.run(
            ["/bin/bash", str(HOOK)], input=payload, env=env, capture_output=True, text=True, check=True
        ).stdout
        self.assertEqual(out, "", "redirect does not deny a search the index cannot answer")

    def test_redirect_mode_never_denies_bash(self) -> None:
        env = {**os.environ, "TMPDIR": self.tmp, "XDG_RUNTIME_DIR": "", "BASEMIND_GUARD": "redirect"}
        payload = json.dumps({"session_id": "s", "tool_name": "Bash", "tool_input": {"command": "rg x"}})
        out = subprocess.run(
            ["/bin/bash", str(HOOK)], input=payload, env=env, capture_output=True, text=True, check=True
        ).stdout
        self.assertNotIn("permissionDecision", json.loads(out)["hookSpecificOutput"])

    def test_grep_tool_still_nudges(self) -> None:
        out = self.run_guard("Grep", {"pattern": "x"})
        context = json.loads(out)["hookSpecificOutput"]["additionalContext"]
        self.assertIn('code_grep {"pattern":"x","regex":true}', context)
        self.assertIn('code_symbols {"name":"…"}', context)

    def test_manifest_routes_bash_to_the_guard(self) -> None:
        manifest = json.loads((ROOT / "plugin/hooks/hooks.json").read_text())
        matchers = [entry["matcher"] for entry in manifest["hooks"]["PreToolUse"]]
        self.assertIn("Bash", matchers[0].split("|"))


if __name__ == "__main__":
    unittest.main()
