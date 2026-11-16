"""Exercise the agent-as PreToolUse hook and the announcing inbox-notify hook against a stub launcher."""

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

ROOT = Path(__file__).resolve().parent.parent
HOOKS = ROOT / "plugin/.ai-rulez/hooks"
NAME = "session-1a2b3c4d"

INBOX = {
    "messages": [
        {
            "id": "t-abcdef.3",
            "subject": "auth refactor",
            "from": "claude-basemind-0a1b2c",
            "ts_micros": 1,
            "body": "",
            "body_bytes": 120,
        },
        {
            "id": "t-abcdef.4",
            "subject": "auth refactor",
            "from": "cursor-basemind-9f8e7d.perf",
            "ts_micros": 2,
            "body": "",
            "body_bytes": 7,
        },
    ],
    "more": False,
    "notices": [],
}

# The CLI starting the daemon the way `comms daemon` is started (a session of its own, stdio
# detached), then hanging. Until the CLI exits, the daemon is a direct child of the command the hook
# runs.
DAEMON_SPAWNING_CLI = """import os, sys, time
if os.fork() == 0:
    os.setsid()
    null = os.open(os.devnull, os.O_RDWR)
    for fd in (0, 1, 2):
        os.dup2(null, fd)
    with open(sys.argv[1] + ".tmp", "w") as out:
        out.write(str(os.getpid()))
    os.rename(sys.argv[1] + ".tmp", sys.argv[1])
    time.sleep(60)
    os._exit(0)
time.sleep(60)
"""

# Records its arguments and stdin, then answers like the CLI: `hook name`, `hook pre-tool-use` or `inbox`.
LAUNCHER = """[ -n "${SPAWN_DAEMON:-}" ] && exec "$PYTHON" -c "$DAEMON_SPAWNING_CLI" "$HOME/daemon-pid"
printf '%s\\n' "$@" > "$HOME/launcher-args"
cat > "$HOME/launcher-stdin"
case "$1 $2 $3" in
"agent hook name") printf '%s' "${NAME_RESULT-session-1a2b3c4d}"; exit "${NAME_EXIT:-0}" ;;
"agent hook pre-tool-use") sleep "${LAUNCHER_SLEEP:-0}"; printf '%s' "$HOOK_RESULT"; exit "${LAUNCHER_EXIT:-0}" ;;
"agent inbox "*) printf '%s' "$INBOX_RESULT"; exit "${LAUNCHER_EXIT:-0}" ;;
esac
"""


