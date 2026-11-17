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
WARMUP, LOAD, IDLE = 120.0, 600.0, 180.0
DAEMON, MAIN = 4242, 4343


def healthy(rng: random.Random) -> list[dict[str, Any]]:
    """A run whose memory ramps during warm-up then plateaus with noise, and whose idle is quiet."""
    recs: list[dict[str, Any]] = [
        {
            "kind": "meta",
            "t": 0,
            "binary": "synthetic",
            "version": "synthetic",
            "duration": LOAD,
            "warmup": WARMUP,
            "idle": IDLE,
            "sample_every": EVERY,
        },
        {"kind": "session", "event": "open", "name": "main-1", "pid": MAIN, "t": 1},
    ]
    t = 2.0
    cpu = {DAEMON: 0.0, MAIN: 0.0}
    short_pid = 9000
    while t < LOAD + IDLE:
        phase = "warmup" if t < WARMUP else "load" if t < LOAD else "idle"
        for pid, role, base in ((DAEMON, "daemon", 400), (MAIN, "serve", 20)):
            ramp = min(t, WARMUP) / WARMUP
            mem = (base * (0.5 + 0.5 * ramp) + rng.gauss(0, base * 0.03)) * MIB
            busy = 0.4 if phase != "idle" else 0.0005
            delta = busy * EVERY * rng.uniform(0.5, 1.5)
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
                    "threads": 30 + rng.choice((0, 0, 1, 2)),
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
                "comms_bytes": 0,
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
        if phase != "idle" and int(t) % 20 == 2:
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
        if abs(t - LOAD) < EVERY / 2:
            recs.append({"kind": "phase", "t": t, "name": "idle_start"})
        t += EVERY
    end = LOAD + IDLE
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


def rss_climb(rng: random.Random) -> list[dict[str, Any]]:
    """The daemon gains 3 MiB/min for the whole run: a slow linear leak."""
    recs = healthy(rng)
    for r in recs:
        if r.get("kind") == "proc" and r["pid"] == DAEMON:
            extra = 3 * MIB * r["t"] / 60
            r["footprint"] += int(extra)
            r["rss"] += int(extra)
    return recs


def late_plateau(rng: random.Random) -> list[dict[str, Any]]:
    """A cache keeps filling for three minutes after warm-up, then stops: bounded, so it passes."""
    recs = healthy(rng)
    for r in recs:
        if r.get("kind") == "proc" and r["pid"] == DAEMON:
            extra = 20 * MIB * min(max(r["t"] - WARMUP, 0), 180) / 60
            r["footprint"] += int(extra)
            r["rss"] += int(extra)
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
    ("rss-climb", rss_climb, 1, {"R2"}),
    ("idle-busy", idle_busy, 1, {"R6"}),
    ("fd-leak", fd_leak, 1, {"R3"}),
    ("leaked-child", leaked_child, 1, {"R7"}),
    ("checked-nothing", checked_nothing, 1, {"R0", "R2", "R3", "R4", "R5", "R6", "R7", "R8", "R9"}),
]


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
            [sys.executable, str(HERE / "analyze.py"), str(path), "--json", str(verdict_path)],
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
