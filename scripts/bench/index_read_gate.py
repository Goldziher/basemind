#!/usr/bin/env python3
"""Run benches/index_read interleaved before/after and judge the ADR-0028 p95 gate.

Runs the two bench binaries in the order before, after, before, after, before (``--rounds`` sets
how many after runs), each in full mode with fresh isolated state, then writes ``gate.md``: per
version count and operation, the p95 of every run, the spread of the before runs (noise) and the
larger after run against the larger before run. The gate (ADR-0028 consequence: <= 10 % at p95 at
three content versions) is judged on that last column for the 3x rows; a cell whose before runs
spread more than the gate itself is reported as not judgeable rather than as a pass or a failure.

Usage: index_read_gate.py --before BIN --after BIN --out DIR [--rounds N] [-- extra bench args]
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

GATE = 0.10
GATE_VERSIONS = 3


def run(binary: Path, label: str, out: Path, extra: list[str]) -> dict:
    report = out / f"{label}.json"
    with tempfile.TemporaryDirectory(prefix="index-read-") as state:
        env = dict(os.environ)
        env["BASEMIND_DATA_HOME"] = str(Path(state) / "data")
        env["BASEMIND_COMMS_DIR"] = str(Path(state) / "comms")
        with (out / f"{label}.log").open("w", encoding="utf-8") as log:
            subprocess.run(
                [str(binary), "--bench", "--out", str(report), *extra],
                stdout=log,
                stderr=subprocess.STDOUT,
                env=env,
                check=True,
            )
    return json.loads(report.read_text(encoding="utf-8"))


def p95_cells(report: dict) -> dict[tuple[int, str], float]:
    return {(group["versions"], op["op"]): op["p95_us"] for group in report["groups"] for op in group["ops"]}


def verdict(befores: list[float], afters: list[float]) -> tuple[float, float, str]:
    worst_before, best_before = max(befores), min(befores)
    noise = (worst_before - best_before) / best_before if best_before else 0.0
    delta = (max(afters) - worst_before) / worst_before if worst_before else 0.0
    if delta <= GATE:
        return noise, delta, "pass"
    if noise > GATE:
        return noise, delta, "noisy"
    return noise, delta, "FAIL"


def table(labels: list[str], cells: list[dict[tuple[int, str], float]]) -> tuple[str, bool]:
    keys = sorted(set().union(*cells))
    header = "| versions | operation | " + " | ".join(labels) + " | before noise | after vs max(before) | gate |"
    lines = [header, "|" + "---|" * (len(labels) + 5)]
    failed = False
    for key in keys:
        values = [cell.get(key) for cell in cells]
        befores = [v for label, v in zip(labels, values, strict=True) if label.startswith("before") and v is not None]
        afters = [v for label, v in zip(labels, values, strict=True) if label.startswith("after") and v is not None]
        noise, delta, mark = verdict(befores, afters) if befores and afters else (0.0, 0.0, "n/a")
        if key[0] != GATE_VERSIONS:
            mark = "-"
        failed |= mark == "FAIL"
        shown = " | ".join("n/a" if v is None else f"{v:.1f}" for v in values)
        lines.append(f"| {key[0]}x | {key[1]} | {shown} | {noise:+.0%} | {delta:+.0%} | {mark} |")
    return "\n".join(lines) + "\n", failed


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--before", type=Path, required=True)
    parser.add_argument("--after", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--rounds", type=int, default=2, help="after runs; before runs are one more")
    parser.add_argument("extra", nargs="*", help="arguments passed to both bench binaries")
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)

    order = ["before-1"]
    for round_ in range(1, args.rounds + 1):
        order += [f"after-{round_}", f"before-{round_ + 1}"]
    cells = []
    for label in order:
        binary = args.before if label.startswith("before") else args.after
        sys.stdout.write(f"running {label}\n")
        sys.stdout.flush()
        cells.append(p95_cells(run(binary, label, args.out, args.extra)))

    body, failed = table(order, cells)
    text = (
        "# index_read p95 (µs), interleaved\n\n"
        f"Gate: after vs max(before) <= {GATE:.0%} at {GATE_VERSIONS}x. 'noisy' marks a cell over the gate "
        "whose before runs spread more than the gate, so it cannot be judged.\n\n" + body
    )
    (args.out / "gate.md").write_text(text, encoding="utf-8")
    sys.stdout.write(text)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
