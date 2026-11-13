import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


class LayeredOverlapTest(unittest.TestCase):
    def run_comparison(
        self, count: int = 200, replacements: int = 0, route: str = "layered"
    ) -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryDirectory() as directory:
            paths = [Path(directory) / name for name in ("standalone", "layered")]
            for path, observed in zip(paths, ("standalone", route), strict=True):
                rows = []
                for tier in ("code", "documents"):
                    for index in range(count):
                        start = replacements if path == paths[1] else 0
                        rows.append(
                            json.dumps(
                                {
                                    "tier": tier,
                                    "query": f"query {index}",
                                    "route": observed,
                                    "degraded": False,
                                    "hits": [str(hit) for hit in range(start, start + 10)],
                                }
                            )
                        )
                path.write_text("\n".join(rows), encoding="utf-8")
            return subprocess.run(
                [sys.executable, str(Path(__file__).with_name("layered_overlap.py")), *paths],
                capture_output=True,
                text=True,
                check=False,
            )

    def test_complete_identical_results_pass(self) -> None:
        result = self.run_comparison()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["tiers"]["code"], {"queries": 200, "mean_overlap": 1.0})

    def test_changed_hits_fail_threshold(self) -> None:
        result = self.run_comparison(replacements=1)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(json.loads(result.stdout)["tiers"]["documents"]["mean_overlap"], 0.9)

    def test_missing_queries_and_fallback_fail_closed(self) -> None:
        self.assertEqual(self.run_comparison(count=199).returncode, 2)
        self.assertEqual(self.run_comparison(route="standalone").returncode, 2)
