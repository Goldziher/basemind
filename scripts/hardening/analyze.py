#!/usr/bin/env python3
"""Decide pass/fail for a soak NDJSON with stated numeric rules, and write a markdown report.

Usage:
    analyze.py soak.ndjson [--report report.md] [--json verdict.json]

Exit status: 0 PASS, 1 FAIL, 2 unusable input. The rules and their thresholds are listed in
docs/hardening.md; every threshold is a flag here so a rerun can state what it used.
"""

from __future__ import annotations

import argparse
import json
import math
import statistics
import sys
from collections import defaultdict
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

MIB = 1024 * 1024


@dataclass
class Fit:
    slope: float
    se: float
    n: int

    @property
    def significant(self) -> bool:
        return self.slope - 2 * self.se > 0


def fit(points: list[tuple[float, float]]) -> Fit | None:
    """Least-squares slope (y per x unit) and its standard error."""
    n = len(points)
    if n < 3:  # noqa: PLR2004 - a slope and its error need three points
        return None
    xs = [p[0] for p in points]
    ys = [p[1] for p in points]
    mx, my = statistics.fmean(xs), statistics.fmean(ys)
    sxx = sum((x - mx) ** 2 for x in xs)
    if sxx == 0:
        return None
    slope = sum((x - mx) * (y - my) for x, y in zip(xs, ys, strict=True)) / sxx
    resid = [y - (my + slope * (x - mx)) for x, y in zip(xs, ys, strict=True)]
    se = math.sqrt(sum(r * r for r in resid) / (n - 2) / sxx)
    return Fit(slope, se, n)


@dataclass
class Rule:
    name: str
    passed: bool
    detail: str


@dataclass
class Verdict:
    rules: list[Rule] = field(default_factory=list)
    sections: list[str] = field(default_factory=list)

    def add(self, name: str, passed: bool, detail: str) -> None:
        self.rules.append(Rule(name, passed, detail))

    @property
    def passed(self) -> bool:
        return bool(self.rules) and all(r.passed for r in self.rules)


def load(path: Path) -> list[dict[str, Any]]:
    out = []
    for line in path.read_text().splitlines():
        if line.strip():
            out.append(json.loads(line))
    return out


