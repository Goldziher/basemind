import json
import sys
import tempfile
import unittest
from pathlib import Path

from capture_layered_overlap import capture

FAKE = """
import json, os, sys
tier = 'code' if 'semantic' in sys.argv else 'documents'
target = 'helpers_code_search' if tier == 'code' else 'memory'
if f'basemind::mcp::{target}=debug' not in os.environ['RUST_LOG'].split(','):
    raise SystemExit('route target is filtered')
route = 'layered' if os.environ['BASEMIND_INDEX_LAYERED'] == '1' else 'standalone'
fallback = sys.argv[-1] == 'fallback'
if fallback:
    route = 'standalone'
print(f'semantic search route tier="{tier}" route="{route}" degraded={str(fallback).lower()}', file=sys.stderr)
key = 'chunk_id' if tier == 'code' else 'chunk_idx'
hits = [{'path': 'fixture', key: str(i) if tier == 'code' else i} for i in range(10)]
print(json.dumps({'hits': hits}))
"""


class CaptureTest(unittest.TestCase):
    def test_actual_child_output_pairs_routes_and_rejects_fallback(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            cache = Path(directory)
            fake = cache / "fake.py"
            fake.write_text(FAKE, encoding="utf-8")
            for tier in ("code", "documents"):
                for route in ("standalone", "layered"):
                    row = capture([sys.executable, fake], cache, tier, "fixture", route)
                    self.assertEqual((row["tier"], row["route"], len(row["hits"])), (tier, route, 10))
                    self.assertEqual(json.loads(row["hits"][0])[0], "fixture")
            with self.assertRaisesRegex(ValueError, "route evidence"):
                capture([sys.executable, fake], cache, "code", "fallback", "layered")
