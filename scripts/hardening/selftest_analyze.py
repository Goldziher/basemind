#!/usr/bin/env python3
"""Prove analyze.py's rules fire: synthetic soak NDJSON, one healthy and several broken.

Each case is written to an NDJSON file and run through `analyze.py` as a subprocess. The healthy
case must PASS (exit 0) and every broken case must FAIL (exit 1) on the rule it targets and on no
rule outside the expected set, so a rule that always fails is caught as well as one that never does.

Usage:
    selftest_analyze.py [--out DIR]
"""

from __future__ import annotations

import argparse
import json
import random
import subprocess
import sys
import time
from collections.abc import Callable
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

from common import MIB, default_out_root  # noqa: E402

HERE = Path(__file__).resolve().parent
EVERY = 5.0
WARMUP, QUIESCE, LOAD, IDLE = 120.0, 45.0, 600.0, 180.0
DAEMON, MAIN = 4242, 4343


def healthy(rng: random.Random, load: float = LOAD) -> list[dict[str, Any]]:
    """A run whose memory ramps during warm-up then plateaus with noise, and whose idle is quiet."""
    recs: list[dict[str, Any]] = [
        {
            "kind": "meta",
            "t": 0,
            "binary": "synthetic",
            "version": "synthetic",
            "duration": load,
            "warmup": WARMUP,
            "idle": IDLE,
            "sample_every": EVERY,
        },
        {"kind": "session", "event": "open", "name": "main-1", "pid": MAIN, "t": 1},
    ]
    t = 2.0
    cpu = {DAEMON: 0.0, MAIN: 0.0}
    short_pid = 9000
    while t < load + IDLE:
        phase = "warmup" if t < WARMUP else "baseline" if t < WARMUP + QUIESCE else "load" if t < load else "idle"
        for pid, role, base in ((DAEMON, "daemon", 400), (MAIN, "serve", 20)):
            ramp = min(t, WARMUP) / WARMUP
            mem = (base * (0.5 + 0.5 * ramp) + rng.gauss(0, base * 0.03)) * MIB
            busy = 0.4 if phase != "idle" else 0.0005
            delta = busy * EVERY * rng.uniform(0.5, 1.5)
            threads = 30 + rng.choice((0, 0, 1, 2))
            cpu[pid] += delta
            recs.append(
                {
                    "kind": "proc",
                    "t": round(t, 2),
                    "phase": phase,
                    "pid": pid,
                    "role": role,
                    "rss": int(mem * 1.1),
                    "footprint": int(mem),
                    "vsz": 4 << 30,
                    "threads": threads,
                    "thread_names": {"bm-scan-#": 4, "tokio-runtime-w": threads - 4},
                    "fds": 60 + rng.choice((0, 1, 2, 3)),
                    "cpu_s": cpu[pid],
                    "cpu_delta_s": delta,
                    "wakeups_delta": int(EVERY * rng.uniform(2, 8)),
                    "interval_s": EVERY,
                }
            )
        recs.append(
            {
                "kind": "dir",
                "t": t,
                "phase": phase,
                "data_bytes": int((50 + rng.gauss(0, 0.5)) * MIB),
                "data_alloc_bytes": 0,
                "data_journals": {"cache/groups/g/index.fjall": 2 * MIB},
                "data_telemetry_bytes": MIB,
                "data_parts": {"cache/groups/g/index.fjall": 3 * MIB},
                "comms_bytes": 70 * MIB,
                "comms_journals": {"g1/agents.fjall": 64 * MIB},
            }
        )
        recs.append(
            {
                "kind": "census",
                "t": t,
                "phase": phase,
                "counts": {"daemon": 1, "serve": 1},
                "total": 2 + rng.choice((0, 1)),
            }
        )
        if phase not in ("idle", "baseline") and int(t) % 20 == 2:
            recs.append({"kind": "session", "event": "open", "name": f"short-{short_pid}", "pid": short_pid, "t": t})
            recs.append(
                {
                    "kind": "session",
                    "event": "close",
                    "how": "eof",
                    "name": f"short-{short_pid}",
                    "pid": short_pid,
                    "exited": True,
                    "exit_code": 0,
                    "t": t + 1,
                }
            )
            recs.append({"kind": "edit_check", "t": t, "cycle": short_pid, "edit_visible": True, "deleted_gone": True})
            recs.append({"kind": "cli", "t": t, "argv": ["status"], "exit_code": 0, "ok": True, "ms": 30})
            short_pid += 1
        recs.append(
            {
                "kind": "call",
                "t": t,
                "session": "main-1",
                "tool": "code_symbols",
                "ok": True,
                "expect_ok": True,
                "ms": 3.0,
            }
        )
        if abs(t - WARMUP) < EVERY / 2:
            recs.append({"kind": "phase", "t": t, "name": "baseline_start"})
        if abs(t - load) < EVERY / 2:
            recs.append({"kind": "phase", "t": t, "name": "idle_start"})
        t += EVERY
    end = load + IDLE
    recs += [
        {"kind": "shutdown", "t": end + 1, "step": "main_stdin_eof", "exited": True, "secs": 0.2},
        {"kind": "shutdown", "t": end + 2, "step": "host_sigkill", "exited": True, "secs": 0.3},
        {
            "kind": "shutdown",
            "t": end + 3,
            "step": "serve_sigkill",
            "exited": True,
            "daemon_fds_before": 60,
            "daemon_fds_during": 62,
            "daemon_fds_after": 60,
        },
        {"kind": "shutdown", "t": end + 4, "step": "comms_stop", "exited": True, "secs": 0.5},
        {"kind": "survivors", "t": end + 6, "pids": [], "argv": {}},
    ]
    return recs


