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

# The block caches of the Fjall databases the daemon holds for one workspace, in MiB: the group
# index (`INDEX_CACHE_CEILING_BYTES`, src/index/mod.rs), the memory store (`MEMORY_CACHE_BYTES`,
# src/index/memory_store.rs), the git history (`CACHE_BYTES`, src/git_history/mod.rs) and the comms
# store (`CACHE_BYTES`, src/comms/v2/store/open.rs).
LSM_BLOCK_CACHE_MIB = 256 + 8 + 16 + 16


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
    value: str = ""
    threshold: str = ""
    waived: bool = False


@dataclass
class Verdict:
    rules: list[Rule] = field(default_factory=list)
    sections: list[str] = field(default_factory=list)

    def add(self, name: str, passed: bool, detail: str, value: str = "", threshold: str = "") -> None:
        self.rules.append(Rule(name, passed, detail, value, threshold))

    @property
    def passed(self) -> bool:
        return bool(self.rules) and all(r.passed for r in self.rules)


def load(path: Path) -> list[dict[str, Any]]:
    out = []
    for line in path.read_text().splitlines():
        if line.strip():
            out.append(json.loads(line))
    return out


@dataclass
class Growth:
    whole: Fit
    half: Fit
    tol: float
    first: float
    last: float
    minutes: float

    @property
    def plateaued(self) -> bool:
        """The second half levelled off: its slope plus two standard errors is under tolerance."""
        return self.half.slope + 2 * self.half.se < self.tol

    @property
    def climbing(self) -> bool:
        return self.whole.slope > self.tol and self.whole.significant and not self.plateaued

    def detail(self, unit: str) -> str:
        return (
            f"slope {self.whole.slope:+.3f} {unit}/min (±{2 * self.whole.se:.3f}), second half {self.half.slope:+.3f} "
            f"(±{2 * self.half.se:.3f}) {unit}/min{' (levelled off)' if self.plateaued else ''}, tolerance {self.tol} "
            f"{unit}/min; {self.first:.1f} -> {self.last:.1f} {unit} over {self.minutes:.1f} min, n={self.whole.n}"
        )

    def value(self, unit: str) -> str:
        return f"{self.whole.slope:+.3f} {unit}/min (2SE {2 * self.whole.se:.3f}; 2nd half {self.half.slope:+.3f})"


