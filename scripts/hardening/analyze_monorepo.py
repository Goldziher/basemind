#!/usr/bin/env python3
"""Decide pass/fail for a large-monorepo soak NDJSON (monorepo_soak.py) with stated numeric rules.

Usage:
    analyze_monorepo.py monorepo.ndjson [--report report.md] [--json verdict.json]

Exit status: 0 PASS, 1 FAIL, 2 unusable input. The rules and their thresholds are listed in
docs/hardening.md; every threshold is a flag here so a rerun can state what it used. The verdict
JSON has the shape of analyze.py's, so ci_summary.py renders both.
"""

from __future__ import annotations

import argparse
import json
import math
import statistics
import sys
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

from analyze import MIB, Rule, Verdict, growth_rule, load, rule_verdict

SECONDS_PER_HOUR = 3600


def percentile(values: list[float], pct: float) -> float:
    """The nearest-rank `pct` percentile of `values` (0 for none)."""
    if not values:
        return 0.0
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, max(0, math.ceil(pct / 100 * len(ordered)) - 1))]


def of_kind(records: list[dict[str, Any]], kind: str) -> list[dict[str, Any]]:
    return [r for r in records if r.get("kind") == kind]


def rss_rule(v: Verdict, records: list[dict[str, Any]], args: argparse.Namespace) -> None:
    peaks = [r["rss"] for r in of_kind(records, "proc") if r.get("role") == "daemon" and r.get("rss") is not None]
    if not peaks:
        v.add("M1 daemon RSS", False, "no daemon sample: the soak never reached the daemon")
        return
    peak = max(peaks) / MIB
    v.add(
        "M1 daemon RSS",
        peak <= args.rss_mib,
        f"peak {peak:.0f} MiB over {len(peaks)} samples; the daemon is capped at {args.cap_mib:g} MiB",
        f"{peak:.0f} MiB",
        f"<= {args.rss_mib:g} MiB",
    )


def save_rule(v: Verdict, records: list[dict[str, Any]], args: argparse.Namespace) -> None:
    saves = of_kind(records, "save")
    times = [s["ms"] for s in saves if s.get("ok")]
    if len(times) < args.min_saves:
        v.add("M2 per-save cost", False, f"{len(times)} successful saves of {len(saves)}; need {args.min_saves}")
        return
    p95 = percentile(times, 95)
    rewritten = [s.get("rewritten_bytes", {}).get("total", 0) for s in saves if s.get("ok")]
    v.add(
        "M2 per-save cost",
        p95 <= args.save_ms,
        f"p95 {p95:.0f} ms, median {statistics.median(times):.0f} ms, max {max(times):.0f} ms over {len(times)} saves "
        f"with {args.worktrees} sibling views; {statistics.median(rewritten) / MIB:.2f} MiB rewritten per save (median)",
        f"p95 {p95:.0f} ms",
        f"<= {args.save_ms:g} ms",
    )


def worktree_rule(v: Verdict, records: list[dict[str, Any]], args: argparse.Namespace) -> None:
    scan = next((s for s in of_kind(records, "scan") if s.get("ok")), None)
    added = of_kind(records, "worktree")
    if scan is None or not scan.get("data_bytes"):
        v.add("M3 bytes per added worktree", False, "the main checkout's first scan did not finish")
        return
    failed = [w["index"] for w in added if not w.get("ok")]
    if failed or len(added) < args.worktrees:
        v.add(
            "M3 bytes per added worktree",
            False,
            f"{len(added) - len(failed)} of {args.worktrees} worktrees answered (failed: {failed})",
        )
        return
    sizes = [w["added_bytes"] for w in added]
    worst = max(sizes) / scan["data_bytes"] * 100
    v.add(
        "M3 bytes per added worktree",
        worst <= args.worktree_pct,
        f"first scan {scan['data_bytes'] / MIB:.1f} MiB; an added worktree adds {statistics.median(sizes) / MIB:.2f} MiB "
        f"(median), {max(sizes) / MIB:.2f} MiB (max) = {worst:.1f}% of it; {sum(sizes) / MIB:.1f} MiB for {len(added)}",
        f"{worst:.1f}% of the first scan",
        f"<= {args.worktree_pct:g}%",
    )


