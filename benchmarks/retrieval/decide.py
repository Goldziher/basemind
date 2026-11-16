#!/usr/bin/env python3
"""Apply the backend decision rule (ADR-0029 rule 12) to retrieval_bench NDJSON and print a verdict.

    decide.py results.ndjson [more.ndjson ...] [--markdown table.md] [--tiers 50000,250000]

Later files override earlier lines for the same measurement, so a tuning or deliberately degraded
run can be judged against the committed baseline without editing it. Arm A (LanceDB as it is
today) is the reference every number is reported against. Arm B (LanceDB with scalar and full-text
indexes) is judged; arm C (Fjall plus flat vector files) is measured only when arm B fails a row,
and judged when present. The 1,000,000 tier is published and does not decide. Exit status: 0 when
arm B passes every deciding tier, 1 when it fails a check, 2 when a deciding tier has measurements
missing.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from collections.abc import Callable

# The table of ADR-0029 rule 12. Latencies in ms, build in seconds. The 1,000,000 row is the design
# report's proposal; that tier is shown against it and never decides.
THRESHOLDS = {
    50_000: {"quality": 0.99, "vector_p95": 50, "lexical_p95": 30, "keys": 10, "cold": 1000, "build": 30},
    250_000: {"quality": 0.97, "vector_p95": 100, "lexical_p95": 60, "keys": 25, "cold": 1500, "build": 180},
    1_000_000: {"quality": 0.95, "vector_p95": 250, "lexical_p95": 120, "keys": 60, "cold": 2500, "build": 900},
}
DECIDING = (50_000, 250_000)
LIVE = (100, 90, 50)
KEY_LISTS = (2_000, 20_000)
MAX_SIZE_RATIO = 1.6
APPEND_P95_RATIO = 1.5
APPEND_RECALL_SLACK = 0.005  # "recall unchanged": each state is scored against its own truth
SELECTIVITIES = (1, 10, 100)
JUDGED = ("B", "C")
ARMS = ("A", "B", "C")


def load(paths: list[str]) -> dict[tuple, dict]:
    rows: dict[tuple, dict] = {}
    for path in paths:
        for number, line in enumerate(Path(path).read_text(encoding="utf-8").splitlines(), 1):
            if not line.strip():
                continue
            try:
                row = json.loads(line)
            except json.JSONDecodeError as error:
                sys.exit(f"{path}:{number}: not JSON: {error}")
            kind = row.get("kind")
            if kind == "query":
                key = (
                    row["arm"],
                    row["n"],
                    "query",
                    row["lane"],
                    row["selectivity"],
                    row["live"],
                    row["state"],
                    row.get("key_list", 0),
                )
            elif kind == "build":
                key = (row["arm"], row["n"], "build", row["stage"])
            elif kind == "rss":
                key = (row["arm"], row["n"], "rss", row["step"])
            else:
                continue
            rows[key] = row
    return rows


class Tier:
    def __init__(self, rows: dict[tuple, dict], n: int) -> None:
        self.rows, self.n, self.limits = rows, n, THRESHOLDS[n]
        self.checks: list[dict] = []

    def query(
        self, arm: str, lane: str, sel: int, state: str = "base", live: int = 100, key_list: int = 0
    ) -> dict | None:
        return self.rows.get((arm, self.n, "query", lane, sel, live, state, key_list))

    def build(self, arm: str, stage: str) -> dict | None:
        return self.rows.get((arm, self.n, "build", stage))

    def add(self, metric: str, limit: str, values: dict[str, float | None], ok: Callable[[float], bool]) -> None:
        """Record one table row: a value per arm and, for judged arms, whether it meets the rule."""
        status = {}
        for arm in JUDGED:
            value = values.get(arm)
            status[arm] = "missing" if value is None else ("ok" if ok(value) else "FAIL")
        self.checks.append({"metric": metric, "limit": limit, "values": values, "status": status})

    def field(
        self, lane: str, sel: int, name: str, state: str = "base", live: int = 100, key_list: int = 0
    ) -> dict[str, float | None]:
        out = {}
        for arm in ARMS:
            row = self.query(arm, lane, sel, state, live, key_list)
            out[arm] = None if row is None else row.get(name)
        return out

    def evaluate(self) -> None:
        lim = self.limits
        floor = lim["quality"]
        combos = [(sel, live) for live in LIVE for sel in SELECTIVITIES]
        for name, label in (("recall10", "recall@10"), ("ndcg10", "nDCG@10")):
            for sel, live in combos:
                self.add(
                    f"{label}, {sel}% filter, {live}% live",
                    f">= {floor}",
                    self.field("vector", sel, name, live=live),
                    lambda v: v >= floor,
                )
        for lane in ("vector", "lexical"):
            limit = lim[f"{lane}_p95"]
            for sel, live in combos:
                self.add(
                    f"{lane} p95 ms, {sel}% filter, {live}% live",
                    f"<= {limit}",
                    self.field(lane, sel, "p95_ms", live=live),
                    lambda v, m=limit: v <= m,
                )
        # What a `doc_key IN (...)` list of that many keys adds to an unfiltered query.
        for lane in ("vector", "lexical"):
            plain = self.field(lane, 100, "p95_ms")
            for keys in KEY_LISTS:
                listed = self.field(lane, 100, "p95_ms", key_list=keys)
                added = {
                    arm: None if None in (plain[arm], listed[arm]) else max(0.0, listed[arm] - plain[arm])
                    for arm in ARMS
                }
                self.add(
                    f"{lane} p95 ms added by a {keys:,}-key list",
                    f"<= {lim['keys']}",
                    added,
                    lambda v: v <= lim["keys"],
                )

        cold = {}
        for arm in ARMS:
            row = self.query(arm, "vector", 100)
            cold[arm] = None if row is None else row["cold_open_ms"] + row["cold_first_ms"]
        self.add("cold open + first query ms", f"<= {lim['cold']}", cold, lambda v: v <= lim["cold"])

        # Arm C writes its postings and SQ8 codes while ingesting, so its index cost is what its
        # ingest takes beyond arm A's plain ingest, plus its own index stage.
        ingest = {arm: self.build(arm, "ingest") for arm in ARMS}
        index = {arm: self.build(arm, "index") for arm in ARMS}
        build_s: dict[str, float | None] = {arm: (index[arm] or {}).get("seconds") for arm in ARMS}
        if build_s["C"] is not None and ingest["C"] is not None:
            reference = ingest["A"]["seconds"] if ingest["A"] else 0.0
            build_s["C"] += max(0.0, ingest["C"]["seconds"] - reference)
        self.add("index build s", f"<= {lim['build']}", build_s, lambda v: v <= lim["build"])

        size = {}
        for arm in ARMS:
            row = index[arm]
            size[arm] = None if row is None else row["disk_bytes"] / row["raw_vector_bytes"]
        self.add("on-disk size / raw f32 bytes", f"<= {MAX_SIZE_RATIO}", size, lambda v: v <= MAX_SIZE_RATIO)

        base_recall = self.field("vector", 100, "recall10")
        appended_recall = self.field("vector", 100, "recall10", "appended")
        drop = {
            arm: None if None in (base_recall[arm], appended_recall[arm]) else base_recall[arm] - appended_recall[arm]
            for arm in ARMS
        }
        self.add(
            "recall@10 lost after 1% append",
            f"<= {APPEND_RECALL_SLACK}",
            drop,
            lambda v: v <= APPEND_RECALL_SLACK,
        )
        for lane in ("vector", "lexical"):
            base, appended = self.field(lane, 100, "p95_ms"), self.field(lane, 100, "p95_ms", "appended")
            ratio = {
                arm: None if None in (base[arm], appended[arm]) or not base[arm] else appended[arm] / base[arm]
                for arm in ARMS
            }
            self.add(
                f"{lane} p95 after 1% append / before",
                f"<= {APPEND_P95_RATIO}",
                ratio,
                lambda v: v <= APPEND_P95_RATIO,
            )

    def failures(self, arm: str) -> set[str]:
        return {c["metric"] for c in self.checks if c["status"][arm] == "FAIL"}

    def missing(self, arm: str) -> list[str]:
        return [c["metric"] for c in self.checks if c["status"][arm] == "missing"]

    def informational(self) -> list[tuple[str, dict[str, str]]]:
        """Measurements the rule does not judge, shown for context."""
        out = []

        def cell(row: dict | None, text: Callable[[dict], str]) -> str:
            return "n/a" if row is None else text(row)

        for live in (90, 50):
            rows = {arm: self.query(arm, "vector", 100, "base", live) for arm in ARMS}
            out.append(
                (
                    f"{live}% live: keys / applied as / queries short at the cap",
                    {
                        arm: cell(r, lambda r: f"{r['live_keys']:,} / {r['live_mode']} / {r['liveness_short']}")
                        for arm, r in rows.items()
                    },
                )
            )
        for sel in SELECTIVITIES:
            rows = {arm: self.query(arm, "lexical", sel) for arm in ARMS}
            out.append(
                (
                    f"lexical recall@10 vs brute-force BM25, {sel}%",
                    {arm: cell(r, lambda r: f"{r['recall10']:.3f}") for arm, r in rows.items()},
                )
            )
        for sel in SELECTIVITIES:
            rows = {arm: self.query(arm, "vector", sel) for arm in ARMS}
            out.append(
                (
                    f"vector queries answered exact / ANN, {sel}%",
                    {arm: cell(r, lambda r: f"{r['exact_queries']} / {r['ann_queries']}") for arm, r in rows.items()},
                )
            )
        for lane in ("vector", "lexical"):
            rows = {arm: self.query(arm, lane, 100, "optimized") for arm in ARMS}
            out.append(
                (
                    f"{lane} p95 ms after optimize",
                    {arm: cell(r, lambda r: f"{r['p95_ms']:.1f}") for arm, r in rows.items()},
                )
            )
        for stage in ("ingest", "index", "append", "optimize"):
            rows = {arm: self.build(arm, stage) for arm in ARMS}
            out.append((f"{stage} s", {arm: cell(r, lambda r: f"{r['seconds']:.1f}") for arm, r in rows.items()}))
            rss = {arm: self.rows.get((arm, self.n, "rss", f"build-{stage}")) for arm in ARMS}
            out.append(
                (
                    f"{stage} peak RSS MiB",
                    {arm: cell(r, lambda r: f"{r['peak_rss_bytes'] / 2**20:.0f}") for arm, r in rss.items()},
                )
            )
        return out


def fmt(value: float | None) -> str:
    if value is None:
        return "n/a"
    return f"{value:.3f}" if abs(value) < 10 else f"{value:.1f}"


def render(tiers: list[Tier], verdict: list[str]) -> str:
    out = ["# Retrieval backend benchmark", ""]
    out += [f"- {line}" for line in verdict]
    for tier in tiers:
        out += [
            "",
            f"## {tier.n:,} chunks",
            "",
            "| Metric | Rule | A (flat) | B (indexed) | C (Fjall) |",
            "|---|---|---|---|---|",
        ]
        for check in tier.checks:
            cells = [fmt(check["values"].get("A"))]
            cells += [f"{fmt(check['values'].get(arm))} {check['status'][arm]}" for arm in JUDGED]
            out.append(f"| {check['metric']} | {check['limit']} | " + " | ".join(cells) + " |")
        out += ["", "Not judged:", "", "| Measurement | A (flat) | B (indexed) | C (Fjall) |", "|---|---|---|---|"]
        for name, cells in tier.informational():
            out.append(f"| {name} | " + " | ".join(cells[arm] for arm in ARMS) + " |")
    return "\n".join(out) + "\n"


def decide(tiers: list[Tier]) -> tuple[list[str], int]:
    lines, status = [], 0
    passing = []
    switch = unmeasured = False
    for tier in tiers:
        b_fail, c_fail = tier.failures("B"), tier.failures("C")
        missing = tier.missing("B")
        c_measured = len(tier.missing("C")) < len(tier.checks)
        if tier.n not in DECIDING:
            outcome = (
                f"fails {len(b_fail)} checks: {', '.join(sorted(b_fail))}" if b_fail else "meets every measured check"
            )
            lines.append(f"{tier.n:,} (published, does not decide): arm B {outcome}")
            continue
        if missing:
            status = max(status, 2)
            lines.append(f"{tier.n:,}: arm B has no measurement for: {', '.join(missing)}")
        if not b_fail and not missing:
            passing.append(tier.n)
            lines.append(f"{tier.n:,}: arm B passes every check")
            continue
        if b_fail:
            status = max(status, 1)
            lines.append(f"{tier.n:,}: arm B FAILS: {', '.join(sorted(b_fail))}")
            if not c_measured:
                unmeasured = True
                continue
            c_passes = sorted(m for m in b_fail if m not in c_fail and m not in tier.missing("C"))
            if c_passes:
                switch = True
                lines.append(f"{tier.n:,}: arm C passes where B fails: {', '.join(c_passes)}")
            if c_fail:
                lines.append(f"{tier.n:,}: arm C FAILS: {', '.join(sorted(c_fail))}")
    if status == 0:
        head = "VERDICT: LanceDB stays, with scalar and full-text indexes on (arm B passes every deciding row)."
    elif status == 2 and not any("FAILS" in line for line in lines):
        head = "VERDICT: incomplete; a deciding tier is missing measurements."
    elif switch:
        head = "VERDICT: arm B fails a row that arm C passes at the same size; a new ADR replaces the backend clause."
    elif unmeasured:
        head = "VERDICT: arm B fails a row; build and measure the Fjall arm (C) before deciding."
    else:
        cap = f"{max(passing):,} chunks" if passing else "below the smallest tier measured"
        head = f"VERDICT: arm B fails a row and arm C does not pass in its place; LanceDB stays and the documented limit is {cap}."
    return [head, *lines], status


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("ndjson", nargs="+")
    p.add_argument("--markdown", help="also write the Markdown table to this path")
    p.add_argument("--tiers", help="comma-separated corpus sizes to judge (default: every size present)")
    a = p.parse_args()

    rows = load(a.ndjson)
    present = sorted({key[1] for key in rows})
    wanted = [int(t) for t in a.tiers.split(",")] if a.tiers else present
    unknown = [n for n in wanted if n not in THRESHOLDS]
    if unknown:
        sys.exit(f"no thresholds for corpus sizes {unknown}; known: {sorted(THRESHOLDS)}")
    if not wanted:
        sys.exit("no measurements found")
    tiers = [Tier(rows, n) for n in wanted]
    for tier in tiers:
        tier.evaluate()
    verdict, status = decide(tiers)
    not_run = [n for n in THRESHOLDS if n not in present]
    if not_run:
        verdict.append("not run: " + ", ".join(f"{n:,}" for n in not_run))
    table = render(tiers, verdict)
    print(table, end="")
    if a.markdown:
        Path(a.markdown).write_text(table, encoding="utf-8")
    sys.exit(status)


if __name__ == "__main__":
    main()