class AgentHookTests(unittest.TestCase):
    def setUp(self) -> None:
        if shutil.which("jq") is None:
            self.skipTest("jq is required")
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        for command in ("jq", "dirname", "cat", "tr", "sed", "grep", "sleep", "pkill"):
            (self.bin / command).symlink_to(shutil.which(command))
        hooks = self.root / "hooks"
        hooks.mkdir()
        for name in ("agent-as", "inbox-notify"):
            shutil.copyfile(HOOKS / name, hooks / name)
        scripts = self.root / "scripts"
        scripts.mkdir()
        launcher = scripts / "mcp-launch.sh"
        launcher.write_text(f"#!/bin/bash\n{LAUNCHER}")
        launcher.chmod(0o755)
        self.hooks = hooks

    def run_hook(self, name: str, event: dict | str, **extra: str) -> subprocess.CompletedProcess:
        env = {**os.environ, "HOME": str(self.root), "PATH": str(self.bin), "HOOK_RESULT": "", "INBOX_RESULT": ""}
        for variable in ("CURSOR_PLUGIN_ROOT", "COPILOT_CLI", "CLAUDE_PLUGIN_ROOT"):
            env.pop(variable, None)
        env.update(extra)
        payload = event if isinstance(event, str) else json.dumps(event)
        return subprocess.run(
            ["/bin/bash", str(self.hooks / name)],
            input=payload,
            env=env,
            cwd=self.root,
            capture_output=True,
            text=True,
            timeout=15,
            check=True,
        )

    def launcher_args(self) -> list[str]:
        return (self.root / "launcher-args").read_text().splitlines()

    def test_agent_as_pipes_the_event_to_the_cli_and_prints_its_answer(self) -> None:
        answer = json.dumps(
            {
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "updatedInput": {"body": "hi", "subject": "s", "as": NAME},
                }
            }
        )
        event = {
            "session_id": "s1",
            "tool_name": "mcp__plugin_basemind_basemind__agent_post",
            "tool_input": {"body": "hi"},
        }
        result = self.run_hook("agent-as", event, HOOK_RESULT=answer)
        self.assertEqual(self.launcher_args(), ["agent", "hook", "pre-tool-use"])
        self.assertEqual(json.loads((self.root / "launcher-stdin").read_text()), event)
        self.assertEqual(json.loads(result.stdout)["hookSpecificOutput"]["updatedInput"]["as"], NAME)

    def test_agent_as_is_silent_when_the_cli_has_nothing_to_say(self) -> None:
        result = self.run_hook("agent-as", {"tool_name": "x"}, HOOK_RESULT="")
        self.assertEqual(result.stdout, "")

    def test_agent_as_fails_open(self) -> None:
        self.assertEqual(
            self.run_hook("agent-as", {"tool_name": "x"}, HOOK_RESULT="partial", LAUNCHER_EXIT="1").stdout, ""
        )
        self.assertEqual(self.run_hook("agent-as", "", HOOK_RESULT="ignored").stdout, "")

    def test_agent_as_is_time_boxed_without_a_timeout_binary(self) -> None:
        started = time.monotonic()
        result = self.run_hook("agent-as", {"tool_name": "x"}, HOOK_RESULT="late", LAUNCHER_SLEEP="30")
        self.assertLess(time.monotonic() - started, 12)
        self.assertEqual(result.stdout, "")

    def assert_the_watchdog_spares_the_daemon(self, hook: str) -> None:
        started = time.monotonic()
        result = self.run_hook(
            hook,
            {"session_id": "s1", "tool_name": "x", "prompt": "go"},
            SPAWN_DAEMON="1",
            PYTHON=sys.executable,
            DAEMON_SPAWNING_CLI=DAEMON_SPAWNING_CLI,
        )
        self.assertLess(time.monotonic() - started, 12)
        self.assertEqual(result.stdout, "")
        daemon = int((self.root / "daemon-pid").read_text())
        self.addCleanup(os.kill, daemon, signal.SIGKILL)
        os.kill(daemon, 0)

    def test_agent_as_watchdog_leaves_a_daemon_the_cli_started_running(self) -> None:
        self.assert_the_watchdog_spares_the_daemon("agent-as")

    def test_inbox_notify_watchdog_leaves_a_daemon_the_cli_started_running(self) -> None:
        self.assert_the_watchdog_spares_the_daemon("inbox-notify")

    def test_agent_as_does_not_start_the_launcher_on_a_host_that_ignores_the_rewrite(self) -> None:
        for marker in ("CURSOR_PLUGIN_ROOT", "COPILOT_CLI"):
            result = self.run_hook("agent-as", {"tool_name": "x"}, HOOK_RESULT="{}", **{marker: "1"})
            self.assertEqual(result.stdout, "", marker)
            self.assertFalse((self.root / "launcher-args").exists(), marker)

    def test_agent_as_is_time_boxed_when_timeout_exists(self) -> None:
        (self.bin / "timeout").write_text('#!/bin/bash\nprintf "%s\\n" "$1" > "$HOME/timeout-arg"\nshift\nexec "$@"\n')
        (self.bin / "timeout").chmod(0o755)
        self.run_hook("agent-as", {"tool_name": "x"}, HOOK_RESULT="{}")
        self.assertEqual((self.root / "timeout-arg").read_text().strip(), "6")

    def announce(self, inbox: dict | str, **extra: str) -> subprocess.CompletedProcess:
        text = inbox if isinstance(inbox, str) else json.dumps(inbox)
        return self.run_hook("inbox-notify", {"session_id": "s1", "prompt": "go"}, INBOX_RESULT=text, **extra)

    def test_inbox_notify_announces_under_the_session_name_and_lists_each_message(self) -> None:
        result = self.announce(INBOX, CLAUDE_PLUGIN_ROOT=str(self.root))
        args = self.launcher_args()
        self.assertEqual(args[:2], ["agent", "inbox"])
        for flag in ("--announce", "--json"):
            self.assertIn(flag, args)
        self.assertEqual(args[args.index("--as") + 1], NAME)
        self.assertEqual(args[args.index("--limit") + 1], "10")
        self.assertEqual(Path(args[args.index("--root") + 1]).resolve(), self.root.resolve())
        payload = json.loads(result.stdout)["hookSpecificOutput"]
        self.assertEqual(payload["hookEventName"], "UserPromptSubmit")
        context = payload["additionalContext"]
        self.assertIn("from claude-basemind-0a1b2c in auth refactor (t-abcdef.3, 120 bytes)", context)
        self.assertIn("from cursor-basemind-9f8e7d.perf in auth refactor (t-abcdef.4, 7 bytes)", context)
        self.assertIn("agent_inbox", context)
        self.assertNotIn("more unread than listed", context)

    def test_inbox_notify_says_when_more_is_unread_than_listed(self) -> None:
        context = json.loads(self.announce({**INBOX, "more": True}, CLAUDE_PLUGIN_ROOT=str(self.root)).stdout)
        self.assertIn("more unread than listed", context["hookSpecificOutput"]["additionalContext"])

    def test_inbox_notify_keeps_no_state_between_turns(self) -> None:
        first = self.announce(INBOX).stdout
        second = self.announce(INBOX).stdout
        self.assertEqual(first, second)
        self.assertNotEqual(first, "")
        self.assertEqual(list(self.root.glob("**/*hwm*")), [])

    def test_inbox_notify_is_silent_without_messages_or_a_name(self) -> None:
        self.assertEqual(self.announce({"messages": [], "more": False}).stdout, "")
        self.assertEqual(self.announce("not JSON").stdout, "")
        self.assertEqual(self.announce("").stdout, "")
        self.assertEqual(self.announce(INBOX, LAUNCHER_EXIT="1").stdout, "")

    def test_inbox_notify_announces_as_the_principal_when_the_host_names_no_session(self) -> None:
        result = self.announce(INBOX, NAME_RESULT="")
        self.assertNotIn("--as", self.launcher_args(), "the tool calls act as the principal there")
        self.assertIn("t-abcdef.3", json.loads(result.stdout)["additionalContext"])

    def test_inbox_notify_says_nothing_when_naming_the_session_fails(self) -> None:
        for code in ("124", "143", "1"):
            (self.root / "launcher-args").unlink(missing_ok=True)
            result = self.announce(INBOX, NAME_RESULT="", NAME_EXIT=code)
            self.assertEqual(result.stdout, "", code)
            self.assertEqual(self.launcher_args()[:3], ["agent", "hook", "name"], "no principal-level read follows")

    def test_inbox_notify_builds_valid_json_from_any_subject(self) -> None:
        hostile = 'quote " backslash \\ tab \t bell \u0007 }{'
        inbox = {**INBOX, "messages": [{**INBOX["messages"][0], "subject": hostile}]}
        payload = json.loads(self.announce(inbox, CLAUDE_PLUGIN_ROOT=str(self.root)).stdout)
        self.assertIn(hostile, payload["hookSpecificOutput"]["additionalContext"])

    def test_inbox_notify_emits_the_harness_shape(self) -> None:
        cursor = json.loads(self.announce(INBOX, CURSOR_PLUGIN_ROOT="/p").stdout)
        self.assertIn("additional_context", cursor)
        plain = json.loads(self.announce(INBOX).stdout)
        self.assertIn("additionalContext", plain)


