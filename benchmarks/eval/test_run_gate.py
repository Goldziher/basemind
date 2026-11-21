"""Tests for `run_gate.sh`: the environment the gate runs the binary and its baselines in."""

import os
import subprocess
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).parent

FAKE_BIN = """#!/bin/sh
env > "$GATE_ENV_OUT"
exit 1
"""


class RunGate(unittest.TestCase):
    def test_the_binary_runs_without_host_git_configuration_in_the_c_locale(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            data, comms = tmp / "data", tmp / "comms"
            data.mkdir()
            comms.mkdir()
            hostile = tmp / "gitconfig"
            hostile.write_text("[color]\n\tui = always\n[core]\n\tautocrlf = true\n")
            fake = tmp / "basemind"
            fake.write_text(FAKE_BIN)
            fake.chmod(0o755)
            env_out = tmp / "env.txt"
            env = {k: v for k, v in os.environ.items() if k != "GIT_CONFIG_NOSYSTEM"}
            env.update(
                BASEMIND_DATA_HOME=str(data),
                BASEMIND_COMMS_DIR=str(comms),
                GATE_ENV_OUT=str(env_out),
                GIT_CONFIG_GLOBAL=str(hostile),
                LC_ALL="en_US.UTF-8",
            )
            proc = subprocess.run(
                ["bash", str(HERE / "run_gate.sh"), str(fake), str(tmp / "corpus"), str(tmp / "results")],
                capture_output=True,
                text=True,
                env=env,
                check=False,
            )
            self.assertEqual(proc.returncode, 2)
            self.assertIn("does not count o200k tokens", proc.stderr)
            seen = dict(line.split("=", 1) for line in env_out.read_text().splitlines() if "=" in line)
            self.assertEqual(
                (seen.get("GIT_CONFIG_NOSYSTEM"), seen.get("GIT_CONFIG_GLOBAL"), seen.get("LC_ALL")),
                ("1", "/dev/null", "C"),
            )


if __name__ == "__main__":
    unittest.main()
