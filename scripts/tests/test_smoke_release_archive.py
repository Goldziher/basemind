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


class MissingNoticesTests(unittest.TestCase):
    def unpacked(self, root: Path, *files: str) -> Path:
        for relative in files:
            (root / relative).parent.mkdir(parents=True, exist_ok=True)
            (root / relative).write_bytes(b"")
        return root

    def test_an_archive_with_every_notice_lacks_none(self) -> None:
        smoke = load_smoke()
        cases = (
            ("basemind", "LICENSE", "ATTRIBUTIONS.md", "THIRD_PARTY_LICENSES"),
            (
                "basemind",
                "LICENSE",
                "ATTRIBUTIONS.md",
                "THIRD_PARTY_LICENSES",
                "lib/libstdc++.so.6",
                "licenses/libstdc++.so.6.txt",
            ),
            ("basemind.exe", "LICENSE", "ATTRIBUTIONS.md", "THIRD_PARTY_LICENSES", "x.dll", "licenses/x.dll.txt"),
        )
        for files in cases:
            with self.subTest(files=files), tempfile.TemporaryDirectory() as root:
                self.assertEqual(smoke.missing_notices(self.unpacked(Path(root), *files)), [])

    def test_missing_notices_are_named_in_order(self) -> None:
        smoke = load_smoke()
        cases = (
            (("basemind", "LICENSE", "ATTRIBUTIONS.md"), ["THIRD_PARTY_LICENSES"]),
            (
                ("basemind", "THIRD_PARTY_LICENSES", "lib/libstdc++.so.6", "lib/libz.so.1", "licenses/libz.so.1.txt"),
                ["LICENSE", "ATTRIBUTIONS.md", "licenses/libstdc++.so.6.txt"],
            ),
            (
                ("basemind.exe", "LICENSE", "ATTRIBUTIONS.md", "THIRD_PARTY_LICENSES", "x.dll"),
                ["licenses/x.dll.txt"],
            ),
        )
        for files, missing in cases:
            with self.subTest(files=files), tempfile.TemporaryDirectory() as root:
                self.assertEqual(smoke.missing_notices(self.unpacked(Path(root), *files)), missing)


if __name__ == "__main__":
    unittest.main()
