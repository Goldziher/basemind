#!/usr/bin/env python3
"""Prove leak_check.py's judging without a leak checker: synthetic valgrind reports and a run that
outlives the run timeout.

- A definitely-lost record that points at indirect blocks is counted and named by the frame that
  owns it, past the allocator and Rust's allocation shims.
- A clean report passes; a report with no leak summary is a run the checker could not inspect.
- A run killed at the run timeout fails on the timeout, with what it wrote until then.

Usage:
    selftest_leak_check.py
"""

from __future__ import annotations

import os
import sys
from pathlib import Path
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from collections.abc import Callable

sys.path.insert(0, str(Path(__file__).resolve().parent))

from leak_check import judge, judge_valgrind, run_bounded, valgrind_sites  # noqa: E402

PID = "==4242=="
BIN = "(in /opt/basemind)"
INDIRECT_RECORD = f"""{PID} 28,703 (264 direct, 28,439 indirect) bytes in 1 blocks are definitely lost in loss record 9 of 12
{PID}    at 0x4848899: malloc (vg_replace_malloc.c:447)
{PID}    by 0xA236F19: alloc::boxed::box_new_uninit {BIN}
{PID}    by 0xA225873: <fjall::db::Database>::create_new {BIN}
{PID}    by 0x87F68EC: <basemind::git_history::GitHistoryIndex>::open_retrying {BIN}
{PID}    by 0x8E3F0D4: basemind::cmd_scan {BIN}
{PID}
"""
DIRECT_RECORD = f"""{PID} 48 bytes in 2 blocks are definitely lost in loss record 3 of 12
{PID}    at 0x4848899: malloc (vg_replace_malloc.c:447)
{PID}    by 0x1000001: __rustc::__rust_alloc {BIN}
{PID}    by 0x1000002: <alloc::raw_vec::RawVec<u8>>::with_capacity {BIN}
{PID}    by 0x1000003: basemind::lang::cache_query {BIN}
{PID}
"""
SUMMARY = f"""{PID} LEAK SUMMARY:
{PID}    definitely lost: 312 bytes in 3 blocks
{PID}    indirectly lost: 28,439 bytes in 203 blocks
"""
CLEAN = f"{PID} All heap blocks were freed -- no leaks are possible\n"


def check_sites() -> list[str]:
    got = valgrind_sites(INDIRECT_RECORD + DIRECT_RECORD + SUMMARY)
    want = [
        ("basemind::lang::cache_query -> malloc", 2),
        ("<fjall::db::Database>::create_new -> malloc", 1),
    ]
    return [] if got == want else [f"sites {got}, want {want}"]


def check_verdicts() -> list[str]:
    leaked = {"timed_out": False, "secs": 9.0, "inspected": True, "leaks": 3, "bytes": 312}
    clean = {"timed_out": False, "secs": 9.0, "inspected": True, "leaks": 0, "bytes": 0}
    blind = {"timed_out": False, "secs": 9.0, "inspected": False, "leaks": None, "bytes": None}
    cases = [
        (judge_valgrind(leaked), ["3 blocks (312 bytes) definitely lost"]),
        (judge_valgrind(clean), []),
        (judge_valgrind(blind), ["valgrind could not inspect the process"]),
        (judge(blind), ["leaks could not inspect the process"]),
    ]
    return [f"verdict {got}, want {want}" for got, want in cases if got != want]


def check_timeout() -> list[str]:
    sleeper = [sys.executable, "-c", "import sys, time; print('started', flush=True); time.sleep(60)"]
    code, stdout, _ = run_bounded(sleeper, Path.cwd(), dict(os.environ), 2)
    problems = [] if code is None else [f"exit {code} from a run past its timeout, want None"]
    if stdout.strip() != "started":
        problems.append(f"stdout {stdout!r} of the killed run, want 'started'")
    killed = {"timed_out": code is None, "secs": 2.0, "inspected": False, "leaks": None, "bytes": None}
    want = ["killed at the run timeout after 2.0 s, before the checker reported"]
    for name, verdict in (("valgrind", judge_valgrind(killed)), ("leaks", judge(killed))):
        if verdict != want:
            problems.append(f"{name} verdict {verdict}, want {want}")
    return problems


CHECKS: list[tuple[str, Callable[[], list[str]]]] = [
    ("allocation sites", check_sites),
    ("verdicts", check_verdicts),
    ("run timeout", check_timeout),
]


def main() -> int:
    failures = 0
    for name, check in CHECKS:
        problems = check()
        failures += 1 if problems else 0
        sys.stdout.write(f"{'BAD ' if problems else 'ok  '} {name}\n")
        sys.stdout.writelines(f"      {p}\n" for p in problems)
    sys.stdout.write(f"\n{len(CHECKS) - failures}/{len(CHECKS)} checks behaved as expected\n")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