class StatusLineCommsTests(unittest.TestCase):
    """The status line counts unread agent mail on a machine with no `timeout` binary."""

    def setUp(self) -> None:
        if shutil.which("jq") is None:
            self.skipTest("jq is required")
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        # The system commands and jq, without `timeout`, as on stock macOS.
        self.bin = self.root / "bin"
        self.bin.mkdir()
        for directory in ("/bin", "/usr/bin", "/sbin", "/usr/sbin", str(Path(shutil.which("jq")).parent)):
            if not Path(directory).is_dir():
                continue
            for entry in os.scandir(directory):
                target = self.bin / entry.name
                if entry.name in ("timeout", "gtimeout", "basemind", "pgrep") or target.exists():
                    continue
                if os.access(entry.path, os.X_OK) and not entry.is_dir():
                    target.symlink_to(entry.path)
        stubs = {
            "pgrep": "exit 0",
            "basemind": """printf '%s\\n' "$@" >> "$HOME/basemind-args"
case "$1 $2 $3" in
"agent hook name") cat >/dev/null; printf '%s' session-1a2b3c4d ;;
"agent inbox --peek") printf '%s' '{"messages":[{"id":"t-abcdef.1"},{"id":"t-abcdef.2"}],"more":false}' ;;
esac""",
        }
        for name, body in stubs.items():
            (self.bin / name).write_text(f"#!/bin/bash\n{body}\n")
            (self.bin / name).chmod(0o755)
        self.work = self.root / "work"
        (self.work / ".basemind/blobs").mkdir(parents=True)
        (self.work / ".basemind/views/working").mkdir(parents=True)
        (self.work / ".basemind/blobs/0aaaaaaaa.fm.msgpack").touch()
        (self.work / ".basemind/views/working/index.msgpack").touch()

    def test_unread_mail_is_counted_for_the_session(self) -> None:
        env = {**os.environ, "HOME": str(self.root), "TMPDIR": str(self.root), "PATH": str(self.bin), "COLUMNS": "200"}
        payload = json.dumps({"session_id": "s1", "workspace": {"current_dir": str(self.work)}})
        result = subprocess.run(
            ["/bin/bash", str(ROOT / ".claude-plugin/statusline.sh")],
            input=payload,
            env=env,
            capture_output=True,
            text=True,
            timeout=30,
            check=True,
        )
        self.assertIn("\u2709 2", result.stdout)
        args = (self.root / "basemind-args").read_text().splitlines()
        self.assertEqual(args[args.index("--as") + 1], NAME)