def climb(recs: list[dict[str, Any]], pid: int, mib_per_min: float) -> list[dict[str, Any]]:
    """Grow one process's memory linearly by `mib_per_min` for the whole run."""
    for r in recs:
        if r.get("kind") == "proc" and r["pid"] == pid:
            extra = mib_per_min * MIB * r["t"] / 60
            r["footprint"] += int(extra)
            r["rss"] += int(extra)
    return recs


def rss_climb(rng: random.Random) -> list[dict[str, Any]]:
    """The daemon gains 3 MiB/min for the whole run: a slow linear leak, judged with the #164 waiver
    lifted (it is inside the block-cache bound)."""
    return climb(healthy(rng), DAEMON, 3.0)


def lsm_cache_fill(rng: random.Random) -> list[dict[str, Any]]:
    """The daemon gains 20 MiB/min through load: 145 MiB over the 7.25 judged minutes, inside the
    296 MiB the lsm-tree block caches may fill with dead blocks (#164), so R2 is waived."""
    return climb(healthy(rng), DAEMON, 20.0)


def lsm_past_bound(rng: random.Random) -> list[dict[str, Any]]:
    """The daemon gains 60 MiB/min: 435 MiB over load, more than its block caches can hold."""
    return climb(healthy(rng), DAEMON, 60.0)


def serve_climb(rng: random.Random) -> list[dict[str, Any]]:
    """The main serve gains 3 MiB/min: it holds no Fjall database, so no waiver applies."""
    return climb(healthy(rng), MAIN, 3.0)


def late_plateau(rng: random.Random) -> list[dict[str, Any]]:
    """A cache keeps filling for three minutes after warm-up, then stops: bounded, so it passes."""
    recs = healthy(rng)
    for r in recs:
        if r.get("kind") == "proc" and r["pid"] == DAEMON:
            extra = 20 * MIB * min(max(r["t"] - WARMUP, 0), 180) / 60
            r["footprint"] += int(extra)
            r["rss"] += int(extra)
    return recs


LONG_LOAD = 3600.0


