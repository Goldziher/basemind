#!/usr/bin/env python3
"""Soak a release basemind build on a large public repository with many linked worktrees.

One pinned repository (large_repos.tsv, fetched with the harden harness's pinned fetch), scanned
once, then `--worktrees` linked worktrees each opened by their own session, then `--sessions`
concurrent sessions looping over cheap tools while three scenarios run beside them:

- save fan-out: one file every worktree also holds is saved in one worktree, `--saves` times, with an
  explicit rescan (the cost of a save, the bytes the stores rewrite) and without one (how long the
  watcher takes to make the edit answerable);
- branch switch: every worktree switches branch at the same moment, and each is asked until its
  answers follow the new branch;
- retire: a few worktrees are removed and `basemind cache gc` reaps their views.

The records go to NDJSON for analyze_monorepo.py, with the sampler of soak.py (daemon RSS, data
directory) running throughout. Linux and macOS only.

Usage:
    monorepo_soak.py --binary target/release/basemind --name llvm-project [--worktrees 20] [--sessions 30]
"""

from __future__ import annotations

import argparse
import contextlib
import json
import os
import random
import re
import subprocess
import sys
import threading
import time
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

from common import (
    IS_WINDOWS,
    Isolation,
    McpError,
    McpSession,
    Ndjson,
    ToolResult,
    binary_version,
    default_out_root,
    dir_usage,
    host_info,
    install_cleanup,
    log,
    owned_processes,
    role_of,
    run_cli,
    terminate_owned,
)
from soak import Sampler

HERE = Path(__file__).resolve().parent
CORPUS = HERE / "large_repos.tsv"
SHA1 = re.compile(r"^[0-9a-f]{40}$")
GIT_QUIET = (
    "git",
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "user.name=soak",
    "-c",
    "user.email=soak@example.invalid",
    "-c",
    "commit.gpgsign=false",
)
SHARED_PY = '"""Identical in every worktree."""\n\n\ndef soak_shared():\n    return 0\n'
MSGPACK = "index.msgpack"
WATCH_EVERY_S = 0.5


@dataclass(frozen=True)
class CorpusRow:
    """One pinned repository of large_repos.tsv."""

    name: str
    url: str
    commit: str
    depth: int
    worktree_paths: tuple[str, ...]


def parse_corpus(text: str) -> list[CorpusRow]:
    """The rows of a corpus table; a malformed or duplicate row raises ValueError."""
    rows: list[CorpusRow] = []
    for number, line in enumerate(text.splitlines(), 1):
        if not line.strip() or line.startswith("#"):
            continue
        fields = line.split("\t")
        if len(fields) != 5:
            msg = f"line {number}: {len(fields)} fields, want 5"
            raise ValueError(msg)
        name, url, commit, depth, paths = fields
        if not SHA1.fullmatch(commit):
            msg = f"line {number}: {name}: {commit!r} is not a full commit id"
            raise ValueError(msg)
        if not url.startswith("https://") or not depth.isdigit() or int(depth) < 1 or not paths:
            msg = f"line {number}: {name}: want an https URL, a positive depth and worktree paths"
            raise ValueError(msg)
        if any(r.name == name for r in rows):
            msg = f"line {number}: {name} is listed twice"
            raise ValueError(msg)
        rows.append(CorpusRow(name, url, commit, int(depth), tuple(p for p in paths.split(",") if p)))
    return rows


def rewritten_bytes(root: Path, since_ns: int) -> dict[str, int]:
    """Bytes of the files under `root` modified at or after `since_ns`: an upper bound on what the
    stores wrote since then, `index.msgpack` counted apart."""
    out = {"total": 0, MSGPACK: 0}
    for dirpath, _dirs, files in os.walk(root):
        for name in files:
            try:
                st = os.lstat(os.path.join(dirpath, name))
            except OSError:
                continue
            if st.st_mtime_ns >= since_ns:
                out["total"] += st.st_size
                if name == MSGPACK:
                    out[MSGPACK] += st.st_size
    return out


