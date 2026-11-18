"""The publish gate accepts only a CI run that ran the harden harness on the released commit."""

import json
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / ".github/scripts/require-green-ci.sh"
SHA = "a" * 40

# `gh api [--paginate] <endpoint> -q <filter>`: answer from the JSON fixture the endpoint maps to.
FAKE_GH = """#!/usr/bin/env bash
set -euo pipefail
[ "$1" = api ] || exit 2
shift
[ "$1" = --paginate ] && shift
endpoint="$1"
[ "$2" = -q ] || exit 2
case "$endpoint" in
*/actions/workflows/ci.yaml/runs*) fixture=runs.json ;;
*/jobs*) fixture=jobs.json ;;
*/check-runs*) fixture=checks.json ;;
*) exit 3 ;;
esac
jq -r "$3" "$FIXTURES/$fixture"
"""


def job(name: str, conclusion: str = "success") -> dict:
    """One job row of the run's jobs listing."""
    return {"name": name, "conclusion": conclusion}


@unittest.skipUnless(shutil.which("jq"), "the fake gh answers through jq")
class RequireGreenCiTests(unittest.TestCase):
    """Each case writes the GitHub API answers for one commit and runs the gate against them."""

    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory(prefix="require-green-ci-")
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        bin_dir = self.root / "bin"
        bin_dir.mkdir()
        gh = bin_dir / "gh"
        gh.write_text(FAKE_GH)
        gh.chmod(0o755)
        self.env = {
            **os.environ,
            "PATH": f"{bin_dir}{os.pathsep}{os.environ['PATH']}",
            "FIXTURES": str(self.root),
            "GITHUB_REPOSITORY": "owner/repo",
            "CI_GATE_WAIT_MINUTES": "0",
            "CI_GATE_POLL_SECONDS": "0",
        }
        self.write("checks.json", {"check_runs": []})

    def write(self, name: str, value: dict) -> None:
        (self.root / name).write_text(json.dumps(value))

    def runs(self, *events: str) -> None:
        rows = [
            {
                "id": 100 + i,
                "event": event,
                "status": "completed",
                "conclusion": "success",
                "created_at": f"2026-11-18T0{i}:00:00Z",
            }
            for i, event in enumerate(events)
        ]
        self.write("runs.json", {"workflow_runs": rows})

    def gate(self) -> subprocess.CompletedProcess:
        return subprocess.run(["bash", str(SCRIPT), SHA], env=self.env, capture_output=True, text=True, check=False)

    def test_a_dispatched_run_with_a_green_harden_harness_passes(self) -> None:
        self.runs("workflow_dispatch")
        self.write("jobs.json", {"jobs": [job("test / linux (full)"), job("hardening harness (nightly)")]})
        result = self.gate()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("the harden harness succeeded", result.stderr)

    def test_a_push_run_does_not_vouch_for_a_release(self) -> None:
        self.runs("push")
        self.write("jobs.json", {"jobs": [job("test / linux (full)")]})
        result = self.gate()
        self.assertEqual(result.returncode, 1)
        self.assertIn("no completed ci run", result.stderr)

    def test_a_skipped_harden_harness_is_refused(self) -> None:
        self.runs("workflow_dispatch")
        self.write("jobs.json", {"jobs": [job("test / linux (full)"), job("hardening harness (nightly)", "skipped")]})
        result = self.gate()
        self.assertEqual(result.returncode, 1)
        self.assertIn("'hardening harness (nightly)' is 'skipped'", result.stderr)

    def test_a_newer_push_run_does_not_hide_the_dispatched_one(self) -> None:
        self.runs("workflow_dispatch", "push")
        self.write("jobs.json", {"jobs": [job("test / linux (full)"), job("hardening harness (nightly)")]})
        result = self.gate()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("newest ci run on", result.stderr)
        self.assertIn(": 100 (success)", result.stderr)


if __name__ == "__main__":
    unittest.main()