def disk_rule(v: Verdict, records: list[dict[str, Any]], args: argparse.Namespace) -> None:
    points = [
        (r["t"], float(r["data_bytes"] - sum(r.get("data_journals", {}).values()) - r.get("data_telemetry_bytes", 0)))
        for r in of_kind(records, "dir")
        if r.get("phase") == "load"
    ]
    growth_rule(v, "M4 data dir slope", points, args.dir_tol, "MiB", scale=MIB)


def lock_rule(v: Verdict, records: list[dict[str, Any]], args: argparse.Namespace) -> None:
    calls = [c for c in of_kind(records, "call") if c.get("phase") == "load"]
    status = [c["ms"] for c in calls if c.get("tool") == "index_status" and c.get("ok") and "ms" in c]
    if len(status) < args.min_calls:
        v.add("M5 lock waits", False, f"{len(status)} index_status calls under load; need {args.min_calls}")
        return
    p99 = percentile(status, 99)
    busy = sum(1 for c in calls if "busy" in str(c.get("error", "")).lower())
    busy_pct = busy / len(calls) * 100
    v.add(
        "M5 lock waits",
        p99 <= args.status_p99_ms and busy_pct <= args.busy_pct,
        f"index_status p50 {statistics.median(status):.0f} ms, p99 {p99:.0f} ms, max {max(status):.0f} ms under "
        f"{args.sessions} sessions (a cheap call waits only on a lock or admission); {busy} of {len(calls)} calls "
        f"answered busy ({busy_pct:.2f}%)",
        f"p99 {p99:.0f} ms, {busy_pct:.2f}% busy",
        f"p99 <= {args.status_p99_ms:g} ms, busy <= {args.busy_pct:g}%",
    )


def error_rule(v: Verdict, records: list[dict[str, Any]], args: argparse.Namespace) -> None:
    calls = [c for c in of_kind(records, "call") if c.get("phase") == "load"]
    if not calls:
        v.add("M6 call errors", False, "no call recorded under load")
        return
    bad = [c for c in calls if not c.get("ok")]
    pct = len(bad) / len(calls) * 100
    first = (
        f"; first: {bad[0].get('tool')}: {str(bad[0].get('error', bad[0].get('transport_error', '')))[:120]}"
        if bad
        else ""
    )
    v.add(
        "M6 call errors",
        pct <= args.error_pct,
        f"{len(bad)} of {len(calls)} calls failed ({pct:.2f}%){first}",
        f"{pct:.2f}%",
        f"<= {args.error_pct:g}%",
    )


def staleness_rule(v: Verdict, records: list[dict[str, Any]], args: argparse.Namespace) -> None:
    edits = [s for s in of_kind(records, "save") if "stale_s" in s]
    switches = of_kind(records, "switch")
    if len(edits) < args.min_saves or len(switches) < args.worktrees:
        v.add(
            "M7 answer staleness",
            False,
            f"{len(edits)} edits and {len(switches)} branch switches measured; need {args.min_saves} and {args.worktrees}",
        )
        return
    hidden = [s for s in edits if not s.get("stale_visible")] + [s for s in switches if not s.get("visible")]
    edit_max = max(s["stale_s"] for s in edits)
    switch_max = max(s["visible_s"] for s in switches)
    v.add(
        "M7 answer staleness",
        not hidden and edit_max <= args.edit_stale_s and switch_max <= args.switch_stale_s,
        f"a save is answered after {edit_max:.1f} s at most (p95 {percentile([s['stale_s'] for s in edits], 95):.1f} s); "
        f"{len(switches)} concurrent branch switches are answered after {switch_max:.1f} s at most; "
        f"{len(hidden)} never answered",
        f"save {edit_max:.1f} s, switch {switch_max:.1f} s",
        f"<= {args.edit_stale_s:g} s, <= {args.switch_stale_s:g} s",
    )


