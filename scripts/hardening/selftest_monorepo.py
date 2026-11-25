#!/usr/bin/env python3
"""Prove the large-monorepo soak's pieces on known input: the pinned corpus parses and refuses a
malformed row, `rewritten_bytes` and the index.msgpack watcher count what they should, a worktree
is set up as the scenarios assume (real git, a tiny repository), and analyze_monorepo.py passes a
healthy run and fails each rule on the one measurement that breaks it.

Usage:
    selftest_monorepo.py
"""

from __future__ import annotations

import copy
import os
import subprocess
import sys
import tempfile
from collections.abc import Callable
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

from analyze import rule_verdict
from analyze_monorepo import analyze, build_parser
from monorepo_soak import (
    CORPUS,
    MSGPACK,
    MsgpackWatcher,
    add_worktree,
    git,
    parse_corpus,
    rewritten_bytes,
    validate,
)
from monorepo_soak import build_parser as soak_parser

MIB = 1024 * 1024
COMMIT = "0123456789abcdef0123456789abcdef01234567"
WORKTREES = 20


def healthy() -> list[dict[str, Any]]:
    """The records of a run that passes every rule."""
    records: list[dict[str, Any]] = [{"kind": "meta", "repo": "demo", "worktrees": WORKTREES}]
    records += [{"kind": "proc", "role": "daemon", "rss": 1024 * MIB, "phase": "load"} for _ in range(4)]
    records += [
        {"kind": "dir", "phase": "load", "t": 60.0 * i, "data_bytes": 100 * MIB + 1000 * (i % 3), "data_journals": {}}
        for i in range(10)
    ]
    records.append({"kind": "scan", "ok": True, "data_bytes": 100 * MIB, "secs": 60})
    records += [
        {"kind": "worktree", "index": i, "ok": True, "added_bytes": 10 * MIB, "secs": 5} for i in range(WORKTREES)
    ]
    records += [{"kind": "call", "phase": "load", "tool": "index_status", "ok": True, "ms": 20} for _ in range(200)]
    records += [
        {
            "kind": "save",
            "n": n,
            "ok": True,
            "ms": 50,
            "rewritten_bytes": {"total": MIB, MSGPACK: 0},
            "stale_visible": True,
            "stale_s": 1.0,
        }
        for n in range(10)
    ]
    records += [
        {"kind": "switch", "index": i, "git_ok": True, "visible": True, "visible_s": 2.0} for i in range(WORKTREES)
    ]
    records.append(
        {"kind": "gc", "exit_code": 0, "secs": 3.0, "removed": 5, "workspaces_before": 21, "workspaces_after": 16}
    )
    records.append(
        {
            "kind": "msgpack_total",
            "bytes": 100 * MIB,
            "written_lower": 150 * MIB,
            "written_upper": 150 * MIB,
            "seconds": 900,
            "rewrites": 40,
            "files": 21,
        }
    )
    records.append({"kind": "survivors", "pids": []})
    return records


def failing(records: list[dict[str, Any]]) -> list[str]:
    """The names of the rules that fail on `records`."""
    verdict = analyze(records, build_parser().parse_args(["unused.ndjson"]))
    return [r.name.split(" ")[0] for r in verdict.rules if not r.passed]


def mutated(change: Callable[[list[dict[str, Any]]], None]) -> list[str]:
    records = copy.deepcopy(healthy())
    change(records)
    return failing(records)


def m9_cell(change: Callable[[list[dict[str, Any]]], None]) -> str:
    """The verdict cell M9 gets on the healthy run after `change`."""
    records = copy.deepcopy(healthy())
    change(records)
    verdict = analyze(records, build_parser().parse_args(["unused.ndjson"]))
    return next(rule_verdict(r) for r in verdict.rules if r.name.startswith("M9"))


def each(records: list[dict[str, Any]], kind: str) -> list[dict[str, Any]]:
    return [r for r in records if r["kind"] == kind]