def measure_growth(points: list[tuple[float, float]], tol: float, scale: float = 1.0) -> Growth | str:
    """The whole-window and second-half slopes of a series, per minute; a string says why there are none."""
    if len(points) < 6:  # noqa: PLR2004
        return f"only {len(points)} samples: nothing to fit"
    t0 = points[0][0]
    pts = [((t - t0) / 60.0, y / scale) for t, y in points]
    whole = fit(pts)
    half = fit(pts[len(pts) // 2 :])
    if whole is None or half is None:
        return "degenerate series"
    return Growth(whole, half, tol, pts[0][1], pts[-1][1], pts[-1][0])


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
    growth = measure_growth(points, tol, scale)
    if isinstance(growth, str):
        v.add(label, False, growth)
        return
    v.add(label, not growth.climbing, growth.detail(unit), growth.value(unit), f"{tol} {unit}/min")


def lsm_waiver_bound(minutes: float, args: argparse.Namespace) -> float:
    """The daemon memory slope, in MiB/min, waived as the lsm-tree block cache filling (#164).

    Until lsm-tree evicts the blocks of deleted tables, each Fjall database's block cache fills
    with dead blocks up to its capacity, so the daemon may grow by at most the sum of its block
    caches (`--lsm-cache-mib`) over the window judged. A slope above that capacity spread over the
    window is more than the caches can hold and is not waived. `--lsm-cache-mib 0` lifts the waiver.
    """
    return args.lsm_cache_mib / minutes if minutes > 0 else 0.0


def _waiver(rate: float, minutes: float, args: argparse.Namespace, rise: float | None = None) -> tuple[bool, str, str]:
    """Whether a daemon memory `rate` (MiB/min) over `minutes` is waived, the note for its detail and
    the suffix for its threshold. A fitted slope can understate a late step, so a measured `rise`
    (MiB) must also fit in the block caches when the caller has one."""
    if args.lsm_cache_mib <= 0:
        return False, "; no lsm-tree block-cache waiver (--lsm-cache-mib 0)", ""
    bound = lsm_waiver_bound(minutes, args)
    waived = rate <= bound and (rise is None or rise <= args.lsm_cache_mib)
    rise_note = f", measured rise {rise:.1f} MiB" if rise is not None else ""
    note = (
        f"; {'waived until #164' if waived else 'over the #164 waiver bound'}: lsm-tree block-cache bound "
        f"{bound:.3f} MiB/min ({args.lsm_cache_mib:g} MiB of block cache over {minutes:.1f} min{rise_note})"
    )
    return waived, note, f"; waived to {bound:.3f} MiB/min until #164"


def measured_rise(points: list[tuple[float, float]], scale: float = 1.0) -> float:
    """The level at the end of a series less the level at its start, each the median of a tenth of the
    samples (at least three), so sampling noise does not move it and a late step is not averaged away."""
    k = max(3, len(points) // 10)
    ys = [y / scale for _, y in points]
    return statistics.median(ys[-k:]) - statistics.median(ys[:k])


def daemon_memory_rule(v: Verdict, label: str, points: list[tuple[float, float]], args: argparse.Namespace) -> None:
    """R2 for the daemon: the slope rule, with a climb waived while it stays within the lsm-tree
    block-cache bound (`lsm_waiver_bound`) and failed above it."""
    growth = measure_growth(points, args.mem_tol, MIB)
    if isinstance(growth, str):
        v.add(label, False, growth)
        return
    threshold = f"{args.mem_tol} MiB/min"
    if not growth.climbing:
        v.add(label, True, growth.detail("MiB"), growth.value("MiB"), threshold)
        return
    waived, note, suffix = _waiver(growth.whole.slope, growth.minutes, args, measured_rise(points, MIB))
    v.rules.append(Rule(label, waived, growth.detail("MiB") + note, growth.value("MiB"), threshold + suffix, waived))


def tables_rule(v: Verdict, role: str, pid: int, series: list[dict[str, Any]], args: argparse.Namespace) -> None:
    """R3 on Linux and macOS. A daemon sample that counts its live Fjall table descriptors is judged
    net of them: each is a table on disk, opened by the database's descriptor cache and closed when
    a compaction deletes it, so their number follows the tables the stores hold (their growth is
    R8's), not a leak. A descriptor kept on a deleted table is not live, so it is still judged. A
    series with any sample lacking the count (a process that reused the daemon's pid) is judged on
    its total fds."""
    if not series or any(r.get("fds_lsm_tables") is None for r in series):
        growth_rule(v, f"R3 {role} {pid} open fds", [(r["t"], r["fds"]) for r in series], args.fd_tol, "fds")
        return
    growth_rule(
        v,
        f"R3 {role} {pid} open fds net of live LSM tables",
        [(r["t"], r["fds"] - r["fds_lsm_tables"]) for r in series],
        args.fd_tol,
        "fds",
    )
    tables = [r["fds_lsm_tables"] for r in series]
    v.add(
        f"R3t {role} {pid} live LSM table fds",
        True,
        f"not judged: {tables[0]} -> {tables[-1]} over load, peak {max(tables)}; {_fd_kind_moves(series)}",
    )


def _fd_kind_moves(series: list[dict[str, Any]]) -> str:
    """The descriptor kinds whose count moved over load, for an R3 failure to name what it holds."""
    first, last = series[0].get("fd_kinds"), series[-1].get("fd_kinds")
    if first is None or last is None:
        return "no descriptor kinds recorded"
    moved = sorted(((last.get(k, 0) - first.get(k, 0), k) for k in set(first) | set(last)), key=lambda m: -abs(m[0]))
    return "descriptors by kind over load: " + (", ".join(f"{k} {d:+d}" for d, k in moved if d) or "unchanged")


def _quiescent(rows: list[dict[str, Any]], phase: str, start: float | None, settle: float) -> list[dict[str, Any]]:
    return [r for r in rows if r["phase"] == phase and start is not None and r["t"] >= start + settle]


def _name_delta(before: dict[str, int] | None, after: dict[str, int] | None) -> str:
    if before is None or after is None:
        return "no thread names on this platform"
    keys = sorted(set(before) | set(after))
    moved = [f"{k or '<unnamed>'} {before.get(k, 0)}->{after.get(k, 0)}" for k in keys if before.get(k) != after.get(k)]
    return ", ".join(moved) or "same names and counts"


def _prefixed(sizes: dict[str, int], prefix: str) -> list[tuple[str, int]]:
    return [(f"{prefix}{k}", n) for k, n in sizes.items()]


def _dir_growth(dirs: list[dict[str, Any]]) -> str:
    """The data-dir subtrees that grew most over load, for a failure to name its store."""
    first, last = dirs[0].get("data_parts") or {}, dirs[-1].get("data_parts") or {}
    grown = sorted(((last.get(k, 0) - first.get(k, 0), k) for k in set(first) | set(last)), reverse=True)
    top = [f"{k} {d / MIB:+.2f}" for d, k in grown[:4] if d > 0]
    return "grew over load (MiB): " + (", ".join(top) or "nothing")


def thread_rule(
    v: Verdict,
    label: str,
    rows: list[dict[str, Any]],
    marks: dict[str, float],
    args: argparse.Namespace,
) -> None:
    """Fail when threads started under load are still alive once the process is quiescent again.

    A leaked thread never exits; a pool thread (tokio's blocking pool, a reaped worker) exits after
    its keep-alive once nothing is asked of it. So the count is compared between two quiescent
    windows with the same sessions connected: the baseline after warm-up (every pool the workload
    uses already exists) and the idle phase after load, each after `--idle-settle` seconds, taking
    each window's minimum so a thread busy at one sampling instant does not count. The load slope is
    reported, not judged: a demand-sized pool steps up whenever concurrency peaks, and a line fitted
    through such a step reads as growth however bounded the pool is.
    """
    base = _quiescent(rows, "baseline", marks.get("baseline_start"), args.idle_settle)
    after = _quiescent(rows, "idle", marks.get("idle_start"), args.idle_settle)
    if len(base) < 2 or len(after) < 2:  # noqa: PLR2004
        v.add(label, False, f"quiescent samples: baseline {len(base)}, idle {len(after)} (need >= 2 each)")
        return
    low_base = min(base, key=lambda r: r["threads"])
    low_after = min(after, key=lambda r: r["threads"])
    retained = low_after["threads"] - low_base["threads"]
    load = [r for r in rows if r["phase"] == "load"]
    peak = max((r["threads"] for r in load), default=0)
    slope = fit([(r["t"] / 60.0, r["threads"]) for r in load]) if len(load) >= 3 else None  # noqa: PLR2004
    v.add(
        label,
        retained <= args.thread_slack,
        f"quiescent floor {low_base['threads']} before load, {low_after['threads']} after "
        f"({retained:+d}, slack {args.thread_slack}); load peak {peak}"
        + (f", load slope {slope.slope:+.3f}/min (not judged)" if slope else "")
        + f"; names before -> after: {_name_delta(low_base.get('thread_names'), low_after.get('thread_names'))}",
        f"{retained:+d} threads retained",
        f"{args.thread_slack}",
    )


def floor_rule(v: Verdict, label: str, points: list[tuple[float, float]], args: argparse.Namespace) -> None:
    """Fail when the memory floor keeps rising on a long run, below what the slope rule can see.

    The load phase is cut into four equal windows and each window's floor is its 10th-percentile
    sample, which ignores the sawtooth of memtables and transient scan peaks. A cache that fills
    and stops leaves the floors of windows 2-4 level; a leak lifts every floor. The rise from
    window 2 to window 4, per minute between their centres, must stay under `--floor-tol`. Window
    1 is left out because a cache may still be filling there. Runs shorter than
    `--floor-min-minutes` are not judged: over ten minutes a 0.25 MiB/min leak is a few MiB, under
    the sampling noise. A rise within the lsm-tree block-cache bound (`lsm_waiver_bound`) is
    waived until #164.
    """
    if len(points) < 40:  # noqa: PLR2004 - ten samples per window at least
        v.add(label, True, f"not judged: {len(points)} samples")
        return
    minutes = (points[-1][0] - points[0][0]) / 60.0
    if minutes < args.floor_min_minutes:
        v.add(label, True, f"not judged: {minutes:.1f} min of load < {args.floor_min_minutes} min")
        return
    quarter = len(points) // 4
    windows = [points[i * quarter : (i + 1) * quarter] for i in range(4)]
    floors = [statistics.quantiles([y for _, y in w], n=10)[0] / MIB for w in windows]
    centres = [statistics.fmean(t for t, _ in w) / 60.0 for w in windows]
    rate = (floors[3] - floors[1]) / (centres[3] - centres[1])
    rising = floors[1] <= floors[2] <= floors[3] and rate > args.floor_tol
    detail = (
        f"window floors {' / '.join(f'{f:.1f}' for f in floors)} MiB, windows 2-4 rise {rate:+.3f} MiB/min "
        f"(tolerance {args.floor_tol}) over {minutes:.1f} min"
    )
    value = f"{rate:+.3f} MiB/min (floors {floors[1]:.1f} / {floors[2]:.1f} / {floors[3]:.1f} MiB)"
    if not rising:
        v.add(label, True, detail, value, f"{args.floor_tol} MiB/min")
        return
    waived, note, suffix = _waiver(rate, minutes, args)
    v.rules.append(Rule(label, waived, detail + note, value, f"{args.floor_tol} MiB/min{suffix}", waived))


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
    marks = {r["name"]: r["t"] for r in records if r.get("kind") == "phase"}
    windows = str(meta.get("platform", "")).startswith("Windows")
    for pid in long_lived:
        role = "daemon" if pid in daemon_pids else "main serve"
        series = [r for r in by_pid[pid] if r["phase"] == "load"]
        mem_key = "footprint" if series and series[0].get("footprint") is not None else "rss"
        for key in dict.fromkeys((mem_key, "rss")):
            points = [(r["t"], r[key]) for r in series]
            if role == "daemon":
                daemon_memory_rule(v, f"R2 daemon {pid} memory ({key})", points, args)
            else:
                growth_rule(v, f"R2 {role} {pid} memory ({key})", points, args.mem_tol, "MiB", MIB)
            if role == "daemon" and key == mem_key:
                floor_rule(v, f"R2f daemon {pid} memory floor ({key})", points, args)
        if windows:
            # A Windows handle count includes one handle per live thread, so a pool that grows at a
            # concurrency peak and is reaped at idle reads as handle growth (R4 judges threads). The
            # handles a call can leak are the ones threads do not account for.
            growth_rule(
                v,
                f"R3 {role} {pid} open handles net of threads",
                [(r["t"], r["fds"] - r["threads"]) for r in series],
                args.fd_tol,
                "handles",
            )
        else:
            tables_rule(v, role, pid, series, args)
        thread_rule(v, f"R4 {role} {pid} threads", by_pid[pid], marks, args)

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
    # macOS reuses pids within a long soak: a later CLI one-shot or session can get a closed
    # session's pid, so only a `serve` sample taken before that pid opened another session counts.
    reopened: dict[int, list[float]] = {}
    for r in records:
        if r.get("kind") == "session" and r.get("event") == "open":
            reopened.setdefault(r["pid"], []).append(r["t"])
    lingering = []
    for pid, close in sessions_close.items():
        if not close.get("exited"):
            lingering.append(f"{pid} did not exit after {close.get('how')}")
            continue
        next_open = min((t for t in reopened.get(pid, []) if t > close["t"]), default=float("inf"))
        late = [
            r["t"]
            for r in by_pid.get(pid, [])
            if r["role"] == "serve" and close["t"] + args.exit_grace < r["t"] < next_open
        ]
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
            f"{cores * 100:.3f}% of a core",
            f"{args.idle_cpu * 100:.1f}%",
        )
        if idle_wake:
            rate = sum(idle_wake) / window
            v.add(
                "R6b idle wakeups",
                rate <= args.idle_wakeups,
                f"{rate:.1f} wakeups/s summed over the run's processes (threshold {args.idle_wakeups}/s)",
                f"{rate:.1f}/s",
                f"{args.idle_wakeups}/s",
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
    growth_rule(
        v,
        "R8 data dir size (apparent, journals and telemetry excluded)",
        [
            (r["t"], r["data_bytes"] - sum(r.get("data_journals", {}).values()) - r.get("data_telemetry_bytes", 0))
            for r in dirs
        ],
        args.dir_tol,
        "MiB",
        MIB,
    )
    # The comms dir holds the daemon's log and its stores; their Fjall journals are size-bounded and
    # judged with the data dir's by R8b.
    growth_rule(
        v,
        "R8c comms dir size (apparent, journals excluded)",
        [(r["t"], r.get("comms_bytes", 0) - sum(r.get("comms_journals", {}).values())) for r in dirs],
        args.dir_tol,
        "MiB",
        MIB,
    )
    if dirs and "data_journals" not in dirs[0]:
        v.add("R8b size-bounded stores", False, "no journal / telemetry split recorded: soak.py predates it")
    elif dirs:
        journal = max(
            (
                (n, db)
                for r in dirs
                for db, n in [*r["data_journals"].items(), *_prefixed(r.get("comms_journals", {}), "comms/")]
            ),
            default=(0, "-"),
        )
        telemetry = max(r["data_telemetry_bytes"] for r in dirs)
        v.add(
            "R8b size-bounded stores",
            journal[0] <= args.journal_mib * MIB and telemetry <= args.telemetry_mib * MIB,
            f"largest Fjall journal set {journal[0] / MIB:.1f} MiB ({journal[1]}; bound {args.journal_mib} MiB), "
            f"telemetry log {telemetry / MIB:.1f} MiB (bound {args.telemetry_mib} MiB); "
            f"{_dir_growth(dirs)}",
        )

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
        for phase in ("warmup", "baseline", "load", "idle"):
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


def rule_verdict(r: Rule) -> str:
    """A rule's verdict cell: a waived pass says so, so the waiver stays visible until it is lifted."""
    if not r.passed:
        return "**FAIL**"
    return "waived" if r.waived else "pass"


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
        lines.append(f"| {r.name} | {rule_verdict(r)} | {r.detail.replace('|', '/')} |")
    for section in v.sections:
        lines += ["", section]
    return "\n".join(lines) + "\n"


def build_parser() -> argparse.ArgumentParser:
    """The command line: the input, the report paths and every threshold with its default."""
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("ndjson", type=Path)
    ap.add_argument("--report", type=Path, help="markdown report path (default: next to the input)")
    ap.add_argument("--json", type=Path, help="also write the rule verdicts as JSON")
    ap.add_argument(
        "--mem-tol", type=float, default=1.0, help="MiB/min a long-lived process may still grow (default 1.0)"
    )
    ap.add_argument(
        "--floor-tol", type=float, default=0.25, help="MiB/min the daemon's memory floor may rise (default 0.25)"
    )
    ap.add_argument(
        "--floor-min-minutes", type=float, default=30.0, help="load minutes before the floor rule judges (default 30)"
    )
    ap.add_argument(
        "--lsm-cache-mib",
        type=float,
        default=LSM_BLOCK_CACHE_MIB,
        help="daemon block-cache MiB whose filling R2 and R2f waive until #164; 0 lifts the waiver "
        f"(default {LSM_BLOCK_CACHE_MIB:g})",
    )
    ap.add_argument("--fd-tol", type=float, default=0.2, help="fds/min (default 0.2)")
    ap.add_argument(
        "--thread-slack",
        type=int,
        default=0,
        help="threads the quiescent floor may rise from the baseline to the idle phase (default 0)",
    )
    ap.add_argument(
        "--dir-tol", type=float, default=1.0, help="data dir MiB/min, journals and telemetry excluded (default 1.0)"
    )
    ap.add_argument(
        "--journal-mib",
        type=float,
        default=192,
        help="Fjall journal MiB one database may hold (default 192; see docs/hardening.md R8b)",
    )
    ap.add_argument(
        "--telemetry-mib",
        type=float,
        default=16,
        help="telemetry.jsonl plus its rotated generation, MiB (default 16 = 2 x TELEMETRY_ROTATE_BYTES)",
    )
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
    return ap


def main() -> int:
    args = build_parser().parse_args()
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
