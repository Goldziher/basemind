"""The release archive smoke's sandbox must be a profile the platform can resolve folders in."""

import importlib.util
import tempfile
import unittest
from pathlib import Path
from types import ModuleType


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


if __name__ == "__main__":
    unittest.main()
