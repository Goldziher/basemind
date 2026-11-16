"""Tests for `fetch_corpus.sh`: a pin or tree that moved fails the fetch."""

import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).parent


def git(repo: Path, *args: str) -> str:
    env = {
        **os.environ,
        "GIT_AUTHOR_NAME": "t",
        "GIT_AUTHOR_EMAIL": "t@e.x",
        "GIT_COMMITTER_NAME": "t",
        "GIT_COMMITTER_EMAIL": "t@e.x",
    }
    return subprocess.run(["git", "-C", str(repo), *args], check=True, capture_output=True, text=True, env=env).stdout


class FetchCorpus(unittest.TestCase):
    def test_a_moved_pin_or_tree_fails_the_fetch(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            src = tmp / "src"
            src.mkdir()
            git(src, "init", "-q")
            git(src, "config", "commit.gpgsign", "false")
            (src / "f.txt").write_text("x\n")
            git(src, "add", "-A")
            git(src, "commit", "-qm", "one")
            sha = git(src, "rev-parse", "HEAD").strip()
            tools = tmp / "tools"
            tools.mkdir()
            shutil.copy(HERE / "fetch_corpus.sh", tools / "fetch_corpus.sh")

            def run(sum_: str, files: str) -> subprocess.CompletedProcess:
                (tools / "corpus.tsv").write_text(f"demo\t{src}\t{sha}\t{sum_}\t{files}\n")
                return subprocess.run(
                    ["bash", str(tools / "fetch_corpus.sh"), str(tmp / "corpus")], capture_output=True, text=True
                )

            (tools / "corpus.tsv").write_text(f"demo\t{src}\t{sha}\t0\t0\n")
            printed = subprocess.run(
                ["bash", str(tools / "fetch_corpus.sh"), "--print", str(tmp / "corpus")],
                capture_output=True,
                text=True,
                check=True,
            )
            _, _, got_sha, got_sum, got_files = printed.stdout.strip().split("\t")
            self.assertEqual((got_sha, got_files), (sha, "1"))
            self.assertEqual(run(got_sum, got_files).returncode, 0)
            bad = run("0" * 64, got_files)
            self.assertEqual(bad.returncode, 1)
            self.assertIn("pin mismatch", bad.stderr)


if __name__ == "__main__":
    unittest.main()
