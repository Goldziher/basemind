"""Drive the publish gate (.github/scripts/require-green-ci.sh) against a stand-in `gh`.

The stand-in answers `gh api <path> -q <filter>` by running the filter with `jq` over canned
GitHub API responses, so the script's own queries decide every verdict.
"""

import json
import os
import shutil
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[2] / ".github" / "scripts" / "require-green-ci.sh"
SHA = "a" * 40

FAKE_GH = """#!/usr/bin/env python3
import json, os, re, subprocess, sys
args = [a for a in sys.argv[1:] if a != "--paginate"]
assert args[0] == "api" and args[2] == "-q", args
path, query = args[1], args[3]
responses = json.loads(open(os.environ["FAKE_GH_RESPONSES"]).read())
for pattern, body in responses.items():
    if re.search(pattern, path):
        sys.exit(subprocess.run(["jq", "-r", query], input=json.dumps(body), text=True).returncode)
sys.exit(f"stand-in gh: no response for {path}")
"""


def run(id_: int, event: str, conclusion: str = "success", branch: str = "main") -> dict:
    return {
        "id": id_,
        "event": event,
        "head_branch": branch,
        "status": "completed",
        "conclusion": conclusion,
        "created_at": f"2026-11-18T0{id_}:00:00Z",
    }


def jobs(*pairs: tuple[str, str]) -> dict:
    return {"jobs": [{"name": name, "conclusion": conclusion} for name, conclusion in pairs]}


TEST_LEGS = (("test / ubuntu-latest", "success"), ("test / windows-latest", "success"))
NIGHTLY_JOBS = (
    *TEST_LEGS,
    ("hardening harness (nightly)", "success"),
    ("ignored tests (nightly)", "success"),
    ("release build (nightly) / Smoke / x86_64-pc-windows-msvc", "success"),
)


def green() -> dict:
    return {
        r"workflows/ci\.yaml/runs": {"workflow_runs": [run(1, "push"), run(2, "workflow_dispatch")]},
        r"workflows/hardening\.yaml/runs": {"workflow_runs": [run(3, "schedule")]},
        r"runs/1/jobs": jobs(*TEST_LEGS),
        r"runs/2/jobs": jobs(*NIGHTLY_JOBS),
        r"check-runs": {"check_runs": []},
    }


@unittest.skipIf(os.name == "nt", "the gate runs on ubuntu-latest")
class RequireGreenCiTests(unittest.TestCase):
    def gate(self, responses: dict) -> subprocess.CompletedProcess:
        self.assertIsNotNone(shutil.which("jq"), "jq is required")
        with tempfile.TemporaryDirectory() as tmp:
            gh = Path(tmp) / "gh"
            gh.write_text(FAKE_GH, encoding="utf-8")
            gh.chmod(gh.stat().st_mode | stat.S_IEXEC)
            canned = Path(tmp) / "responses.json"
            canned.write_text(json.dumps(responses), encoding="utf-8")
            env = {
                **os.environ,
                "PATH": f"{tmp}{os.pathsep}{os.environ['PATH']}",
                "FAKE_GH_RESPONSES": str(canned),
                "GITHUB_REPOSITORY": "owner/repo",
                "CI_GATE_WAIT_MINUTES": "0",
                "CI_GATE_POLL_SECONDS": "0",
            }
            return subprocess.run(["bash", str(SCRIPT), SHA], env=env, capture_output=True, text=True, timeout=60)

    def refused(self, responses: dict, reason: str) -> None:
        done = self.gate(responses)
        self.assertNotEqual(done.returncode, 0, done.stderr)
        self.assertIn(reason, done.stderr)

    def test_all_release_gates_green_passes(self) -> None:
        done = self.gate(green())
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn("hardening run 3", done.stderr)

    def test_a_push_run_alone_is_refused(self) -> None:
        responses = green()
        responses[r"workflows/ci\.yaml/runs"] = {"workflow_runs": [run(1, "push")]}
        self.refused(responses, "no scheduled or dispatched ci.yaml run")

    def test_a_newer_green_branch_push_does_not_mask_a_red_main_run(self) -> None:
        responses = green()
        responses[r"workflows/ci\.yaml/runs"] = {
            "workflow_runs": [run(2, "workflow_dispatch"), run(4, "push", "failure"), run(5, "push", branch="feature")]
        }
        responses[r"runs/5/jobs"] = jobs(("test / ubuntu-latest / ", "success"))
        self.refused(responses, "ci run 4 on")

    def test_a_skipped_harden_harness_is_refused(self) -> None:
        responses = green()
        responses[r"runs/2/jobs"] = jobs(
            *NIGHTLY_JOBS[:2], ("hardening harness (nightly)", "skipped"), *NIGHTLY_JOBS[3:]
        )
        self.refused(responses, "hardening harness (nightly)")

    def test_a_failed_hardening_workflow_is_refused(self) -> None:
        responses = green()
        responses[r"workflows/hardening\.yaml/runs"] = {"workflow_runs": [run(3, "schedule", "failure")]}
        self.refused(responses, "hardening.yaml run 3")

    def test_no_hardening_run_is_refused(self) -> None:
        responses = green()
        responses[r"workflows/hardening\.yaml/runs"] = {"workflow_runs": [run(3, "push")]}
        self.refused(responses, "no scheduled or dispatched hardening.yaml run")


if __name__ == "__main__":
    unittest.main()
