#!/usr/bin/env python3
"""Render one CI leg's hardening results as a markdown table (rule, value, threshold, verdict).

Reads what the other scripts leave in their `--out` directories: the selftest log, `sweep.json`,
the soak verdict JSON written by `analyze.py --json`, and `leaks.json`. A missing input is a
`not run` row, never a silent omission. Appends to `$GITHUB_STEP_SUMMARY` when it is set.

Usage:
    ci_summary.py --title "ubuntu-latest / full" --reports DIR [--leaks-skipped REASON]
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
from pathlib import Path
from typing import Any

Row = tuple[str, str, str, str]


def cell(text: object) -> str:
    return str(text).replace("|", "/").replace("\n", " ")


def verdict(ok: bool | None) -> str:
    return "not run" if ok is None else "pass" if ok else "**FAIL**"


def read_json(path: Path) -> Any:  # noqa: ANN401 - parsed JSON
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None


def selftest_rows(reports: Path) -> list[Row]:
    try:
        text = (reports / "selftest.log").read_text(encoding="utf-8")
    except OSError:
        return [("selftest: analyze.py rules fire", "-", "every case as expected", verdict(None))]
    m = re.search(r"(\d+)/(\d+) cases behaved as expected", text)
    if not m:
        return [("selftest: analyze.py rules fire", "no tally in the log", "every case as expected", verdict(False))]
    good, total = int(m.group(1)), int(m.group(2))
    return [("selftest: analyze.py rules fire", f"{good}/{total} cases", f"{total}/{total}", verdict(good == total))]


def sweep_rows(reports: Path) -> list[Row]:
    data = read_json(reports / "sweep" / "sweep.json")
    if data is None:
        return [("functional sweep", "-", "0 failures", verdict(None))]
    value = f"{data['passed']}/{data['checks']} passed, {data['skipped']} skipped, {len(data['failed'])} failed"
    rows = [("functional sweep", value, "0 failures, every tool called", verdict(data["pass"]))]
    rows += [("sweep failure", f[:200], "-", "**FAIL**") for f in data["failed"][:10]]
    if data.get("uncovered"):
        rows.append(("sweep: tools not exercised", ", ".join(data["uncovered"]), "none", "**FAIL**"))
    return rows


def soak_rows(reports: Path) -> list[Row]:
    data = read_json(reports / "soak" / "verdict.json")
    if data is None:
        return [("soak (analyze.py)", "-", "every rule passes", verdict(None))]
    rows = []
    for r in data["rules"]:
        value = r.get("value") or r["detail"][:160]
        rows.append((f"soak {r['name']}", value, r.get("threshold") or "-", verdict(r["passed"])))
    return rows


def leak_rows(reports: Path, skipped: str | None) -> list[Row]:
    if skipped:
        return [("one-shot leak check", f"skipped: {skipped}", "0 definite leaks", "skipped")]
    data = read_json(reports / "leaks" / "leaks.json")
    if data is None:
        return [("one-shot leak check", "-", "0 definite leaks", verdict(None))]
    rows = []
    for r in data["runs"]:
        ok = r["inspected"] and r["leaks"] == 0
        value = f"{r['leaks']} leaks, {r['bytes']} bytes" if r["inspected"] else "checker could not inspect"
        rows.append((f"leaks ({data['tool']}) {r['label']}", value, "0 definite leaks", verdict(ok)))
    return rows


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--title", required=True)
    ap.add_argument("--reports", type=Path, required=True)
    ap.add_argument("--leaks-skipped", help="why the leak check did not run on this leg")
    args = ap.parse_args()
    rows = selftest_rows(args.reports) + sweep_rows(args.reports) + soak_rows(args.reports)
    rows += leak_rows(args.reports, args.leaks_skipped)
    lines = [f"## Hardening: {args.title}", "", "| rule | value | threshold | verdict |", "|---|---|---|---|"]
    lines += [f"| {cell(a)} | {cell(b)} | {cell(c)} | {d} |" for a, b, c, d in rows]
    text = "\n".join(lines) + "\n\n"
    sys.stdout.write(text)
    target = os.environ.get("GITHUB_STEP_SUMMARY")
    if target:
        with Path(target).open("a", encoding="utf-8") as fh:
            fh.write(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
