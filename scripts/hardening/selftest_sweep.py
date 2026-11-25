#!/usr/bin/env python3
"""Prove functional_sweep.py's matchers and verdict: each runs on answers in the shipped shapes,
one that must pass and one that must fail, so a matcher that misreads a shape is caught whether it
then never matches or always does.

Usage:
    selftest_sweep.py
"""

from __future__ import annotations

import sys
from collections.abc import Callable
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from functional_sweep import (  # noqa: E402
    Outcome,
    any_row_mentions,
    file_listed,
    items,
    outline_has,
    sweep_passes,
)
from wire_decode import decode  # noqa: E402

GREP_LITERAL = {
    "hits": {
        "src/lib.rs": {
            "total_reading": ["20: pub fn total_reading(gauges: &[&dyn Gauge]) -> u32 {"],
            "compute_total": ["28:     total_reading(&[&a, &b])"],
        }
    }
}
GREP_REGEX = {
    "hits": {
        "src/settings.py": {
            "load_settings": ["7: def load_settings(path):"],
            "settings_summary": ["12: def settings_summary(settings):"],
        }
    }
}
GREP_OUTSIDE_SYMBOLS = {"hits": {"src/lib.rs": {"": ["1: // total_reading"]}}}
REFS = {"hits": {"src/lib.rs": ["28:5 compute_total"]}}

GREP_TEXT = (
    "total: 2\n# src/lib.rs\n## total_reading\n20: pub fn total_reading()\n## compute_total\n28: total_reading(&[])"
)
OUTLINE_TEXT = "path: src/lib.rs\nsymbols[2]{name,kind,line}:\n  compute_total,function,27\n  total_reading,function,20"

PASS = [Outcome("a", "mcp", "pass")]
SKIP = [*PASS, Outcome("b", "mcp", "skip")]
FAIL = [*PASS, Outcome("c", "cli", "fail")]

CASES: list[tuple[str, Callable[[], object], object]] = [
    (
        "a grep text answer decodes to grouped hits",
        lambda: decode(GREP_TEXT),
        {
            "total": 2,
            "hits": {
                "src/lib.rs": {
                    "total_reading": ["20: pub fn total_reading()"],
                    "compute_total": ["28: total_reading(&[])"],
                }
            },
        },
    ),
    (
        "a decoded grep text answer passes the literal matcher",
        lambda: any_row_mentions(("hits",), "total_reading", 2)(decode(GREP_TEXT)),
        None,
    ),
    (
        "a TOON outline decodes to symbol rows",
        lambda: decode(OUTLINE_TEXT)["symbols"][0],
        {"name": "compute_total", "kind": "function", "line": 27},
    ),
    (
        "a decoded TOON outline passes the outline matcher",
        lambda: outline_has("compute_total")(decode(OUTLINE_TEXT)),
        None,
    ),
    (
        "a TOON find_files list decodes",
        lambda: decode("files[2]: src/lib.rs,src/settings.py"),
        {"files": ["src/lib.rs", "src/settings.py"]},
    ),
    (
        "grep rows flattened with path and symbol",
        lambda: items(GREP_LITERAL, "hits"),
        [
            {
                "path": "src/lib.rs",
                "symbol": "total_reading",
                "row": "20: pub fn total_reading(gauges: &[&dyn Gauge]) -> u32 {",
            },
            {"path": "src/lib.rs", "symbol": "compute_total", "row": "28:     total_reading(&[&a, &b])"},
        ],
    ),
    ("grep literal counts both rows", lambda: any_row_mentions(("hits",), "total_reading", 2)(GREP_LITERAL), None),
    (
        "grep literal below its floor",
        lambda: any_row_mentions(("hits",), "total_reading", 3)(GREP_LITERAL) is None,
        False,
    ),
    ("grep regex rows carry their path", lambda: any_row_mentions(("hits",), "settings.py", 2)(GREP_REGEX), None),
    ("grep regex wrong file", lambda: any_row_mentions(("hits",), "lib.rs")(GREP_REGEX) is None, False),
    (
        "grep rows outside every symbol",
        lambda: items(GREP_OUTSIDE_SYMBOLS, "hits"),
        [{"path": "src/lib.rs", "symbol": "", "row": "1: // total_reading"}],
    ),
    ("refs rows grouped by file", lambda: items(REFS, "hits"), [{"path": "src/lib.rs", "row": "28:5 compute_total"}]),
    ("a map of counts is not rows", lambda: items({"files": {"src/lib.rs": 40}}, "files"), []),
    ("a listed path", lambda: file_listed("src/lib.rs")({"files": ["src/lib.rs", "src/settings.py"]}), None),
    ("an unlisted path", lambda: file_listed("src/main.rs")({"files": ["src/lib.rs"]}) is None, False),
    (
        "an outline symbol with its line",
        lambda: outline_has("compute_total")({"symbols": [{"name": "compute_total", "line": 27}]}),
        None,
    ),
    ("a passing run", lambda: sweep_passes(PASS, [], "comms"), True),
    ("a skip outside full", lambda: sweep_passes(SKIP, [], "comms"), True),
    ("a skip in full", lambda: sweep_passes(SKIP, [], "full"), False),
    ("a skip in full with another feature", lambda: sweep_passes(SKIP, [], "full,system-allocator"), False),
    ("a failure", lambda: sweep_passes(FAIL, [], "comms"), False),
    ("an uncovered tool", lambda: sweep_passes(PASS, ["code_new"], "comms"), False),
    ("a run that checked nothing", lambda: sweep_passes([Outcome("b", "mcp", "skip")], [], "comms"), False),
]


def main() -> int:
    failures = 0
    for name, run, want in CASES:
        got = run()
        good = got == want
        failures += 0 if good else 1
        detail = "" if good else f": got {got!r}, want {want!r}"
        sys.stdout.write(f"{'ok  ' if good else 'BAD '} {name}{detail}\n")
    sys.stdout.write(f"\n{len(CASES) - failures}/{len(CASES)} cases behaved as expected\n")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