class MsgpackWatcher(threading.Thread):
    """Counts the bytes of every `index.msgpack` under the data home each time a poll finds it replaced
    or modified (a new inode, mtime or size). Rewrites that land between two polls collapse into one,
    so the total is a lower bound on what was written, tighter the shorter the poll."""

    def __init__(self, root: Path, every: float = WATCH_EVERY_S) -> None:
        super().__init__(name="msgpack-watcher", daemon=True)
        self.root, self.every = root, every
        self.stop_event = threading.Event()
        self.seen: dict[str, tuple[int, int, int]] = {}
        self.bytes = 0
        self.rewrites = 0
        self.started = time.monotonic()

    def tick(self) -> None:
        for dirpath, _dirs, files in os.walk(self.root):
            if MSGPACK not in files:
                continue
            path = os.path.join(dirpath, MSGPACK)
            try:
                st = os.stat(path)
            except OSError:
                continue
            previous = self.seen.get(path)
            signature = (st.st_ino, st.st_mtime_ns, st.st_size)
            self.seen[path] = signature
            if previous is not None and previous != signature:
                self.bytes += st.st_size
                self.rewrites += 1

    def run(self) -> None:
        self.tick()
        while not self.stop_event.wait(self.every):
            self.tick()

    def total(self) -> dict[str, Any]:
        return {
            "kind": "msgpack_total",
            "bytes": self.bytes,
            "rewrites": self.rewrites,
            "files": len(self.seen),
            "seconds": round(time.monotonic() - self.started, 1),
        }


