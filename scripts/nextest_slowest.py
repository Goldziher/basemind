"""Print the slowest tests of a nextest JUnit report as a Markdown table.

Usage:
    python3 scripts/nextest_slowest.py <junit.xml> [--top N]

CI appends the table to the job summary of each test leg, so a leg that grows slower names the tests
that grew. Rows are ordered by duration, longest first, then by test name.
"""

from __future__ import annotations

import argparse
import sys
import xml.etree.ElementTree as ET
from pathlib import Path

DEFAULT_TOP = 20


def slowest(report: Path, top: int) -> list[tuple[float, str]]:
    """Return the ``top`` longest test cases in ``report`` as (seconds, "binary test") pairs."""
    cases = [
        (float(case.get("time", "0")), f"{case.get('classname', '')} {case.get('name', '')}".strip())
        for case in ET.parse(report).getroot().iter("testcase")
    ]
    return sorted(cases, key=lambda item: (-item[0], item[1]))[:top]


def table(rows: list[tuple[float, str]]) -> str:
    """Render ``rows`` as a Markdown table headed "Slowest tests"."""
    lines = [f"### Slowest {len(rows)} tests", "", "| Seconds | Test |", "| ---: | --- |"]
    lines += [f"| {seconds:.1f} | `{name}` |" for seconds, name in rows]
    return "\n".join(lines) + "\n"


def main(argv: list[str]) -> int:
    """Print the table for the report named in ``argv``; fail when the report has no test case."""
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("report", type=Path)
    parser.add_argument("--top", type=int, default=DEFAULT_TOP)
    args = parser.parse_args(argv)
    rows = slowest(args.report, args.top)
    if not rows:
        print(f"{args.report}: no test cases", file=sys.stderr)
        return 1
    sys.stdout.write(table(rows))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