def set_all(records: list[dict[str, Any]], kind: str, **fields: Any) -> None:
    for r in each(records, kind):
        r.update(fields)


def keep(records: list[dict[str, Any]], kind: str, count: int) -> None:
    """Leave only the first `count` records of `kind`."""
    extra = each(records, kind)[count:]
    records[:] = [r for r in records if not any(r is e for e in extra)]


def drop(records: list[dict[str, Any]], kind: str) -> None:
    records[:] = [r for r in records if r["kind"] != kind]


def climbing(records: list[dict[str, Any]]) -> None:
    for i, r in enumerate(each(records, "dir")):
        r["data_bytes"] = 100 * MIB + 10 * MIB * i


def one_busy_in_ten(records: list[dict[str, Any]]) -> None:
    for r in each(records, "call")[::10]:
        r.update(ok=False, error="server_busy")


def corpus_error(text: str) -> str:
    try:
        parse_corpus(text)
    except ValueError as exc:
        return str(exc)
    return ""


def tab(*fields: str) -> str:
    return "\t".join(fields) + "\n"


def watcher_counts() -> tuple[int, int]:
    with tempfile.TemporaryDirectory() as tmp:
        view = Path(tmp) / "cache" / "workspaces" / "k" / "views" / "working"
        view.mkdir(parents=True)
        target = view / MSGPACK
        target.write_bytes(b"x" * 100)
        os.utime(target, ns=(1_000, 1_000))
        watcher = MsgpackWatcher(Path(tmp))
        watcher.tick()
        watcher.tick()
        target.write_bytes(b"y" * 250)
        os.utime(target, ns=(2_000, 2_000))
        watcher.tick()
        target.write_bytes(b"z" * 300)
        os.utime(target, ns=(2_000, 2_000))
        watcher.tick()
        return watcher.bytes, watcher.rewrites


def write_accounting() -> tuple[object, ...]:
    """The watcher's daemon write total: summed over pids, never shrinking, skipping an unreadable pid."""
    readings = iter([{1: 500, 2: 70}, {1: 900, 2: 70}, {1: 400}, {1: 900}])
    current: dict[int, int] = {}

    def counter(pid: int) -> int | None:
        return current.get(pid)

    with tempfile.TemporaryDirectory() as tmp:
        watcher = MsgpackWatcher(Path(tmp), daemon_pids=lambda: (1, 2, 3), counter=counter)
        before = watcher.total()["written_lower"]
        totals = []
        for reading in readings:
            current.clear()
            current.update(reading)
            watcher.poll_counters()
            totals.append(watcher.total()["written_lower"])
        return before, *totals


def load_window(readings: list[dict[int, int]], pids: tuple[int, ...] = (1,)) -> dict[str, Any]:
    """The watcher's total after 3600 s of setup, `begin_load`, then 900 s of load with one counter
    reading set per entry of `readings` (the first is read at the start of the load)."""
    now = [0.0]
    current: dict[int, int] = {}

    def counter(pid: int) -> int | None:
        return current.get(pid)

    with tempfile.TemporaryDirectory() as tmp:
        watcher = MsgpackWatcher(Path(tmp), daemon_pids=lambda: pids, counter=counter, clock=lambda: now[0])
        now[0] = 3600.0
        current.update(readings[0])
        watcher.begin_load()
        now[0] += 900.0
        for reading in readings[1:]:
            current.clear()
            current.update(reading)
            watcher.poll_counters()
        return watcher.total()


def daemon_timeline(setup: dict[int, int], at_begin: dict[int, int], after: dict[int, int]) -> dict[str, Any]:
    """The watcher's total when the counters read `setup` before `begin_load`, `at_begin` at it, and `after`
    later; every pid in any of them is a known daemon from the poll that reads it."""
    current: dict[int, int] = {}
    known: set[int] = set()

    def counter(pid: int) -> int | None:
        return current.get(pid)

    with tempfile.TemporaryDirectory() as tmp:
        watcher = MsgpackWatcher(Path(tmp), daemon_pids=lambda: known, counter=counter)
        for reading, begin in ((setup, False), (at_begin, True), (after, False)):
            current.clear()
            current.update(reading)
            known.update(reading)
            if begin:
                watcher.begin_load()
            else:
                watcher.poll_counters()
        return watcher.total()


