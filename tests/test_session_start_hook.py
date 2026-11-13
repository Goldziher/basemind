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
            """printf '%s' '{"url":"http://127.0.0.1:32323/mcp?token=test"}'""",
        )
        self.executable(
            self.bin / "curl",
            """printf '%s\\n' "$@" > "$HOME/probe-args"
printf '%s' "$PROBE_RESULT" """,
        )

    @staticmethod
    def executable(path: Path, body: str) -> None:
        path.write_text(f"#!/bin/bash\n{body}\n")
        path.chmod(0o755)

    def invoke(self, response: str) -> str:
        env = {
            **os.environ,
            "HOME": str(self.root),
            "PATH": str(self.bin),
            "CLAUDE_PLUGIN_ROOT": str(self.root),
            "CLAUDE_PROJECT_DIR": "/some/project with space & ampersand",
            "PROBE_RESULT": response,
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

    def test_tools_list_confirms_routing_without_coreutils_timeout(self) -> None:
        result = self.invoke('{"id":1,"result":{"tools":[{"name":"code","inputSchema":{}}]}}')
        self.assertIn("tools were verified", result)
        self.assertIn("code grep for exact matches in indexed code", result)
        self.assertIn("Jupyter notebooks", result)
        args = (self.root / "probe-args").read_text().splitlines()
        self.assertIn("--max-time", args)
        self.assertIn(
            "http://127.0.0.1:32323/mcp?token=test&root=%2Fsome%2Fproject%20with%20space%20%26%20ampersand",
            args,
        )

    def test_zero_tools_and_failed_probe_have_different_guidance(self) -> None:
        empty = self.invoke('event: message\ndata: {"id":1,"result":{"tools":[]}}\n\n')
        self.assertIn("returned zero tools", empty)
        for result in ("", "not JSON", '{"id":1,"error":{"code":-32603}}'):
            with self.subTest(result=result):
                self.assertIn("availability could not be verified", self.invoke(result))


if __name__ == "__main__":
    unittest.main()
