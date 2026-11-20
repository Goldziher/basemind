"""The #164 waiver of the daemon's memory rules in the soak analysis (scripts/hardening/analyze.py).

Until lsm-tree evicts the blocks of deleted tables, the daemon's Fjall block caches fill with dead
blocks up to their capacity. R2 and R2f waive a daemon climb that stays within that capacity over
the judged window and fail one above it; `--lsm-cache-mib 0` lifts the waiver.
"""

import random
import sys
import unittest
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "hardening"))

import analyze  # noqa: E402
import selftest_analyze as synth  # noqa: E402


def judge(records: list[dict[str, Any]], *flags: str) -> dict[str, analyze.Rule]:
    args = analyze.build_parser().parse_args(["soak.ndjson", *flags])
    return {r.name: r for r in analyze.analyze(records, args).rules}


def daemon_climb(mib_per_min: float, load: float = synth.LOAD) -> list[dict[str, Any]]:
    return synth.climb(synth.healthy(random.Random(11), load), synth.DAEMON, mib_per_min)


DAEMON_FOOTPRINT = f"R2 daemon {synth.DAEMON} memory (footprint)"
DAEMON_RSS = f"R2 daemon {synth.DAEMON} memory (rss)"
DAEMON_FLOOR = f"R2f daemon {synth.DAEMON} memory floor (footprint)"


class WaiverBoundTest(unittest.TestCase):
    def test_the_bound_is_the_block_cache_capacity_spread_over_the_window(self) -> None:
        args = analyze.build_parser().parse_args(["soak.ndjson"])
        self.assertEqual(args.lsm_cache_mib, 296)
        self.assertAlmostEqual(analyze.lsm_waiver_bound(10.0, args), 29.6)

    def test_a_lifted_waiver_has_no_bound(self) -> None:
        args = analyze.build_parser().parse_args(["soak.ndjson", "--lsm-cache-mib", "0"])
        self.assertEqual(analyze.lsm_waiver_bound(10.0, args), 0.0)


class DaemonSlopeWaiverTest(unittest.TestCase):
    def test_a_climb_within_the_block_cache_bound_is_waived(self) -> None:
        rules = judge(daemon_climb(20.0))
        for name in (DAEMON_FOOTPRINT, DAEMON_RSS):
            self.assertTrue(rules[name].passed, rules[name].detail)
            self.assertTrue(rules[name].waived, rules[name].detail)
            self.assertIn("waived until #164", rules[name].detail)

    def test_a_climb_above_the_block_cache_bound_fails(self) -> None:
        rules = judge(daemon_climb(60.0))
        for name in (DAEMON_FOOTPRINT, DAEMON_RSS):
            self.assertFalse(rules[name].passed, rules[name].detail)
            self.assertFalse(rules[name].waived)
            self.assertIn("over the #164 waiver bound", rules[name].detail)

    def test_a_climb_within_the_bound_fails_once_the_waiver_is_lifted(self) -> None:
        rules = judge(daemon_climb(20.0), "--lsm-cache-mib", "0")
        self.assertFalse(rules[DAEMON_FOOTPRINT].passed)
        self.assertFalse(rules[DAEMON_FOOTPRINT].waived)

    def test_a_flat_daemon_passes_without_a_waiver(self) -> None:
        rules = judge(daemon_climb(0.0))
        self.assertTrue(rules[DAEMON_FOOTPRINT].passed)
        self.assertFalse(rules[DAEMON_FOOTPRINT].waived)

    def test_a_climbing_serve_is_not_waived(self) -> None:
        records = synth.climb(synth.healthy(random.Random(11)), synth.MAIN, 3.0)
        rule = judge(records)[f"R2 main serve {synth.MAIN} memory (footprint)"]
        self.assertFalse(rule.passed, rule.detail)
        self.assertFalse(rule.waived)


class DaemonFloorWaiverTest(unittest.TestCase):
    def test_a_rising_floor_within_the_bound_is_waived(self) -> None:
        rule = judge(daemon_climb(0.5, synth.LONG_LOAD))[DAEMON_FLOOR]
        self.assertTrue(rule.passed, rule.detail)
        self.assertTrue(rule.waived, rule.detail)

    def test_a_rising_floor_fails_once_the_waiver_is_lifted(self) -> None:
        rule = judge(daemon_climb(0.5, synth.LONG_LOAD), "--lsm-cache-mib", "0")[DAEMON_FLOOR]
        self.assertFalse(rule.passed, rule.detail)

    def test_a_floor_rising_above_the_bound_fails(self) -> None:
        rule = judge(daemon_climb(8.0, synth.LONG_LOAD))[DAEMON_FLOOR]
        self.assertFalse(rule.passed, rule.detail)
        self.assertFalse(rule.waived)


class ReportTest(unittest.TestCase):
    def test_a_waived_rule_reads_waived_in_the_report(self) -> None:
        records = daemon_climb(20.0)
        args = analyze.build_parser().parse_args(["soak.ndjson"])
        report = analyze.render(analyze.analyze(records, args), records, Path("soak.ndjson"))
        self.assertIn(f"| {DAEMON_FOOTPRINT} | waived |", report)


if __name__ == "__main__":
    unittest.main()
