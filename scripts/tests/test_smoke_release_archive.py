"""The release archive smoke's sandbox must be a profile the platform can resolve folders in."""

import importlib.util
import json
import os
import tempfile
import unittest
from pathlib import Path
from types import ModuleType
from unittest import mock


def load_smoke() -> ModuleType:
    path = Path(__file__).resolve().parents[1] / "smoke_release_archive.py"
    spec = importlib.util.spec_from_file_location("smoke_release_archive", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class SandboxEnvTests(unittest.TestCase):
    def test_windows_profile_folders_exist_under_the_sandbox_home(self) -> None:
        """Windows resolves LocalAppData as %USERPROFILE%\\AppData\\Local and refuses a missing one.

        The grammar cache lives there, so a sandbox profile without it fails `scan` with
        "Could not determine a cache directory" on the Windows release archive.
        """
        smoke = load_smoke()
        with tempfile.TemporaryDirectory() as root:
            env = smoke.sandbox_env(Path(root))
            profile = Path(env["USERPROFILE"])
            self.assertEqual(Path(env["HOME"]), profile)
            for variable, relative in (("LOCALAPPDATA", "AppData/Local"), ("APPDATA", "AppData/Roaming")):
                folder = profile / relative
                self.assertTrue(folder.is_dir(), f"{folder} was not created")
                self.assertEqual(Path(env[variable]), folder)

    def test_every_test_hook_is_set_to_a_value_a_reading_build_trips_on(self) -> None:
        """A release binary must ignore the hooks, so the sandbox sets each, over an inherited value."""
        smoke = load_smoke()
        with (
            tempfile.TemporaryDirectory() as root,
            mock.patch.dict(os.environ, {"BASEMIND_TEST_FAULT_LANE": "inherited"}),
        ):
            env = smoke.sandbox_env(Path(root))
            hooks = {name: value for name, value in env.items() if name.startswith("BASEMIND_TEST_")}
            self.assertEqual(
                hooks,
                {
                    "BASEMIND_TEST_FAULT_LANE": "resolve:abort",
                    "BASEMIND_TEST_COMMS_STORE_OPEN_DELAY_MS": "3600000",
                    "BASEMIND_TEST_ACCOUNT_HOME": str(Path(root) / "test-account-home"),
                },
            )


def status(state: str) -> str:
    return json.dumps({"store": {"state": state}})


class AgentStoreStateTests(unittest.TestCase):
    def test_waits_out_opening_and_returns_the_next_state(self) -> None:
        smoke = load_smoke()
        replies = iter([status("opening"), status("opening"), status("open")])
        with (
            mock.patch.object(smoke, "run", side_effect=lambda *_: next(replies)) as run,
            mock.patch.object(smoke.time, "sleep"),
        ):
            state = smoke.agent_store_state(Path("basemind"), Path("repo"), {})
        self.assertEqual(state, "open")
        self.assertEqual(run.call_count, 3)
        self.assertEqual(run.call_args.args[1], ["--root", "repo", "--json", "agent", "status"])

    def test_a_store_still_opening_at_the_deadline_fails_the_smoke(self) -> None:
        """What a binary that honoured the store-open delay hook reports for the whole deadline."""
        smoke = load_smoke()
        smoke.STORE_OPEN_DEADLINE = 0
        with mock.patch.object(smoke, "run", return_value=status("opening")), mock.patch.object(smoke.time, "sleep"):
            with self.assertRaises(SystemExit) as raised:
                smoke.agent_store_state(Path("basemind"), Path("repo"), {})
        self.assertEqual(str(raised.exception), "the agent store was still opening after 0s")


if __name__ == "__main__":
    unittest.main()