def long_plateau(rng: random.Random) -> list[dict[str, Any]]:
    """An hour of load where a cache fills for ten minutes after warm-up and then holds: passes."""
    recs = healthy(rng, LONG_LOAD)
    for r in recs:
        if r.get("kind") == "proc" and r["pid"] == DAEMON:
            extra = 30 * MIB * min(max(r["t"] - WARMUP, 0), 600) / 600
            r["footprint"] += int(extra)
            r["rss"] += int(extra)
    return recs


def slow_leak(rng: random.Random) -> list[dict[str, Any]]:
    """An hour of load gaining 0.5 MiB/min: under the slope rule's 1 MiB/min, caught by the floor
    with the #164 waiver lifted."""
    return climb(healthy(rng, LONG_LOAD), DAEMON, 0.5)


def _add_threads(r: dict[str, Any], n: int, name: str) -> None:
    r["threads"] += n
    r["thread_names"][name] = r["thread_names"].get(name, 0) + n


def pool_step(rng: random.Random) -> list[dict[str, Any]]:
    """A blocking pool grows by six threads at a concurrency peak mid-load and reaps them once idle:
    bounded, so it passes (a slope fitted through the step reads +1 thread/min)."""
    recs = healthy(rng)
    for r in recs:
        if r.get("kind") == "proc" and r["phase"] == "load" and r["t"] >= LOAD / 2:
            _add_threads(r, 6, "tokio-runtime-w")
    return recs


