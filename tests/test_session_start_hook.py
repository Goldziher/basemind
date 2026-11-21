"""Exercise SessionStart with local command fixtures."""

import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

from test_agent_hooks import DAEMON_SPAWNING_CLI

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
# Listed apart from the verb tools: the hook names them without listing them as loaded.
AGENT_TOOLS = ("agent_post", "agent_inbox", "agent_threads")
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
        for command in ("jq", "dirname", "sed", "grep", "wc", "tr", "cat", "sleep", "pkill", "id", "mkdir", "ls", "rm"):
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
agent)
  [ -n "${SPAWN_DAEMON:-}" ] && exec "$PYTHON" -c "$DAEMON_SPAWNING_CLI" "$HOME/daemon-pid"
  if [ "$2" = hook ]; then
    cat > "$HOME/hook-stdin"
    printf '%s' "${NAME_RESULT-session-1a2b3c4d}"
    exit "${AGENT_EXIT:-0}"
  fi
  printf '%s\\n' "$@" > "$HOME/inbox-args"
  printf '%s' "$INBOX_RESULT"
  ;;
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

    def invoke(self, response: str, *, inbox: str = "", exit_code: int = 0, **extra: str) -> str:
        # Codex's markers: the hook takes a session without them for Claude Code's.
        env = {
            **{key: value for key, value in os.environ.items() if key not in ("PLUGIN_ROOT", "CODEX_PLUGIN_ROOT")},
            "HOME": str(self.root),
            "PATH": str(self.bin),
            "CLAUDE_PLUGIN_ROOT": str(self.root),
            "CLAUDE_PROJECT_DIR": "/some/project with space & ampersand",
            "TMPDIR": str(self.root),
            "XDG_RUNTIME_DIR": "",
            "STATUS_RESULT": response,
            "STATUS_EXIT": str(exit_code),
            "INBOX_RESULT": inbox,
            **extra,
        }
        env.pop("CURSOR_PLUGIN_ROOT", None)
        env.pop("COPILOT_CLI", None)
        output = subprocess.run(
            ["/bin/bash", self.hook],
            input=json.dumps({"session_id": "s1", "source": "startup"}),
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
        self.assertIn(", ".join(VERB_TOOLS), result)
        self.assertIn("mcp__plugin_basemind_basemind__<tool>", result)
        self.assertNotIn("ToolSearch", result)
        self.assertIn("the basemind server instructions name the tool for each question", result)
        self.assertNotIn("code_grep {", result)
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

    def test_the_index_verdict_is_left_for_the_guard_only_when_verified(self) -> None:
        verdict = self.root / f"basemind-{os.getuid()}" / "indexed" / "s1"
        self.invoke('{"file_count":631,"schema_version":33}')
        self.assertTrue(verdict.is_file(), "a verified index lets the guard steer this session")
        self.invoke('{"file_count":0,"schema_version":33}')
        self.assertFalse(verdict.exists(), "an empty index withdraws it")
        self.invoke('{"file_count":631,"schema_version":33}')
        self.invoke("", exit_code=1)
        self.assertFalse(verdict.exists(), "so does a failed probe")

    def test_tool_names_are_exactly_the_advertised_surface(self) -> None:
        declared = re.search(r'^TOOL_NAMES="([^"]*)"$', HOOK.read_text(), re.MULTILINE)
        self.assertIsNotNone(declared, "the hook must declare TOOL_NAMES")
        self.assertEqual(declared.group(1).split(), list(VERB_TOOLS))

    def test_a_host_other_than_claude_code_gets_the_routing_itself(self) -> None:
        result = self.invoke('{"file_count":631,"schema_version":33}', PLUGIN_ROOT=str(self.root))
        self.assertIn('code_grep {"pattern":"cache_root("}', result)
        self.assertIn("Jupyter notebooks", result)
        self.assertNotIn("server instructions", result)
        self.assertNotIn("mcp__plugin_basemind_basemind__", result)

    def test_routing_names_only_verb_tools(self) -> None:
        result = self.invoke('{"file_count":631,"schema_version":33}', PLUGIN_ROOT=str(self.root))
        calls = ROUTED_CALL.findall(result)
        self.assertGreaterEqual(len(calls), 12, result)
        for name, arguments in calls:
            self.assertIn(name, VERB_TOOLS, result)
            self.assertIsInstance(json.loads(arguments), dict)
        self.assertIsNone(REMOVED_SPELLING.search(result), result)

    def test_timeout_is_reported_as_such(self) -> None:
        self.executable(self.bin / "timeout", "exit 124")
        self.assertIn("timed out", self.invoke(""))

    def test_a_hung_status_probe_is_cut_off_without_coreutils_timeout(self) -> None:
        self.executable(self.root / "scripts/mcp-launch.sh", "exec sleep 60")
        started = time.monotonic()
        result = self.invoke("", BASEMIND_HOOK_STATUS_TIMEOUT="1")
        self.assertLess(time.monotonic() - started, 12)
        self.assertIn("timed out after 1s", result)

    def test_malformed_inbox_leaves_startup_context_intact(self) -> None:
        self.executable(self.bin / "timeout", 'shift; exec "$@"')
        context = self.invoke('{"file_count":5,"schema_version":33}', inbox="not JSON")
        self.assertIn("basemind verified", context)
        self.assertNotIn("No messages in your inbox", context)

    def test_agent_tools_are_named_apart_from_the_loaded_ones_without_a_host_feature(self) -> None:
        result = self.invoke('{"file_count":5,"schema_version":33}')
        self.assertIn(
            "The agent tools agent_post, agent_inbox and agent_threads are listed separately; "
            "they are needed only when collaborating",
            result,
        )
        self.assertNotIn("deferred", result)
        self.assertIn("passes `as`", result)
        loaded = result.split("Its tools are always loaded")[1].split(".")[0]
        for tool in AGENT_TOOLS:
            self.assertNotIn(tool, loaded)
        self.assertNotIn("THREADS", result)
        self.assertNotIn("agents", result)

    def test_unread_messages_are_counted_with_a_peek(self) -> None:
        self.executable(self.bin / "timeout", 'shift; exec "$@"')
        status = '{"file_count":5,"schema_version":33}'
        none = self.invoke(status, inbox='{"messages":[],"more":false}')
        self.assertNotIn("unread agent message", none)
        some = self.invoke(status, inbox='{"messages":[{"id":"t-abcdef.1"},{"id":"t-abcdef.2"}],"more":false}')
        self.assertIn("You have 2 unread agent message(s); read them with agent_inbox.", some)
        args = (self.root / "inbox-args").read_text().splitlines()
        self.assertEqual(args[:3], ["agent", "inbox", "--peek"])
        self.assertIn("--json", args)
        more = self.invoke(status, inbox='{"messages":[{"id":"t-abcdef.1"}],"more":true}')
        self.assertIn("You have at least 1 unread agent message(s)", more)

    def test_the_peek_reads_as_the_actor_the_agent_tools_act_as(self) -> None:
        status = '{"file_count":5,"schema_version":33}'
        inbox = '{"messages":[{"id":"t-abcdef.1"}],"more":false}'
        self.invoke(status, inbox=inbox)
        self.assertEqual(json.loads((self.root / "hook-stdin").read_text())["session_id"], "s1")
        args = (self.root / "inbox-args").read_text().splitlines()
        self.assertEqual(args[args.index("--as") + 1], "session-1a2b3c4d")
        self.invoke(status, inbox=inbox, NAME_RESULT="")
        args = (self.root / "inbox-args").read_text().splitlines()
        self.assertNotIn("--as", args, "a host that injects no name reads as the principal")

    def test_the_watchdog_leaves_a_daemon_the_cli_started_running(self) -> None:
        started = time.monotonic()
        result = self.invoke(
            '{"file_count":5,"schema_version":33}',
            SPAWN_DAEMON="1",
            PYTHON=sys.executable,
            DAEMON_SPAWNING_CLI=DAEMON_SPAWNING_CLI,
        )
        self.assertLess(time.monotonic() - started, 12)
        self.assertIn("basemind verified", result)
        daemon = int((self.root / "daemon-pid").read_text())
        self.addCleanup(os.kill, daemon, signal.SIGKILL)
        os.kill(daemon, 0)

    def test_a_build_without_agent_messaging_names_no_agent_tool(self) -> None:
        result = self.invoke('{"file_count":5,"schema_version":33}', AGENT_EXIT="2")
        for tool in AGENT_TOOLS:
            self.assertNotIn(tool, result)
        self.assertFalse((self.root / "inbox-args").exists())

    def test_session_start_hook_runs_on_resume(self) -> None:
        manifest = json.loads((ROOT / "plugin/hooks/hooks.json").read_text())
        matchers = {source for entry in manifest["hooks"]["SessionStart"] for source in entry["matcher"].split("|")}
        self.assertIn("resume", matchers)


if __name__ == "__main__":
    unittest.main()