def retire_rule(v: Verdict, records: list[dict[str, Any]], args: argparse.Namespace) -> None:
    gc = next(iter(of_kind(records, "gc")), None)
    if gc is None:
        v.add("M8 view retire and sweep", False, "no `cache gc` after removing worktrees was recorded")
        return
    reaped = gc["workspaces_before"] - gc["workspaces_after"]
    v.add(
        "M8 view retire and sweep",
        gc["exit_code"] == 0 and gc["secs"] <= args.gc_s and reaped >= gc["removed"],
        f"`cache gc` exit {gc['exit_code']} in {gc['secs']:.1f} s; {reaped} of {gc['removed']} removed worktrees' "
        f"workspaces reaped ({gc['workspaces_before']} -> {gc['workspaces_after']})",
        f"{gc['secs']:.1f} s, {reaped}/{gc['removed']} reaped",
        f"<= {args.gc_s:g} s, all reaped",
    )


def msgpack_rule(v: Verdict, records: list[dict[str, Any]], args: argparse.Namespace) -> None:
    """M9 judges `index.msgpack` writes per hour against a ceiling, from two bounds. The polled rewrites are
    a lower bound: over the ceiling is a failure. The daemon's own write counter is an upper bound (every
    store it writes): under the ceiling is a pass. Between the two, or with only the lower bound, the write
    volume was not measured and the rule neither passes nor fails. Every figure covers the load window
    only (`seconds` runs from the start of the load, not of the run); the counter bound is absent when
    any daemon's counter could not be read at the end."""
    name = "M9 index.msgpack written"
    watch = next(iter(of_kind(records, "msgpack_total")), None)
    if watch is None or watch["seconds"] <= 0:
        v.add(name, False, "no index.msgpack watcher total recorded")
        return
    per_hour = watch["bytes"] / watch["seconds"] * SECONDS_PER_HOUR / MIB
    upper = watch.get("written_upper")
    upper_per_hour = None if upper is None else upper / watch["seconds"] * SECONDS_PER_HOUR / MIB
    threshold = f"<= {args.msgpack_mib_h:g} MiB/h"
    seen = (
        f"at least {watch['bytes'] / MIB:.1f} MiB in {watch['seconds'] / 60:.1f} min = {per_hour:.0f} MiB/h polled "
        f"({watch['rewrites']} rewrites of {watch['files']} files)"
    )
    if per_hour > args.msgpack_mib_h:
        v.add(name, False, f"{seen}, over the ceiling whatever else was written", f">= {per_hour:.0f} MiB/h", threshold)
        return
    if upper_per_hour is not None and upper_per_hour <= args.msgpack_mib_h:
        v.add(
            name,
            True,
            f"{seen}; the daemon wrote at most {upper / MIB:.1f} MiB in all = {upper_per_hour:.0f} MiB/h",
            f"<= {upper_per_hour:.0f} MiB/h",
            threshold,
        )
        return
    reason = (
        "the host keeps no per-process write counter, so only the polled lower bound exists"
        if upper_per_hour is None
        else f"the daemon wrote {upper_per_hour:.0f} MiB/h in all, too much to tell how much was index.msgpack"
    )
    v.rules.append(Rule(name, True, f"not measured: {seen}; {reason}", "not measured", threshold, unmeasured=True))


def survivor_rule(v: Verdict, records: list[dict[str, Any]], _args: argparse.Namespace) -> None:
    last = next(iter(reversed(of_kind(records, "survivors"))), None)
    if last is None:
        v.add("M10 no survivors", False, "no survivor sweep recorded")
        return
    v.add("M10 no survivors", not last["pids"], f"{len(last['pids'])} process(es) of the run outlived `comms stop`")


RULES = (
    rss_rule,
    save_rule,
    worktree_rule,
    disk_rule,
    lock_rule,
    error_rule,
    staleness_rule,
    retire_rule,
    msgpack_rule,
    survivor_rule,
)


def analyze(records: list[dict[str, Any]], args: argparse.Namespace) -> Verdict:
    """Apply every rule; the first-answer times of the worktrees ride along as a section."""
    v = Verdict()
    for rule in RULES:
        rule(v, records, args)
    first = [w["secs"] for w in of_kind(records, "worktree") if w.get("ok")]
    if first:
        v.sections.append(
            f"First answer of an added worktree: median {statistics.median(first):.1f} s, max {max(first):.1f} s "
            f"over {len(first)}."
        )
    return v