def git(cwd: Path, *args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    """Run git in `cwd` with hooks off and a fixed committer (a clone is untrusted data)."""
    return subprocess.run([*GIT_QUIET, *args], cwd=cwd, capture_output=True, text=True, check=check)


def branch_source(kind: str, index: int) -> str:
    return f'"""Branch {kind} of worktree {index}."""\n\n\ndef soak_branch_{kind}_{index}():\n    return {index}\n'


def count_files(root: Path) -> int:
    """Files on disk under `root` other than its `.git` (a sparse worktree tracks more than it holds)."""
    return sum(len([f for f in files if f != ".git"]) for _dirpath, _dirs, files in os.walk(root))


def add_worktree(repo: Path, dest: Path, paths: tuple[str, ...], index: int) -> int:
    """A sparse linked worktree on branch `soak-<i>-a`, with branch `soak-<i>-b` beside it; the
    count of its files. Both branches hold the shared file; each holds its own branch file."""
    git(repo, "worktree", "add", "--quiet", "--detach", "--no-checkout", str(dest), "HEAD")
    git(dest, "sparse-checkout", "set", "--cone", *paths)
    git(dest, "checkout", "--quiet", "-B", f"soak-{index}-a", "HEAD")
    (dest / "soak_shared.py").write_text(SHARED_PY)
    for kind in ("a", "b"):
        if kind == "b":
            git(dest, "checkout", "--quiet", "-b", f"soak-{index}-b")
        (dest / "soak_branch.py").write_text(branch_source(kind, index))
        git(dest, "add", "soak_shared.py", "soak_branch.py")
        git(dest, "commit", "--quiet", "--no-verify", "-m", f"soak branch {kind}")
    git(dest, "checkout", "--quiet", f"soak-{index}-a")
    return count_files(dest)


def workspace_dirs(data_home: Path) -> int:
    root = data_home / "cache" / "workspaces"
    return sum(1 for p in root.iterdir() if p.is_dir()) if root.is_dir() else 0


class Rig:
    """The run: its sessions, the shared recorder and the scenarios."""

    def __init__(self, args: argparse.Namespace, iso: Isolation, sink: Ndjson, sampler: Sampler, out: Path) -> None:
        self.args, self.iso, self.sink, self.sampler, self.out = args, iso, sink, sampler, out
        self.env = iso.env()
        self.repo = iso.root / "repo"
        self.worktrees: list[Path] = []
        self.sessions: list[tuple[int, McpSession]] = []
        self.dedicated: dict[int, McpSession] = {}
        self._seq = 0

    # ---- sessions and calls ----

    def open_session(self, worktree: int, name: str) -> McpSession:
        self._seq += 1
        cwd = self.worktrees[worktree]
        sess = McpSession(self.args.binary, cwd, self.env, self.out / "serve-stderr.log", f"{name}-{self._seq}")
        sess.initialize(timeout=self.args.first_answer_timeout)
        self.sessions.append((worktree, sess))
        return sess

    def call(self, sess: McpSession, tool: str, arguments: dict[str, Any]) -> ToolResult | None:
        rec: dict[str, Any] = {"kind": "call", "session": sess.name, "tool": tool, "phase": self.sampler.phase}
        try:
            res = sess.call(tool, arguments, timeout=self.args.call_timeout)
        except McpError as exc:
            self.sink.write({**rec, "ok": False, "transport_error": str(exc)[:300]})
            return None
        rec.update(ok=not res.is_error, ms=round(res.secs * 1000, 1))
        if res.is_error:
            rec["error"] = res.text[:300]
        self.sink.write(rec)
        return res

    def poll(self, done: Callable[[], bool], timeout: float) -> float | None:
        """Seconds until `done()` holds, polling about four times a second, or None at `timeout`."""
        started = time.monotonic()
        while time.monotonic() - started < timeout:
            if done():
                return time.monotonic() - started
            time.sleep(0.25)
        return None

    def symbol_listed(self, sess: McpSession, name: str) -> bool:
        res = self.call(sess, "code_symbols", {"name": name, "limit": 5})
        return bool(res and not res.is_error and name in res.text)

    # ---- setup ----

    def scan_main(self) -> bool:
        started = time.monotonic()
        try:
            out = run_cli(self.args.binary, ["scan"], self.repo, self.env, timeout=self.args.scan_timeout)
            code: int | None = out.returncode
        except subprocess.TimeoutExpired:
            code = None
        data = dir_usage(self.iso.data_home, 0).apparent
        self.sink.write(
            {
                "kind": "scan",
                "ok": code == 0,
                "exit_code": code,
                "secs": round(time.monotonic() - started, 1),
                "data_bytes": data,
            }
        )
        return code == 0

    def add_worktrees(self, paths: tuple[str, ...]) -> bool:
        """Create and open the worktrees one after another so each one's first answer and the bytes it
        adds are its own."""
        self.worktrees = [self.repo]
        for i in range(self.args.worktrees):
            dest = self.iso.root / "wt" / f"wt-{i}"
            dest.parent.mkdir(exist_ok=True)
            files = add_worktree(self.repo, dest, paths, i)
            self.worktrees.append(dest)
            before = dir_usage(self.iso.data_home, 0).apparent
            started = time.monotonic()
            ok = files >= self.args.min_worktree_files
            if ok:
                sess = self.open_session(i + 1, "wt")
                self.dedicated[i] = sess
                ok = (
                    self.poll(lambda s=sess: self.symbol_listed(s, "soak_shared"), self.args.first_answer_timeout)
                    is not None
                )
            self.sink.write(
                {
                    "kind": "worktree",
                    "index": i,
                    "ok": ok,
                    "files": files,
                    "secs": round(time.monotonic() - started, 1),
                    "added_bytes": dir_usage(self.iso.data_home, 0).apparent - before,
                }
            )
            if not ok:
                return False
        return True

    # ---- load ----

    def load_loop(self, number: int, sess: McpSession, stop: threading.Event) -> None:
        rng = random.Random(self.args.seed + number)
        calls: list[tuple[str, dict[str, Any]]] = [
            ("index_status", {}),
            ("index_status", {}),
            ("find_files", {"query": "soak", "limit": 20}),
            ("code_symbols", {"name": "soak_shared", "limit": 20}),
            ("code_grep", {"pattern": "soak_shared", "limit": 20}),
            ("code_read", {"path": "soak_shared.py", "symbol": "soak_shared"}),
            ("code_outline", {"path": "soak_shared.py"}),
            ("code_refs", {"name": "soak_shared", "limit": 20}),
            ("git_log", {"limit": 5}),
        ]
        while not stop.is_set():
            tool, arguments = rng.choice(calls)
            self.call(sess, tool, arguments)
            stop.wait(rng.uniform(0.05, 0.5))

    # ---- scenarios ----

    def save_scenario(self) -> None:
        sess, wt = self.dedicated[0], self.worktrees[1]
        source = SHARED_PY
        for n in range(self.args.saves):
            source += f"\n\ndef soak_saved_{n}():\n    return {n}\n"
            since = time.time_ns()
            (wt / "soak_shared.py").write_text(source)
            started = time.monotonic()
            res = self.call(sess, "index_rescan", {"paths": ["soak_shared.py"]})
            ms = round((time.monotonic() - started) * 1000, 1)
            ok = bool(res and not res.is_error) and self.symbol_listed(sess, f"soak_saved_{n}")
            rewritten = rewritten_bytes(self.iso.data_home, since)
            (wt / "soak_watch.py").write_text(f"def soak_watched_{n}():\n    return {n}\n")
            stale = self.poll(lambda n=n: self.symbol_listed(sess, f"soak_watched_{n}"), self.args.stale_timeout)
            self.sink.write(
                {
                    "kind": "save",
                    "n": n,
                    "ok": ok,
                    "ms": ms,
                    "rewritten_bytes": rewritten,
                    "stale_visible": stale is not None,
                    "stale_s": round(self.args.stale_timeout if stale is None else stale, 2),
                }
            )
            time.sleep(self.args.save_gap)

    def switch_scenario(self) -> None:
        barrier = threading.Barrier(self.args.worktrees)

        def switch(i: int) -> None:
            wt, sess = self.worktrees[i + 1], self.dedicated[i]
            barrier.wait()
            started = time.monotonic()
            done = git(wt, "switch", "--quiet", f"soak-{i}-b", check=False)
            switched = time.monotonic() - started

            def answers_follow() -> bool:
                return self.symbol_listed(sess, f"soak_branch_b_{i}") and not self.symbol_listed(
                    sess, f"soak_branch_a_{i}"
                )

            seen = self.poll(answers_follow, self.args.stale_timeout * 2) if done.returncode == 0 else None
            self.sink.write(
                {
                    "kind": "switch",
                    "index": i,
                    "git_ok": done.returncode == 0,
                    "secs": round(switched, 2),
                    "visible": seen is not None,
                    "visible_s": round(self.args.stale_timeout * 2 if seen is None else seen, 2),
                }
            )

        threads = [threading.Thread(target=switch, args=(i,), name=f"switch-{i}") for i in range(self.args.worktrees)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()

    def retire_scenario(self) -> None:
        gone = range(self.args.worktrees - self.args.retire, self.args.worktrees)
        for i in gone:
            for wt_index, sess in [p for p in self.sessions if p[0] == i + 1]:
                sess.close(timeout=15)
                self.sessions.remove((wt_index, sess))
            git(self.repo, "worktree", "remove", "--force", str(self.worktrees[i + 1]))
        before = workspace_dirs(self.iso.data_home)
        started = time.monotonic()
        try:
            out = run_cli(self.args.binary, ["cache", "gc"], self.repo, self.env, timeout=self.args.gc_timeout)
            code: int | None = out.returncode
        except subprocess.TimeoutExpired:
            code = None
        self.sink.write(
            {
                "kind": "gc",
                "exit_code": code,
                "secs": round(time.monotonic() - started, 2),
                "removed": len(gone),
                "workspaces_before": before,
                "workspaces_after": workspace_dirs(self.iso.data_home),
            }
        )

    def shutdown(self) -> None:
        self.sampler.phase = "shutdown"
        for _wt, sess in self.sessions:
            sess.close(timeout=15)
        self.sessions.clear()
        run_cli(self.args.binary, ["comms", "stop"], self.repo, self.env, timeout=60)
        deadline = time.monotonic() + self.args.exit_grace * 3
        while time.monotonic() < deadline and any(
            role_of(a) == "daemon" for a in owned_processes(self.iso.root).values()
        ):
            time.sleep(0.25)
        time.sleep(2)
        survivors = owned_processes(self.iso.root)
        self.sink.write(
            {"kind": "survivors", "pids": sorted(survivors), "argv": {str(p): a for p, a in survivors.items()}}
        )


def build_parser() -> argparse.ArgumentParser:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--binary", type=Path, required=True, help="basemind binary under test")
    ap.add_argument("--name", required=True, help="a row of large_repos.tsv")
    ap.add_argument("--out", type=Path, help="output dir (default <reports root>/monorepo-<name>-<ts>)")
    ap.add_argument("--worktrees", type=int, default=20, help="linked worktrees to add (default 20)")
    ap.add_argument("--sessions", type=int, default=30, help="concurrent sessions under load (default 30)")
    ap.add_argument("--duration", type=float, default=900, help="seconds of load, scenarios included (default 900)")
    ap.add_argument("--saves", type=int, default=10, help="saves of the shared file (default 10)")
    ap.add_argument("--save-gap", type=float, default=2.0, help="seconds between saves (default 2)")
    ap.add_argument("--retire", type=int, default=5, help="worktrees removed before `cache gc` (default 5)")
    ap.add_argument("--scan-timeout", type=float, default=7200, help="seconds the main checkout's scan may take")
    ap.add_argument("--first-answer-timeout", type=float, default=1800, help="seconds a worktree may take to answer")
    ap.add_argument("--stale-timeout", type=float, default=60, help="seconds to wait for an edit to be answerable")
    ap.add_argument("--gc-timeout", type=float, default=600)
    ap.add_argument("--call-timeout", type=float, default=300)
    ap.add_argument("--exit-grace", type=float, default=10)
    ap.add_argument("--min-worktree-files", type=int, default=50, help="files a worktree must hold (default 50)")
    ap.add_argument("--sample-every", type=float, default=5)
    ap.add_argument("--seed", type=int, default=7)
    ap.add_argument("--keep-temp", action="store_true", help="keep the temp data home, clone and worktrees")
    return ap


def validate(args: argparse.Namespace) -> str | None:
    """Why the run cannot be set up as asked, or None."""
    if IS_WINDOWS:
        return "the large-monorepo soak runs on Linux and macOS only"
    if args.sessions < args.worktrees:
        return "--sessions must be at least --worktrees: each worktree gets a session of its own"
    if not 0 < args.retire < args.worktrees:
        return "--retire must be between 1 and --worktrees - 1"
    if not args.binary.is_file():
        return f"no binary at {args.binary}"
    return None


def main() -> int:
    args = build_parser().parse_args()
    args.binary = args.binary.resolve()
    if (problem := validate(args)) is not None:
        sys.stderr.write(f"{problem}\n")
        return 2
    rows = {r.name: r for r in parse_corpus(CORPUS.read_text(encoding="utf-8"))}
    if args.name not in rows:
        sys.stderr.write(f"{args.name} is not in {CORPUS.name}: {', '.join(rows)}\n")
        return 2
    row = rows[args.name]

    out = args.out or default_out_root() / time.strftime(f"monorepo-{args.name}-%Y%m%d-%H%M%S")
    out.mkdir(parents=True, exist_ok=True)
    iso = Isolation("monorepo")
    t0 = time.monotonic()
    sink = Ndjson(out / "monorepo.ndjson", t0)
    sampler = Sampler(iso, sink, args.sample_every)
    watcher = MsgpackWatcher(iso.data_home)
    rig = Rig(args, iso, sink, sampler, out)
    stop = threading.Event()

    def cleanup() -> None:
        stop.set()
        sampler.stop_event.set()
        watcher.stop_event.set()
        for _wt, sess in rig.sessions:
            sess.kill()
        killed = terminate_owned(iso.root)
        if killed:
            log(f"cleanup signalled {len(killed)} leftover process(es): {killed}")
        with contextlib.suppress(ValueError):
            sink.write({"kind": "cleanup", "signalled": killed})
        sink.close()
        if not args.keep_temp:
            iso.cleanup()

    install_cleanup(cleanup)

    log(f"fetching {row.name} at {row.commit}")
    fetched = subprocess.run(["bash", str(HERE / "fetch_large_repo.sh"), row.name, str(rig.repo)], check=False)
    if fetched.returncode != 0:
        sys.stderr.write("fetching the pinned repository failed\n")
        return 1
    files = len(git(rig.repo, "ls-files").stdout.splitlines())
    sink.write(
        {
            "kind": "meta",
            "repo": row.name,
            "commit": row.commit,
            "files": files,
            "worktrees": args.worktrees,
            "sessions": args.sessions,
            "duration": args.duration,
            "binary": str(args.binary),
            "version": binary_version(args.binary, iso),
            "data_home": str(iso.data_home),
            **host_info(),
        }
    )

    sampler.start()
    watcher.start()
    log(f"scanning {files} files")
    if not rig.scan_main():
        log("the first scan failed: nothing to soak")
        return 1
    sampler.phase = "setup"
    log(f"adding {args.worktrees} worktrees")
    if not rig.add_worktrees(row.worktree_paths):
        log("a worktree did not come up")
        return 1
    extra = [rig.open_session(1 + i % args.worktrees, "extra") for i in range(args.sessions - args.worktrees)]
    log(f"{len(rig.sessions)} sessions open; load for {args.duration:.0f} s")

    sampler.phase = "load"
    sink.write({"kind": "phase", "name": "load_start"})
    loops = [
        threading.Thread(target=rig.load_loop, args=(n, sess, stop), name=f"load-{n}", daemon=True)
        for n, sess in enumerate([*rig.dedicated.values(), *extra])
    ]
    for t in loops:
        t.start()
    load_start = time.monotonic()
    rig.save_scenario()
    rig.switch_scenario()
    stop.wait(max(0.0, args.duration - (time.monotonic() - load_start)))
    stop.set()
    for t in loops:
        t.join(timeout=args.call_timeout)
    sink.write({"kind": "phase", "name": "load_end"})

    sampler.phase = "retire"
    rig.retire_scenario()
    watcher.stop_event.set()
    sink.write(watcher.total())
    rig.shutdown()
    sampler.stop_event.set()
    sampler.join(timeout=args.sample_every * 2)
    sink.write({"kind": "end"})
    log(f"done: {out / 'monorepo.ndjson'}")
    print(json.dumps({"ndjson": str(out / "monorepo.ndjson")}))
    return 0


if __name__ == "__main__":
    sys.exit(main())