def growth_rule(
    v: Verdict,
    label: str,
    points: list[tuple[float, float]],
    tol: float,
    unit: str,
    scale: float = 1.0,
) -> None:
    """Fail when the series climbs: the whole-window slope is over tolerance and statistically
    positive, unless the second half demonstrably levelled off (its slope plus two standard errors
    is under tolerance), which is how a cache that filled after warm-up is told from a leak."""
    if len(points) < 6:  # noqa: PLR2004
        v.add(label, False, f"only {len(points)} samples: nothing to fit")
        return
    t0 = points[0][0]
    pts = [((t - t0) / 60.0, y / scale) for t, y in points]
    whole = fit(pts)
    half = fit(pts[len(pts) // 2 :])
    if whole is None or half is None:
        v.add(label, False, "degenerate series")
        return
    plateaued = half.slope + 2 * half.se < tol
    climbing = whole.slope > tol and whole.significant and not plateaued
    first, last = pts[0][1], pts[-1][1]
    v.add(
        label,
        not climbing,
        f"slope {whole.slope:+.3f} {unit}/min (±{2 * whole.se:.3f}), second half {half.slope:+.3f} "
        f"(±{2 * half.se:.3f}) {unit}/min{' (levelled off)' if plateaued else ''}, tolerance {tol} {unit}/min; {first:.1f} -> {last:.1f} {unit} over {pts[-1][0]:.1f} min, n={whole.n}",
    )


def analyze(records: list[dict[str, Any]], args: argparse.Namespace) -> Verdict:  # noqa: C901, PLR0912, PLR0915
    v = Verdict()
    meta = next((r for r in records if r.get("kind") == "meta"), {})
    procs = [r for r in records if r.get("kind") == "proc"]
    by_pid: dict[int, list[dict[str, Any]]] = defaultdict(list)
    for r in procs:
        by_pid[r["pid"]].append(r)
    sessions_open = {r["pid"]: r for r in records if r.get("kind") == "session" and r.get("event") == "open"}
    sessions_close = {r["pid"]: r for r in records if r.get("kind") == "session" and r.get("event") == "close"}

    # ---- R0: the run measured something ----
    daemon_pids = sorted(
        {r["pid"] for r in procs if r["role"] == "daemon" and r["phase"] in ("warmup", "load", "idle")}
    )
    main_pids = [p for p, r in sessions_open.items() if str(r.get("name", "")).startswith("main-")]
    long_lived = daemon_pids + [p for p in main_pids if p in by_pid]
    load_counts = {p: sum(1 for r in by_pid[p] if r["phase"] == "load") for p in long_lived}
    idle_rounds = sorted({r["t"] for r in procs if r["phase"] == "idle"})
    calls = [r for r in records if r.get("kind") == "call"]
    enough = (
        bool(meta)
        and any(c >= args.min_samples for c in load_counts.values())
        and len(idle_rounds) >= args.min_idle_samples
        and len(calls) > 0
    )
    v.add(
        "R0 run measured something",
        enough,
        f"meta={'yes' if meta else 'no'}, load samples per long-lived pid {load_counts or '{}'} (need >= {args.min_samples}), "
        f"idle sample rounds {len(idle_rounds)} (need >= {args.min_idle_samples}), tool calls {len(calls)}",
    )

    # ---- R1: the daemon is not replaced mid-run ----
    v.add(
        "R1 one daemon for the whole run",
        len(daemon_pids) <= 1,
        f"daemon pids seen before shutdown: {daemon_pids or 'none (in-process serve)'}",
    )

    # ---- R2-R4: memory, descriptors, threads of every long-lived process ----
    for pid in long_lived:
        role = "daemon" if pid in daemon_pids else "main serve"
        series = [r for r in by_pid[pid] if r["phase"] == "load"]
        mem_key = "footprint" if series and series[0].get("footprint") is not None else "rss"
        growth_rule(
            v, f"R2 {role} {pid} memory ({mem_key})", [(r["t"], r[mem_key]) for r in series], args.mem_tol, "MiB", MIB
        )
        if mem_key != "rss":
            growth_rule(
                v, f"R2 {role} {pid} memory (rss)", [(r["t"], r["rss"]) for r in series], args.mem_tol, "MiB", MIB
            )
        growth_rule(v, f"R3 {role} {pid} open fds", [(r["t"], r["fds"]) for r in series], args.fd_tol, "fds")
        growth_rule(v, f"R4 {role} {pid} threads", [(r["t"], r["threads"]) for r in series], args.thread_tol, "threads")

    # ---- R5: no process accumulation during load ----
    census = [r for r in records if r.get("kind") == "census" and r["phase"] == "load"]
    if len(census) >= 4:  # noqa: PLR2004
        half = len(census) // 2
        first_max = max(r["total"] for r in census[:half])
        second_max = max(r["total"] for r in census[half:])
        v.add(
            "R5 no process accumulation under load",
            second_max <= first_max,
            f"max live processes of the run: first half {first_max}, second half {second_max}",
        )
    else:
        v.add("R5 no process accumulation under load", False, f"{len(census)} census samples in load")
    lingering = []
    for pid, close in sessions_close.items():
        if not close.get("exited"):
            lingering.append(f"{pid} did not exit after {close.get('how')}")
            continue
        late = [r["t"] for r in by_pid.get(pid, []) if r["t"] > close["t"] + args.exit_grace]
        if late:
            lingering.append(f"{pid} still sampled {late[-1] - close['t']:.0f}s after {close.get('how')}")
    v.add(
        "R5b closed sessions exit",
        not lingering,
        f"{len(sessions_close)} sessions closed; lingering: {lingering[:5] or 'none'}",
    )

    # ---- R6: idle means idle ----
    idle_start = next((r["t"] for r in records if r.get("kind") == "phase" and r.get("name") == "idle_start"), None)
    idle = [
        r for r in procs if r["phase"] == "idle" and idle_start is not None and r["t"] >= idle_start + args.idle_settle
    ]
    idle_cpu = sum(r["cpu_delta_s"] or 0 for r in idle)
    idle_wake = [r["wakeups_delta"] for r in idle if r.get("wakeups_delta") is not None]
    rounds = sorted({r["t"] for r in idle})
    window = (rounds[-1] - rounds[0] + (idle[0]["interval_s"] if idle else 0)) if rounds else 0
    if window >= args.min_idle_window:
        cores = idle_cpu / window
        per_proc = defaultdict(float)
        for r in idle:
            per_proc[f"{r['role']} {r['pid']}"] += r["cpu_delta_s"] or 0
        busiest = max(per_proc.items(), key=lambda kv: kv[1]) if per_proc else ("-", 0.0)
        v.add(
            "R6 idle CPU",
            cores <= args.idle_cpu,
            f"{cores * 100:.3f}% of one core over {window:.0f}s (threshold {args.idle_cpu * 100:.1f}%); "
            f"busiest {busiest[0]} {busiest[1] / window * 100:.3f}%",
        )
        if idle_wake:
            rate = sum(idle_wake) / window
            v.add(
                "R6b idle wakeups",
                rate <= args.idle_wakeups,
                f"{rate:.1f} wakeups/s summed over the run's processes (threshold {args.idle_wakeups}/s)",
            )
    else:
        v.add("R6 idle CPU", False, f"idle window after settle is {window:.0f}s (need >= {args.min_idle_window}s)")

    # ---- R7: shutdown leaves nothing behind ----
    shutdown = [r for r in records if r.get("kind") == "shutdown"]
    want = {"main_stdin_eof", "host_sigkill", "serve_sigkill", "comms_stop"}
    seen = {r["step"] for r in shutdown}
    for r in shutdown:
        detail = f"exited={r.get('exited')}"
        ok = bool(r.get("exited"))
        if r["step"] == "serve_sigkill" and r.get("daemon_fds_before") is not None:
            after = r.get("daemon_fds_after")
            fd_ok = after is not None and after <= r["daemon_fds_before"] + args.fd_slack
            detail += f", daemon fds before/during/after {r['daemon_fds_before']}/{r.get('daemon_fds_during')}/{after} (slack {args.fd_slack})"
            ok = ok and fd_ok
        if "secs" in r:
            detail += f", {r['secs']}s"
        v.add(f"R7 shutdown: {r['step']}", ok, detail)
    if missing := want - seen:
        v.add("R7 shutdown steps ran", False, f"missing steps: {sorted(missing)}")
    survivors = next((r for r in records if r.get("kind") == "survivors"), None)
    if survivors is None:
        v.add("R7 no survivors", False, "no survivor sweep recorded")
    else:
        pids = survivors.get("pids", [])
        argv = survivors.get("argv", {})
        v.add(
            "R7 no survivors",
            not pids,
            f"{len(pids)} process(es) of the run alive after shutdown: "
            + ", ".join(f"{p} {' '.join(argv.get(str(p), []))[:80]}" for p in pids),
        )

    # ---- R8: data dir bounded under the steady edit/rescan cycle ----
    dirs = [r for r in records if r.get("kind") == "dir" and r["phase"] == "load"]
    growth_rule(v, "R8 data dir size (apparent)", [(r["t"], r["data_bytes"]) for r in dirs], args.dir_tol, "MiB", MIB)

    # ---- R9: the workload itself worked ----
    transport = [c for c in calls if c.get("transport_error")]
    tool_errors = [c for c in calls if not c.get("ok") and not c.get("transport_error") and c.get("expect_ok", True)]
    v.add(
        "R9 no transport errors",
        not transport,
        f"{len(transport)} of {len(calls)} calls; first: {transport[0]['transport_error'][:160] if transport else '-'}",
    )
    v.add(
        "R9b no unexpected tool errors",
        not tool_errors,
        f"{len(tool_errors)} of {len(calls)} calls; first: "
        + (
            f"{tool_errors[0]['tool']} {json.dumps(tool_errors[0].get('args'))[:120]} -> {tool_errors[0].get('error', '')[:160]}"
            if tool_errors
            else "-"
        ),
    )
    edits = [r for r in records if r.get("kind") == "edit_check"]
    bad_edits = [r for r in edits if not (r["edit_visible"] and r["deleted_gone"])]
    v.add(
        "R9c edits visible after index_rescan",
        bool(edits) and not bad_edits,
        f"{len(edits) - len(bad_edits)}/{len(edits)} cycles saw the edit and lost the deleted file",
    )
    cli = [r for r in records if r.get("kind") == "cli"]
    bad_cli = [r for r in cli if not r.get("ok")]
    v.add(
        "R9d CLI one-shots succeed",
        not bad_cli,
        f"{len(cli) - len(bad_cli)}/{len(cli)} ok; first failure: {(' '.join(bad_cli[0]['argv']) + ' -> ' + str(bad_cli[0].get('stderr', ''))[-160:]) if bad_cli else '-'}",
    )

    v.sections.append(series_table(by_pid, long_lived, daemon_pids))
    v.sections.append(call_table(calls, records))
    return v


def series_table(by_pid: dict[int, list[dict[str, Any]]], long_lived: list[int], daemon_pids: list[int]) -> str:
    lines = [
        "## Long-lived processes",
        "",
        "| process | phase | samples | footprint MiB start/max/end | rss MiB start/max/end | fds start/max/end | threads start/max/end | cpu s |",
        "|---|---|---|---|---|---|---|---|",
    ]
    for pid in long_lived:
        role = "daemon" if pid in daemon_pids else "main serve"
        for phase in ("warmup", "load", "idle"):
            s = [r for r in by_pid[pid] if r["phase"] == phase]
            if not s:
                continue

            def triple(key: str, scale: float = 1.0, rows: list[dict[str, Any]] = s) -> str:
                vals = [r[key] / scale for r in rows if r.get(key) is not None]
                return f"{vals[0]:.0f}/{max(vals):.0f}/{vals[-1]:.0f}" if vals else "-"

            cpu = sum(r["cpu_delta_s"] or 0 for r in s)
            lines.append(
                f"| {role} {pid} | {phase} | {len(s)} | {triple('footprint', MIB)} | {triple('rss', MIB)} | {triple('fds')} | {triple('threads')} | {cpu:.1f} |"
            )
    return "\n".join(lines)


def call_table(calls: list[dict[str, Any]], records: list[dict[str, Any]]) -> str:
    by_tool: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for c in calls:
        by_tool[c["tool"]].append(c)
    lines = ["## Tool calls", "", "| tool | calls | errors | p50 ms | p95 ms | max ms |", "|---|---|---|---|---|---|"]
    for tool in sorted(by_tool):
        rows = by_tool[tool]
        ms = sorted(c["ms"] for c in rows if "ms" in c)
        errs = sum(1 for c in rows if not c.get("ok"))
        if ms:
            p50, p95 = ms[len(ms) // 2], ms[min(len(ms) - 1, int(len(ms) * 0.95))]
            lines.append(f"| {tool} | {len(rows)} | {errs} | {p50:.0f} | {p95:.0f} | {ms[-1]:.0f} |")
        else:
            lines.append(f"| {tool} | {len(rows)} | {errs} | - | - | - |")
    skips = [r for r in records if r.get("kind") == "skip"]
    if skips:
        lines += ["", "Skipped (feature not in this build): " + ", ".join(f"`{r['tool']}`" for r in skips)]
    return "\n".join(lines)


def render(v: Verdict, records: list[dict[str, Any]], source: Path) -> str:
    meta = next((r for r in records if r.get("kind") == "meta"), {})
    end_t = max((r.get("t", 0) for r in records), default=0)
    lines = [
        f"# Soak report: {'PASS' if v.passed else 'FAIL'}",
        "",
        f"- input: `{source}`",
        f"- binary: `{meta.get('binary', '?')}` ({meta.get('version', '?')})",
        f"- host: {meta.get('platform', '?')}, {meta.get('cpus', '?')} CPUs",
        f"- load {meta.get('duration', '?')}s (warm-up {meta.get('warmup', '?')}s), idle {meta.get('idle', '?')}s, sample every {meta.get('sample_every', '?')}s, run {end_t:.0f}s",
        "",
        "## Rules",
        "",
        "| rule | verdict | measurement |",
        "|---|---|---|",
    ]
    for r in v.rules:
        lines.append(f"| {r.name} | {'pass' if r.passed else '**FAIL**'} | {r.detail.replace('|', '/')} |")
    for section in v.sections:
        lines += ["", section]
    return "\n".join(lines) + "\n"


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("ndjson", type=Path)
    ap.add_argument("--report", type=Path, help="markdown report path (default: next to the input)")
    ap.add_argument("--json", type=Path, help="also write the rule verdicts as JSON")
    ap.add_argument(
        "--mem-tol", type=float, default=1.0, help="MiB/min a long-lived process may still grow (default 1.0)"
    )
    ap.add_argument("--fd-tol", type=float, default=0.2, help="fds/min (default 0.2)")
    ap.add_argument("--thread-tol", type=float, default=0.2, help="threads/min (default 0.2)")
    ap.add_argument("--dir-tol", type=float, default=1.0, help="data dir MiB/min (default 1.0)")
    ap.add_argument(
        "--idle-cpu", type=float, default=0.01, help="idle CPU as a fraction of one core (default 0.01 = 1%%)"
    )
    ap.add_argument(
        "--idle-wakeups", type=float, default=100.0, help="idle wakeups/s summed over the run (macOS only; default 100)"
    )
    ap.add_argument(
        "--idle-settle", type=float, default=30.0, help="seconds at the start of idle not judged (default 30)"
    )
    ap.add_argument("--min-idle-window", type=float, default=60.0, help="seconds of judged idle required (default 60)")
    ap.add_argument(
        "--min-samples", type=int, default=20, help="post-warm-up samples a long-lived process needs (default 20)"
    )
    ap.add_argument("--min-idle-samples", type=int, default=10)
    ap.add_argument("--exit-grace", type=float, default=15.0, help="seconds after close a session may still be sampled")
    ap.add_argument(
        "--fd-slack", type=int, default=2, help="daemon fds allowed above the pre-session count after a killed serve"
    )
    args = ap.parse_args()
    try:
        records = load(args.ndjson)
    except (OSError, json.JSONDecodeError) as exc:
        sys.stderr.write(f"unusable input: {exc}\n")
        return 2
    verdict = analyze(records, args)
    report = render(verdict, records, args.ndjson)
    out = args.report or args.ndjson.with_suffix(".report.md")
    out.write_text(report)
    if args.json:
        args.json.write_text(
            json.dumps({"pass": verdict.passed, "rules": [r.__dict__ for r in verdict.rules]}, indent=2)
        )
    sys.stdout.write(report)
    return 0 if verdict.passed else 1


if __name__ == "__main__":
    sys.exit(main())