def written_bounds(total: dict[str, Any]) -> tuple[int | None, int | None]:
    return total["written_lower"], total["written_upper"]


def m9_from_watcher(total: dict[str, Any], polled_mib: int = 0) -> str:
    """The M9 cell for a watcher total, with `polled_mib` of polled rewrites on top."""
    total = {**{k: v for k, v in total.items() if k != "kind"}, "bytes": polled_mib * MIB}
    return m9_cell(lambda r: set_all(r, "msgpack_total", **total))


def rewritten_counts() -> dict[str, int]:
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        old, fresh, packed = root / "old.bin", root / "fresh.bin", root / "v" / MSGPACK
        packed.parent.mkdir()
        for path, size, ns in ((old, 10, 1_000), (fresh, 20, 5_000), (packed, 30, 6_000)):
            path.write_bytes(b"z" * size)
            os.utime(path, ns=(ns, ns))
        return rewritten_bytes(root, 5_000)


def worktree_shape() -> tuple[object, ...]:
    with tempfile.TemporaryDirectory() as tmp:
        repo, dest = Path(tmp) / "repo", Path(tmp) / "wt"
        (repo / "keep").mkdir(parents=True)
        (repo / "skip").mkdir()
        subprocess.run(["git", "init", "-q", str(repo)], check=True)
        (repo / "keep" / "a.py").write_text("def a():\n    pass\n")
        (repo / "skip" / "b.py").write_text("def b():\n    pass\n")
        (repo / "root.txt").write_text("root\n")
        git(repo, "add", "-A")
        git(repo, "commit", "--quiet", "-m", "init")
        files = add_worktree(repo, dest, ("keep",), 3)
        on_a = git(dest, "branch", "--show-current").stdout.strip()
        listed = git(dest, "branch", "--format=%(refname:short)").stdout.split()
        branches = sorted(b for b in listed if b.startswith("soak-"))
        b_has = "soak_branch_b_3" in git(dest, "show", "soak-3-b:soak_branch.py").stdout
        a_has = "soak_branch_a_3" in (dest / "soak_branch.py").read_text()
        return files, (dest / "keep" / "a.py").exists(), (dest / "skip").exists(), on_a, branches, a_has, b_has


def validation(**overrides: Any) -> str | None:
    args = soak_parser().parse_args(["--binary", sys.executable, "--name", "demo"])
    for key, value in overrides.items():
        setattr(args, key, value)
    return validate(args)


ROW = tab("demo", "https://example.invalid/demo.git", COMMIT, "1", "src,lib")
LARGE_PINNED = {"llvm-project", "kibana", "tensorflow", "terraform-provider-aws"}