class AgentAsManifestTests(unittest.TestCase):
    MANIFESTS = (
        "plugin/.ai-rulez/hooks/hooks.json",
        "plugin/hooks/hooks.json",
        "hooks/hooks.json",
        ".codex-plugin/hooks/hooks.json",
    )
    # Hand-written manifests of hosts whose input rewrite basemind does not apply: the agent tools
    # act at principal level there (docs/comms-v2.md).
    WITHOUT_AGENT_AS = (".cursor-plugin/hooks/hooks.json", "gemini-extension.json")

    def entries(self, manifest: str) -> list[dict]:
        return json.loads((ROOT / manifest).read_text())["hooks"]["PreToolUse"]

    def test_the_agent_tools_have_an_agent_as_entry_and_the_guard_keeps_its_own(self) -> None:
        for manifest in self.MANIFESTS:
            entries = self.entries(manifest)
            by_command = {hook["command"].rsplit(" ", 1)[1]: entry for entry in entries for hook in entry["hooks"]}
            self.assertIn("agent-as", by_command, manifest)
            self.assertLessEqual(set(by_command), {"pre-tool-guard", "agent-as"}, manifest)
            if "pre-tool-guard" in by_command:
                self.assertEqual(by_command["pre-tool-guard"]["matcher"], "Grep|Glob|Bash", manifest)
            matcher = re.compile(by_command["agent-as"]["matcher"])
            for tool in ("agent_post", "agent_inbox", "agent_threads"):
                self.assertTrue(matcher.fullmatch(f"mcp__plugin_basemind_basemind__{tool}"), (manifest, tool))
                self.assertTrue(matcher.fullmatch(f"mcp__basemind__{tool}"), (manifest, tool))
            for tool in ("code_outline", "memory_save", "agent_other"):
                self.assertIsNone(matcher.fullmatch(f"mcp__plugin_basemind_basemind__{tool}"), (manifest, tool))

    def test_hosts_that_cannot_take_the_rewrite_do_not_run_agent_as(self) -> None:
        for manifest in self.WITHOUT_AGENT_AS:
            self.assertNotIn("agent-as", (ROOT / manifest).read_text(), manifest)
        self.assertFalse((ROOT / ".cursor-plugin/hooks/agent-as").exists())

    def test_agent_as_ships_wherever_the_hook_scripts_do(self) -> None:
        for directory in ("plugin/.ai-rulez/hooks", "plugin/hooks", "hooks", ".codex-plugin/hooks"):
            self.assertTrue((ROOT / directory / "agent-as").is_file(), directory)
            self.assertTrue(os.access(ROOT / directory / "agent-as", os.X_OK), directory)


if __name__ == "__main__":
    unittest.main()