def thread_leak(rng: random.Random) -> list[dict[str, Any]]:
    """One thread per minute of load is started and never exits. Its name is invented, so a report
    of this case is not read as a basemind thread."""
    recs = healthy(rng)
    for r in recs:
        if r.get("kind") == "proc" and r["pid"] == DAEMON and r["t"] > WARMUP + QUIESCE:
            _add_threads(r, int(min(r["t"], LOAD) - WARMUP - QUIESCE) // 60, "selftest-leak")
    return recs


def _grow_dir(recs: list[dict[str, Any]], mib_per_min: float, journal: bool, telemetry: bool) -> None:
    for r in recs:
        if r.get("kind") == "dir":
            extra = int(mib_per_min * MIB * r["t"] / 60)
            r["data_bytes"] += extra
            if journal:
                r["data_journals"]["cache/groups/g/index.fjall"] += extra if not telemetry else extra // 2
            if telemetry:
                r["data_telemetry_bytes"] += extra if not journal else extra - extra // 2


def journal_fill(rng: random.Random) -> list[dict[str, Any]]:
    """The index journal and the telemetry log grow 2 MiB/min together, under their size bounds:
    a write-ahead log and a rotated log filling toward their configured caps, so it passes."""
    recs = healthy(rng)
    _grow_dir(recs, 2.0, journal=True, telemetry=True)
    return recs


def journal_overflow(rng: random.Random) -> list[dict[str, Any]]:
    """A Fjall journal climbs 30 MiB/min past its 192 MiB bound: nothing ever truncates it."""
    recs = healthy(rng)
    _grow_dir(recs, 30.0, journal=True, telemetry=False)
    return recs


def store_climb(rng: random.Random) -> list[dict[str, Any]]:
    """A store outside the journals and the telemetry log grows 2 MiB/min: unbounded."""
    recs = healthy(rng)
    _grow_dir(recs, 2.0, journal=False, telemetry=False)
    return recs


def comms_climb(rng: random.Random) -> list[dict[str, Any]]:
    """The daemon's log in the comms dir grows 2 MiB/min between its trims: unbounded in practice
    (soak run 44541580154 grew the comms dir 74 -> 115 MiB over 18 minutes of load on Linux,
    +4.2 MiB/min, 93% of it dependency INFO lines)."""
    recs = healthy(rng)
    for r in recs:
        if r.get("kind") == "dir":
            r["comms_bytes"] += int(2.0 * MIB * r["t"] / 60)
    return recs


def comms_journal_overflow(rng: random.Random) -> list[dict[str, Any]]:
    """The agents store's Fjall journal climbs 30 MiB/min past the journal bound: R8b judges the
    comms journals with the data dir's, while R8c leaves them out of the comms dir's slope."""
    recs = healthy(rng)
    for r in recs:
        if r.get("kind") == "dir":
            extra = int(30.0 * MIB * r["t"] / 60)
            r["comms_bytes"] += extra
            r["comms_journals"]["g1/agents.fjall"] += extra
    return recs


def idle_busy(rng: random.Random) -> list[dict[str, Any]]:
    """A thread spins at 15% of a core while nothing is asked of the daemon."""
    recs = healthy(rng)
    for r in recs:
        if r.get("kind") == "proc" and r["pid"] == DAEMON and r["phase"] == "idle":
            r["cpu_delta_s"] = 0.15 * EVERY
    return recs


def fd_leak(rng: random.Random) -> list[dict[str, Any]]:
    """One descriptor per 20-second cycle is never closed."""
    recs = healthy(rng)
    for r in recs:
        if r.get("kind") == "proc" and r["pid"] == DAEMON:
            r["fds"] += int(r["t"] // 20)
    return recs


def _with_tables(recs: list[dict[str, Any]], per_min: float) -> list[dict[str, Any]]:
    """Count the daemon's live LSM table descriptors, `per_min` more each minute, inside its fds."""
    for r in recs:
        if r.get("kind") == "proc" and r["pid"] == DAEMON:
            grown = int(per_min * r["t"] / 60)
            r["fds_lsm_tables"] = 20 + grown
            r["fds"] += 20 + grown
    return recs


def table_growth(rng: random.Random) -> list[dict[str, Any]]:
    """Live table descriptors climb one per minute as the stores add tables: R8's to judge, so the
    descriptors net of them are flat and R3 passes."""
    return _with_tables(healthy(rng), 1.0)


def table_fd_leak(rng: random.Random) -> list[dict[str, Any]]:
    """The live tables stay put while a descriptor per 20-second cycle leaks beside them."""
    return _with_tables(fd_leak(rng), 0.0)


def table_count_gap(rng: random.Random) -> list[dict[str, Any]]:
    """The daemon's pid is reused by a process whose samples carry no table count halfway through
    load: the series is judged on its flat total fds, so it passes."""
    recs = _with_tables(healthy(rng), 0.0)
    for r in recs:
        if r.get("kind") == "proc" and r["pid"] == DAEMON and r["t"] > (WARMUP + QUIESCE + LOAD) / 2:
            r["fds_lsm_tables"] = None
    return recs


def _on_windows(recs: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Mark the run as sampled on Windows, where `fds` is the process handle count: one handle per
    live thread on top of the descriptors."""
    for r in recs:
        if r.get("kind") == "meta":
            r["platform"] = "Windows-2022Server-10.0.20348-SP0"
        elif r.get("kind") == "proc":
            r["fds"] += r["threads"]
    return recs


def win_pool_step(rng: random.Random) -> list[dict[str, Any]]:
    """On Windows the pool step of `pool-step` also steps the handle count, one per thread, and both
    return at idle (soak run 44175503432: handles 234 -> 253 -> 233 beside threads 40 -> 61 -> 39,
    handles net of threads 192-196 throughout): bounded, so it passes."""
    return _on_windows(pool_step(rng))


def win_handle_leak(rng: random.Random) -> list[dict[str, Any]]:
    """On Windows one handle per 20-second cycle is never closed while the threads stay flat."""
    return _on_windows(fd_leak(rng))


def leaked_child(rng: random.Random) -> list[dict[str, Any]]:
    """A process of the run is still alive after every shutdown step."""
    recs = healthy(rng)
    for r in recs:
        if r.get("kind") == "survivors":
            r["pids"] = [7777]
            r["argv"] = {"7777": ["sleep", "600"]}
    return recs


def checked_nothing(_rng: random.Random) -> list[dict[str, Any]]:
    """Only a meta record: a run that sampled nothing must not pass."""
    return [{"kind": "meta", "t": 0, "duration": LOAD, "warmup": WARMUP, "idle": IDLE, "sample_every": EVERY}]


CASES: list[tuple[str, Callable[[random.Random], list[dict[str, Any]]], int, set[str]]] = [
    ("healthy", healthy, 0, set()),
    ("late-plateau", late_plateau, 0, set()),
    ("long-plateau", long_plateau, 0, set()),
    ("slow-leak", slow_leak, 1, {"R2f"}),
    ("rss-climb", rss_climb, 1, {"R2"}),
    ("lsm-cache-fill", lsm_cache_fill, 0, set()),
    ("lsm-past-bound", lsm_past_bound, 1, {"R2"}),
    ("serve-climb", serve_climb, 1, {"R2"}),
    ("pool-step", pool_step, 0, set()),
    ("thread-leak", thread_leak, 1, {"R4"}),
    ("journal-fill", journal_fill, 0, set()),
    ("journal-overflow", journal_overflow, 1, {"R8"}),
    ("store-climb", store_climb, 1, {"R8"}),
    ("comms-climb", comms_climb, 1, {"R8"}),
    ("comms-journal-overflow", comms_journal_overflow, 1, {"R8"}),
    ("idle-busy", idle_busy, 1, {"R6"}),
    ("fd-leak", fd_leak, 1, {"R3"}),
    ("table-growth", table_growth, 0, set()),
    ("table-fd-leak", table_fd_leak, 1, {"R3"}),
    ("table-count-gap", table_count_gap, 0, set()),
    ("win-pool-step", win_pool_step, 0, set()),
    ("win-handle-leak", win_handle_leak, 1, {"R3"}),
    ("leaked-child", leaked_child, 1, {"R7"}),
    ("checked-nothing", checked_nothing, 1, {"R0", "R2", "R3", "R4", "R5", "R6", "R7", "R8", "R9"}),
]


# The cases that model a leak inside the #164 block-cache bound, judged as they will be once the
# waiver is lifted.
NO_WAIVER = ["--lsm-cache-mib", "0"]
CASE_ARGS: dict[str, list[str]] = {"rss-climb": NO_WAIVER, "slow-leak": NO_WAIVER}


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--out", type=Path)
    args = ap.parse_args()
    out = args.out or default_out_root() / time.strftime("selftest-%Y%m%d-%H%M%S")
    out.mkdir(parents=True, exist_ok=True)
    failures = 0
    for name, make, want_exit, want_rules in CASES:
        path = out / f"{name}.ndjson"
        path.write_text("".join(json.dumps(r) + "\n" for r in make(random.Random(11))))
        verdict_path = out / f"{name}.verdict.json"
        proc = subprocess.run(  # noqa: S603
            [
                sys.executable,
                str(HERE / "analyze.py"),
                str(path),
                "--json",
                str(verdict_path),
                *CASE_ARGS.get(name, []),
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        verdict = json.loads(verdict_path.read_text())
        failed = sorted({r["name"].split()[0].rstrip("bcd") for r in verdict["rules"] if not r["passed"]})
        unexpected = [f for f in failed if f not in want_rules]
        missing = (
            [w for w in want_rules if w not in failed]
            if name != "checked-nothing"
            else (["R0"] if "R0" not in failed else [])
        )
        good = proc.returncode == want_exit and not unexpected and not missing
        failures += 0 if good else 1
        detail = "; ".join(r["detail"] for r in verdict["rules"] if not r["passed"])[:400]
        sys.stdout.write(
            f"{'ok  ' if good else 'BAD '} {name:16} exit={proc.returncode} (want {want_exit}) failed rules={failed} "
            f"(want {sorted(want_rules) or 'none'})\n      {detail}\n"
        )
    sys.stdout.write(f"\n{len(CASES) - failures}/{len(CASES)} cases behaved as expected; files in {out}\n")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