def render(v: Verdict, records: list[dict[str, Any]], source: Path) -> str:
    meta = next(iter(of_kind(records, "meta")), {})
    lines = [
        f"# Large-monorepo soak report: {'PASS' if v.passed else 'FAIL'}",
        "",
        f"- input: `{source}`",
        f"- repository: {meta.get('repo', '?')} at `{meta.get('commit', '?')}` ({meta.get('files', '?')} files)",
        f"- binary: `{meta.get('binary', '?')}` ({meta.get('version', '?')})",
        f"- host: {meta.get('platform', '?')}, {meta.get('cpus', '?')} CPUs",
        f"- {meta.get('worktrees', '?')} worktrees, {meta.get('sessions', '?')} sessions, load {meta.get('duration', '?')} s",
        "",
        "## Rules",
        "",
        "| rule | verdict | measurement |",
        "|---|---|---|",
    ]
    lines += [f"| {r.name} | {rule_verdict(r)} | {r.detail.replace('|', '/')} |" for r in v.rules]
    for section in v.sections:
        lines += ["", section]
    return "\n".join(lines) + "\n"


def build_parser() -> argparse.ArgumentParser:
    """The command line: the input, the report paths and every threshold with its default."""
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("ndjson", type=Path)
    ap.add_argument("--report", type=Path, help="markdown report path (default: next to the input)")
    ap.add_argument("--json", type=Path, help="also write the rule verdicts as JSON")
    ap.add_argument("--worktrees", type=int, default=20, help="worktrees the run must have added (default 20)")
    ap.add_argument("--sessions", type=int, default=30, help="concurrent sessions of the run (default 30)")
    ap.add_argument("--cap-mib", type=float, default=3072, help="the daemon's memory cap, MiB (default 3072)")
    ap.add_argument("--rss-mib", type=float, default=2048, help="peak daemon RSS allowed, MiB (default 2048)")
    ap.add_argument("--save-ms", type=float, default=200, help="p95 ms of one save (default 200)")
    ap.add_argument("--min-saves", type=int, default=5, help="saves the run must complete (default 5)")
    ap.add_argument(
        "--worktree-pct", type=float, default=75, help="bytes an added worktree adds, % of the first scan (default 75)"
    )
    ap.add_argument("--dir-tol", type=float, default=2.0, help="data dir MiB/min, journals excluded (default 2.0)")
    ap.add_argument(
        "--status-p99-ms", type=float, default=2000, help="p99 ms of index_status under load (default 2000)"
    )
    ap.add_argument("--busy-pct", type=float, default=1.0, help="calls answered busy, % (default 1)")
    ap.add_argument("--min-calls", type=int, default=100, help="index_status calls the load must make (default 100)")
    ap.add_argument("--error-pct", type=float, default=1.0, help="calls that fail, % (default 1)")
    ap.add_argument("--edit-stale-s", type=float, default=30, help="seconds until a save is answered (default 30)")
    ap.add_argument(
        "--switch-stale-s", type=float, default=60, help="seconds until a branch switch is answered (default 60)"
    )
    ap.add_argument("--gc-s", type=float, default=120, help="seconds `cache gc` may take (default 120)")
    ap.add_argument(
        "--msgpack-mib-h", type=float, default=4096, help="index.msgpack MiB written per hour (default 4096)"
    )
    return ap


def main() -> int:
    args = build_parser().parse_args()
    try:
        records = load(args.ndjson)
    except (OSError, json.JSONDecodeError) as exc:
        sys.stderr.write(f"unusable input: {exc}\n")
        return 2
    verdict = analyze(records, args)
    out = args.report or args.ndjson.with_suffix(".report.md")
    out.write_text(render(verdict, records, args.ndjson))
    if args.json:
        args.json.write_text(
            json.dumps({"pass": verdict.passed, "rules": [r.__dict__ for r in verdict.rules]}, indent=2)
        )
    sys.stdout.write(out.read_text())
    return 0 if verdict.passed else 1


if __name__ == "__main__":
    sys.exit(main())