CASES: list[tuple[str, Callable[[], object], object]] = [
    (
        "the shipped corpus names the four repositories",
        lambda: {r.name for r in parse_corpus(CORPUS.read_text())},
        LARGE_PINNED,
    ),
    (
        "every shipped pin is a full commit at depth 1",
        lambda: {(len(r.commit), r.depth) for r in parse_corpus(CORPUS.read_text())},
        {(40, 1)},
    ),
    ("a corpus row", lambda: parse_corpus(ROW)[0].worktree_paths, ("src", "lib")),
    (
        "a short commit is refused",
        lambda: "not a full commit id" in corpus_error(tab("d", "https://x", "abc", "1", "p")),
        True,
    ),
    ("a missing field is refused", lambda: "want 5" in corpus_error("d\thttps://x\n"), True),
    ("a duplicate is refused", lambda: "listed twice" in corpus_error(ROW + ROW), True),
    ("a depth of zero is refused", lambda: corpus_error(tab("d", "https://x", COMMIT, "0", "p")) != "", True),
    ("a non-https URL is refused", lambda: corpus_error(tab("d", "git://x", COMMIT, "1", "p")) != "", True),
    ("bytes rewritten since a time", rewritten_counts, {"total": 50, MSGPACK: 30}),
    ("the watcher counts a rewrite once, not the first sight, and a same-mtime resize", watcher_counts, (550, 2)),
    ("daemon write totals sum pids, never shrink, skip unreadable pids", write_accounting, (None, 570, 970, 970, 970)),
    ("a lower bound and an upper bound under the ceiling pass M9", lambda: m9_cell(lambda r: None), "pass"),
    (
        "no write counter leaves M9 not measured, not failed",
        lambda: m9_cell(lambda r: set_all(r, "msgpack_total", written_upper=None)),
        "not measured",
    ),
    (
        "a daemon writing over the ceiling in all leaves M9 not measured",
        lambda: m9_cell(lambda r: set_all(r, "msgpack_total", written_upper=2560 * MIB)),
        "not measured",
    ),
    (
        "a polled lower bound over the ceiling fails M9 even with no counter",
        lambda: m9_cell(lambda r: set_all(r, "msgpack_total", bytes=2560 * MIB, written_upper=None)),
        "**FAIL**",
    ),
    (
        "the load window excludes the setup before it and counter writes before it",
        lambda: {
            k: load_window([{1: 100 * MIB}, {1: 2100 * MIB}])[k] for k in ("seconds", "written_lower", "written_upper")
        },
        {"seconds": 900.0, "written_lower": 2000 * MIB, "written_upper": 2000 * MIB},
    ),
    (
        "2000 MiB of rewrites in 900 s of load after 3600 s of setup fails M9 at 8000 MiB/h",
        lambda: m9_from_watcher(load_window([{1: 0}, {1: 2000 * MIB}]), polled_mib=2000),
        "**FAIL**",
    ),
    (
        "a 1 MiB index rewritten every 0.5 s, polled every 5 s (1 MiB seen per poll), is not passed",
        lambda: m9_from_watcher(
            {**load_window([{1: 0}], pids=()), "seconds": 3600.0, "written_lower": None, "written_upper": None},
            polled_mib=720,
        ),
        "not measured",
    ),
    (
        "a daemon unreadable at the end leaves no upper bound, so 1000 MiB over 900 s is not passed",
        lambda: (
            load_window([{1: 0}, {1: 1000 * MIB}, {}])["written_upper"],
            m9_from_watcher(load_window([{1: 0}, {1: 1000 * MIB}, {}])),
        ),
        (None, "not measured"),
    ),
    (
        "a daemon read to the end keeps the upper bound, and 1000 MiB over 900 s passes",
        lambda: m9_from_watcher(load_window([{1: 0}, {1: 1000 * MIB}])),
        "pass",
    ),
    (
        "a daemon first read after the load began counts all its writes, from a zero baseline",
        lambda: written_bounds(daemon_timeline({}, {}, {1: 16 * MIB})),
        (16 * MIB, 16 * MIB),
    ),
    (
        "a daemon first read after the load began, beside one baselined at its start, adds to it",
        lambda: written_bounds(daemon_timeline({1: 100 * MIB}, {1: 100 * MIB}, {1: 150 * MIB, 2: 16 * MIB})),
        (66 * MIB, 66 * MIB),
    ),
    (
        "a daemon read during setup but unreadable at the load start has unknown load writes, so no bound",
        lambda: written_bounds(daemon_timeline({1: 4096 * MIB}, {}, {1: 4112 * MIB})),
        (None, None),
    ),
    (
        "an unbaselined daemon is left out of the lower bound but still voids the upper bound",
        lambda: written_bounds(
            daemon_timeline({1: 10 * MIB, 2: 4096 * MIB}, {1: 10 * MIB}, {1: 30 * MIB, 2: 4112 * MIB})
        ),
        (20 * MIB, None),
    ),
    ("no write counter fails no rule", lambda: mutated(lambda r: set_all(r, "msgpack_total", written_upper=None)), []),
    (
        "a worktree is sparse, on branch a, with both branches",
        worktree_shape,
        (4, True, False, "soak-3-a", ["soak-3-a", "soak-3-b"], True, True),
    ),
    (
        "a runnable setup",
        lambda: validation(),
        "the large-monorepo soak runs on Linux and macOS only" if sys.platform == "win32" else None,
    ),
    ("fewer sessions than worktrees", lambda: validation(sessions=5) is not None, True),
    ("retiring every worktree", lambda: validation(retire=20) is not None, True),
    ("a healthy run fails nothing", lambda: failing(healthy()), []),
    ("daemon over its RSS limit", lambda: mutated(lambda r: set_all(r, "proc", rss=3000 * MIB)), ["M1"]),
    ("no daemon sampled", lambda: mutated(lambda r: drop(r, "proc")), ["M1"]),
    ("saves over 200 ms", lambda: mutated(lambda r: set_all(r, "save", ms=450)), ["M2"]),
    ("three saves are too few", lambda: mutated(lambda r: keep(r, "save", 3)), ["M2", "M7"]),
    (
        "a worktree adding 80% of the first scan",
        lambda: mutated(lambda r: each(r, "worktree")[3].update(added_bytes=80 * MIB)),
        ["M3"],
    ),
    ("a worktree that never answered", lambda: mutated(lambda r: each(r, "worktree")[3].update(ok=False)), ["M3"]),
    ("a climbing data dir", lambda: mutated(climbing), ["M4"]),
    ("index_status at 5 s", lambda: mutated(lambda r: set_all(r, "call", ms=5000)), ["M5"]),
    ("one call in ten busy", lambda: mutated(one_busy_in_ten), ["M5", "M6"]),
    (
        "one failing call in 200 is under 1%",
        lambda: mutated(lambda r: each(r, "call")[0].update(ok=False, error="x")),
        [],
    ),
    (
        "a tenth of the calls failing",
        lambda: mutated(lambda r: [c.update(ok=False, error="x") for c in each(r, "call")[::10]]),
        ["M6"],
    ),
    (
        "an edit that never became answerable",
        lambda: mutated(lambda r: each(r, "save")[0].update(stale_visible=False, stale_s=60)),
        ["M7"],
    ),
    (
        "a branch switch answered after 90 s",
        lambda: mutated(lambda r: each(r, "switch")[0].update(visible_s=90)),
        ["M7"],
    ),
    (
        "a branch switch never answered",
        lambda: mutated(lambda r: each(r, "switch")[0].update(visible=False, visible_s=120)),
        ["M7"],
    ),
    ("a sweep over two minutes", lambda: mutated(lambda r: set_all(r, "gc", secs=300)), ["M8"]),
    ("a sweep that reaps nothing", lambda: mutated(lambda r: set_all(r, "gc", workspaces_after=21)), ["M8"]),
    ("no sweep recorded", lambda: mutated(lambda r: drop(r, "gc")), ["M8"]),
    (
        "index.msgpack written at 10 GiB/h",
        lambda: mutated(lambda r: set_all(r, "msgpack_total", bytes=2560 * MIB)),
        ["M9"],
    ),
    ("a survivor", lambda: mutated(lambda r: set_all(r, "survivors", pids=[4242])), ["M10"]),
]


def main() -> int:
    failures = 0
    for name, run, want in CASES:
        got = run()
        good = got == want
        failures += 0 if good else 1
        detail = "" if good else f": got {got!r}, want {want!r}"
        sys.stdout.write(f"{'ok  ' if good else 'BAD '} {name}{detail}\n")
    sys.stdout.write(f"\n{len(CASES) - failures}/{len(CASES)} cases behaved as expected\n")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
