#!/usr/bin/env python3
"""Soak a release basemind build: a scripted MCP + CLI workload, sampled into NDJSON.

Phases: `load` (the workload; its first `--warmup` seconds are tagged `warmup`), `idle` (sessions
stay connected, no calls), `shutdown` (clean close, host SIGKILL, serve SIGKILL, `comms stop`, then
a sweep for any surviving process of the run). `analyze.py` turns the NDJSON into a verdict.

Usage:
    soak.py --binary target/release/basemind [--repo PATH] [--duration 600] [--idle 180]
"""

from __future__ import annotations

import argparse
import contextlib
import json
import os
import random
import signal
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

from common import (  # noqa: E402 - path set up above
    Isolation,
    McpError,
    McpSession,
    Ndjson,
    ToolResult,
    binary_version,
    default_out_root,
    dir_bytes,
    host_info,
    install_cleanup,
    log,
    owned_processes,
    pid_alive,
    repo_root,
    role_of,
    run_cli,
    sample_pid,
    terminate_owned,
)

HOST_SCRIPT = r"""
import json, subprocess, sys, time
binary, repo = sys.argv[1], sys.argv[2]
p = subprocess.Popen([binary, "serve"], cwd=repo, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                     stderr=subprocess.DEVNULL)
def rpc(i, method, params):
    p.stdin.write((json.dumps({"jsonrpc": "2.0", "id": i, "method": method, "params": params}) + "\n").encode())
    p.stdin.flush()
    while True:
        msg = json.loads(p.stdout.readline())
        if msg.get("id") == i and "method" not in msg:
            return msg
rpc(1, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                      "clientInfo": {"name": "hardening-host", "version": "1"}})
p.stdin.write(b'{"jsonrpc":"2.0","method":"notifications/initialized"}\n'); p.stdin.flush()
rpc(2, "tools/call", {"name": "index_status", "arguments": {}})
print(p.pid, flush=True)
time.sleep(3600)
"""

FEATURE_TOOLS = ("code_search", "docs_search", "memory_save", "memory_search")


class Sampler(threading.Thread):
    """Samples every process carrying the run's marker, plus the data dir, every `every` seconds."""

    def __init__(self, iso: Isolation, sink: Ndjson, every: float) -> None:
        super().__init__(name="sampler", daemon=True)
        self.iso, self.sink, self.every = iso, sink, every
        self.phase = "setup"
        self.stop_event = threading.Event()
        self._last_cpu: dict[int, float] = {}
        self._last_wake: dict[int, int] = {}
        self.daemon_pids: set[int] = set()

    def run(self) -> None:
        while not self.stop_event.is_set():
            self.sample_once()
            self.stop_event.wait(self.every)

    def sample_once(self) -> dict[int, str]:
        procs = owned_processes(self.iso.root)
        census: dict[str, int] = {}
        roles: dict[int, str] = {}
        for pid, argv in procs.items():
            s = sample_pid(pid)
            if s is None:
                continue
            role = role_of(argv)
            roles[pid] = role
            key = role.split(":", 1)[0]
            census[key] = census.get(key, 0) + 1
            if role == "daemon":
                self.daemon_pids.add(pid)
            prev = self._last_cpu.get(pid)
            prev_wake = self._last_wake.get(pid)
            self._last_cpu[pid] = s.cpu_s
            if s.wakeups is not None:
                self._last_wake[pid] = s.wakeups
            self.sink.write(
                {
                    "kind": "proc",
                    "phase": self.phase,
                    "pid": pid,
                    "role": role,
                    "rss": s.rss,
                    "footprint": s.footprint,
                    "vsz": s.vsz,
                    "threads": s.threads,
                    "fds": s.fds,
                    "cpu_s": round(s.cpu_s, 4),
                    "cpu_delta_s": None if prev is None else round(s.cpu_s - prev, 4),
                    "wakeups_delta": None if prev_wake is None or s.wakeups is None else s.wakeups - prev_wake,
                    "interval_s": self.every,
                }
            )
        data_app, data_alloc = dir_bytes(self.iso.data_home)
        comms_app, _ = dir_bytes(self.iso.comms_dir)
        self.sink.write(
            {
                "kind": "dir",
                "phase": self.phase,
                "data_bytes": data_app,
                "data_alloc_bytes": data_alloc,
                "comms_bytes": comms_app,
            }
        )
        self.sink.write({"kind": "census", "phase": self.phase, "counts": census, "total": sum(census.values())})
        return roles


class Workload:
    """The scripted load: tool sweep, edits + rescans, session churn, CLI one-shots."""

    def __init__(self, args: argparse.Namespace, iso: Isolation, repo: Path, sink: Ndjson, out: Path) -> None:
        self.args, self.iso, self.repo, self.sink, self.out = args, iso, repo, sink, out
        self.binary: Path = args.binary
        self.env = iso.env()
        self.rng = random.Random(args.seed)  # noqa: S311 - workload shuffling, not crypto
        self.main: McpSession | None = None
        self.skipped: set[str] = set()
        self.targets: dict[str, Any] = {}
        self.session_seq = 0
        self.soak_dir = repo / "hardening_soak"

    # ---- sessions ----

    def open_session(self, name: str) -> McpSession:
        self.session_seq += 1
        sess = McpSession(self.binary, self.repo, self.env, self.out / "serve-stderr.log", f"{name}-{self.session_seq}")
        sess.initialize(timeout=180)
        self.sink.write({"kind": "session", "event": "open", "name": sess.name, "pid": sess.pid})
        return sess

    def close_session(self, sess: McpSession, how: str) -> None:
        if how == "kill":
            sess.kill()
            code: int | None = sess.proc.returncode
        else:
            code = sess.close(timeout=15)
        self.sink.write(
            {
                "kind": "session",
                "event": "close",
                "how": how,
                "name": sess.name,
                "pid": sess.pid,
                "exit_code": code,
                "exited": code is not None,
            }
        )

    # ---- calls ----

    def call(self, sess: McpSession, tool: str, args: dict[str, Any], expect_ok: bool = True) -> ToolResult | None:
        if tool in self.skipped:
            return None
        try:
            res = sess.call(tool, args, timeout=self.args.call_timeout)
        except McpError as exc:
            self.sink.write(
                {"kind": "call", "session": sess.name, "tool": tool, "ok": False, "transport_error": str(exc)[:300]}
            )
            return None
        if res.feature_missing():
            if tool not in self.skipped:
                self.skipped.add(tool)
                self.sink.write({"kind": "skip", "tool": tool, "reason": res.text[:200]})
            return None
        ok = not res.is_error
        rec: dict[str, Any] = {
            "kind": "call",
            "session": sess.name,
            "tool": tool,
            "ok": ok,
            "expect_ok": expect_ok,
            "ms": round(res.secs * 1000, 1),
        }
        if not ok:
            rec["error"] = res.text[:300]
            rec["args"] = args
        self.sink.write(rec)
        return res

    def discover_targets(self, sess: McpSession) -> None:
        """Pick a code file, a function in it, a trait and an imported module from the index."""
        res = self.call(sess, "find_files", {"limit": 400})
        files = [f.get("path") for f in (res.body or {}).get("files", [])] if res and res.body else []
        code_exts = (".rs", ".py", ".ts", ".go", ".js")
        candidates = [f for f in files if f and f.endswith(code_exts) and "hardening_soak" not in f]
        self.rng.shuffle(candidates)
        for path in candidates[:60]:
            out = self.call(sess, "code_outline", {"path": path})
            body = out.body if out else None
            if not body:
                continue
            fns = [s for s in body.get("symbols", []) if s.get("kind") in ("function", "method") and s.get("name")]
            if len(fns) >= 2:  # noqa: PLR2004
                self.targets["path"] = path
                self.targets["symbol"] = fns[0]["name"]
                imports = body.get("imports") or []
                if imports:
                    first = imports[0]
                    module = first.get("module") if isinstance(first, dict) else str(first)
                    if module:
                        self.targets["module"] = module.split("::")[-1].split(".")[-1].strip("{} ")
                break
        for kind in ("trait", "interface", "class"):
            out = self.call(sess, "code_symbols", {"name": "e", "kind": kind, "limit": 5})
            hits = (out.body or {}).get("symbols") or (out.body or {}).get("hits") if out and out.body else None
            if hits:
                self.targets["trait"] = hits[0].get("name")
                break
        self.sink.write({"kind": "targets", **self.targets})
        if "path" not in self.targets:
            msg = "no code file with two functions found in the index: the workload would check nothing"
            raise SystemExit(msg)

    def sweep(self, sess: McpSession, cycle: int) -> None:
        t = self.targets
        path, sym = t["path"], t["symbol"]
        calls: list[tuple[str, dict[str, Any]]] = [
            ("index_status", {}),
            ("find_files", {"limit": 50}),
            ("find_files", {"query": Path(path).stem[:6], "limit": 20}),
            ("code_symbols", {"name": sym, "limit": 50}),
            ("code_symbols", {"name": sym[:4], "fuzzy": True, "limit": 20}),
            ("code_outline", {"path": path}),
            ("code_read", {"path": path, "symbol": sym}),
            ("code_read", {"path": path, "line": 1, "end_line": 40}),
            ("code_grep", {"pattern": sym, "limit": 50}),
            ("code_grep", {"pattern": r"^\s*(fn|def|func) ", "regex": True, "limit": 20, "context": True}),
            ("code_refs", {"name": sym, "limit": 50}),
            ("code_refs", {"name": sym, "path": path, "limit": 20}),
            ("git_log", {"limit": 20}),
            ("git_log", {"path": path, "limit": 10}),
            ("git_log", {"query": "fix", "limit": 10}),
            ("git_log", {"path": path, "symbol": sym, "limit": 3}),
            ("git_log", {"rev": "HEAD", "max_tokens": 2000}),
            ("git_blame", {"path": path, "line": 1, "end_line": 30}),
            ("git_blame", {"path": path, "symbol": sym}),
            ("git_diff", {}),
            ("git_diff", {"path": path, "structural": True, "from": "HEAD"}),
            ("git_diff", {"path": "hardening_soak/edited.py"}),
            ("code_search", {"query": "parse a configuration file and report errors", "limit": 5}),
            ("docs_search", {"query": "how is the project installed", "limit": 5}),
            ("memory_save", {"key": f"soak/note-{cycle % 5}", "value": f"soak note {cycle % 5}", "tags": ["soak"]}),
            ("memory_search", {"query": "soak note", "limit": 5}),
            ("memory_search", {"key": f"soak/note-{cycle % 5}"}),
        ]
        if "trait" in t:
            calls.append(("code_refs", {"name": t["trait"], "relation": "implementations", "limit": 20}))
        if "module" in t:
            calls.append(("code_refs", {"name": t["module"], "relation": "importers", "limit": 20}))
        for tool, args in calls:
            self.call(sess, tool, args)

    # ---- edits ----

    def edit_cycle(self, sess: McpSession, cycle: int) -> None:
        """Modify, add and delete files with content that repeats every `content_period` cycles."""
        v = cycle % self.args.content_period
        self.soak_dir.mkdir(exist_ok=True)
        edited = self.soak_dir / "edited.py"
        edited.write_text(_py_module(f"soak_edit_v{v}", v))
        slot_add = cycle % 3
        slot_del = (cycle + 1) % 3
        added = self.soak_dir / f"added_{slot_add}.py"
        added.write_text(_py_module(f"soak_added_{slot_add}_v{v}", v + 100))
        deleted = self.soak_dir / f"added_{slot_del}.py"
        with contextlib.suppress(FileNotFoundError):
            deleted.unlink()
        rel = [str(p.relative_to(self.repo)) for p in (edited, added, deleted)]
        self.call(sess, "index_rescan", {"paths": rel})
        res = self.call(sess, "code_symbols", {"name": f"soak_edit_v{v}", "limit": 5})
        found = bool(
            res and not res.is_error and f"soak_edit_v{v}" in res.text and "hardening_soak/edited.py" in res.text
        )
        gone = self.call(sess, "code_symbols", {"name": f"soak_added_{slot_del}_", "limit": 5})
        stale = bool(gone and f"added_{slot_del}.py" in gone.text)
        self.sink.write({"kind": "edit_check", "cycle": cycle, "edit_visible": found, "deleted_gone": not stale})

    # ---- churn ----

    def churn_sessions(self, cycle: int) -> None:
        for i in range(self.args.sessions_per_cycle):
            try:
                sess = self.open_session("short")
            except McpError as exc:
                self.sink.write(
                    {
                        "kind": "call",
                        "session": "short",
                        "tool": "initialize",
                        "ok": False,
                        "transport_error": str(exc)[:300],
                    }
                )
                continue
            self.call(sess, "index_status", {})
            self.call(sess, "code_symbols", {"name": self.targets["symbol"], "limit": 5})
            self.close_session(sess, "kill" if (cycle + i) % 2 else "eof")

    def cli_oneshot(self, cycle: int) -> None:
        commands = [
            ["status"],
            ["code", "symbols", self.targets["symbol"]],
            ["git", "log", "--limit", "5"],
            ["code", "grep", self.targets["symbol"]],
        ]
        if cycle % self.args.full_scan_every == 0:
            # `rescan`, not `scan`: on main a CLI `scan` refuses (exit 3) while the daemon runs.
            commands.append(["rescan"])
        for argv in commands:
            started = time.monotonic()
            try:
                out = run_cli(self.binary, argv, self.repo, self.env, timeout=self.args.call_timeout)
                code: int | None = out.returncode
                err = out.stderr[:300] + (" ... " + out.stderr[-200:] if len(out.stderr) > 500 else "")
            except subprocess.TimeoutExpired:
                code, err = None, "timeout"
            self.sink.write(
                {
                    "kind": "cli",
                    "argv": argv,
                    "exit_code": code,
                    "ok": code == 0,
                    "ms": round((time.monotonic() - started) * 1000, 1),
                    **({"stderr": err} if code != 0 else {}),
                }
            )


def _py_module(name: str, value: int) -> str:
    body = "\n".join(f"    total += {value} * {i}" for i in range(20))
    return f'"""Generated by the hardening soak."""\n\n\ndef {name}(x):\n    total = x\n{body}\n    return total\n'


def clone_repo(src: Path, dest: Path) -> None:
    subprocess.run(["git", "clone", "--quiet", "--local", "--no-hardlinks", str(src), str(dest)], check=True)  # noqa: S607


def shutdown_checks(wl: Workload, sampler: Sampler, sink: Ndjson, iso: Isolation, args: argparse.Namespace) -> None:
    """Every way a session ends must leave no process of it behind."""
    sampler.phase = "shutdown"
    if wl.main is not None:
        started = time.monotonic()
        wl.close_session(wl.main, "eof")
        sink.write(
            {
                "kind": "shutdown",
                "step": "main_stdin_eof",
                "exited": not pid_alive(wl.main.pid),
                "secs": round(time.monotonic() - started, 2),
            }
        )
        wl.main = None

    # The MCP host dies with SIGKILL: serve keeps no writer on its stdin and must exit on EOF.
    host = subprocess.Popen(  # noqa: S603
        [sys.executable, "-c", HOST_SCRIPT, str(args.binary), str(wl.repo)],
        env=wl.env,
        stdout=subprocess.PIPE,
        text=True,
    )
    serve_pid = None
    try:
        assert host.stdout is not None
        line = host.stdout.readline().strip()
        serve_pid = int(line) if line.isdigit() else None
    finally:
        host.kill()
        host.wait()
    exited = False
    started = time.monotonic()
    if serve_pid:
        while time.monotonic() - started < args.exit_grace:
            if not pid_alive(serve_pid):
                exited = True
                break
            time.sleep(0.2)
    sink.write(
        {
            "kind": "shutdown",
            "step": "host_sigkill",
            "serve_pid": serve_pid,
            "exited": exited,
            "secs": round(time.monotonic() - started, 2),
        }
    )

    # serve itself dies with SIGKILL: the daemon must drop the connection and its descriptors.
    daemon_pid = next(iter(p for p, a in owned_processes(iso.root).items() if role_of(a) == "daemon"), None)
    before = sample_pid(daemon_pid).fds if daemon_pid else None
    sess = wl.open_session("killed")
    wl.call(sess, "index_status", {})
    during = sample_pid(daemon_pid).fds if daemon_pid else None
    wl.close_session(sess, "kill")
    time.sleep(args.exit_grace)
    after_s = sample_pid(daemon_pid) if daemon_pid else None
    after = after_s.fds if after_s else None
    sink.write(
        {
            "kind": "shutdown",
            "step": "serve_sigkill",
            "exited": not pid_alive(sess.pid),
            "daemon_pid": daemon_pid,
            "daemon_fds_before": before,
            "daemon_fds_during": during,
            "daemon_fds_after": after,
        }
    )

    started = time.monotonic()
    out = run_cli(args.binary, ["comms", "stop"], wl.repo, wl.env, timeout=60)
    daemon_gone = False
    while time.monotonic() - started < args.exit_grace * 3:
        if not any(role_of(a) == "daemon" for a in owned_processes(iso.root).values()):
            daemon_gone = True
            break
        time.sleep(0.25)
    sink.write(
        {
            "kind": "shutdown",
            "step": "comms_stop",
            "exit_code": out.returncode,
            "exited": daemon_gone,
            "secs": round(time.monotonic() - started, 2),
            "stdout": out.stdout[-300:],
            "stderr": out.stderr[-300:],
        }
    )

    if args.inject_orphan:
        # Proof that the sweep fires: a detached process anchored in the run root, as a leaked child would be.
        subprocess.Popen(["sleep", "600"], cwd=iso.root, env=wl.env, start_new_session=True)  # noqa: S603,S607
        sink.write(
            {"kind": "note", "text": "injected a detached `sleep 600` whose cwd is the run root (--inject-orphan)"}
        )

    time.sleep(2)
    survivors = owned_processes(iso.root)
    sink.write(
        {
            "kind": "survivors",
            "pids": sorted(survivors),
            "argv": {str(p): a for p, a in survivors.items()},
        }
    )


def main() -> int:  # noqa: PLR0915
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--binary", type=Path, required=True, help="basemind binary under test")
    ap.add_argument(
        "--repo", type=Path, help="repository to copy and soak (default: a local clone of this checkout's HEAD)"
    )
    ap.add_argument("--duration", type=float, default=600, help="load phase seconds, warm-up included (default 600)")
    ap.add_argument(
        "--warmup", type=float, default=120, help="seconds at the start of load excluded from slopes (default 120)"
    )
    ap.add_argument("--idle", type=float, default=180, help="idle phase seconds (default 180)")
    ap.add_argument("--sample-every", type=float, default=5, help="sampling period seconds (default 5)")
    ap.add_argument("--sessions-per-cycle", type=int, default=2, help="short sessions opened and closed per cycle")
    ap.add_argument("--content-period", type=int, default=6, help="edit contents repeat every N cycles (default 6)")
    ap.add_argument("--full-scan-every", type=int, default=5, help="whole-workspace rescan + CLI scan every N cycles")
    ap.add_argument("--call-timeout", type=float, default=300)
    ap.add_argument("--exit-grace", type=float, default=10, help="seconds a process gets to exit after its cue")
    ap.add_argument("--seed", type=int, default=7)
    ap.add_argument("--out", type=Path, help="output dir (default <polyrepo>/reports/hardening/soak-<ts>)")
    ap.add_argument(
        "--inject-orphan", action="store_true", help="leak a detached process of the run (proves the sweep fails)"
    )
    ap.add_argument("--keep-temp", action="store_true", help="keep the temp data home / repo copy")
    args = ap.parse_args()
    args.binary = args.binary.resolve()
    if not args.binary.is_file():
        ap.error(f"no binary at {args.binary}")

    out = args.out or default_out_root() / time.strftime("soak-%Y%m%d-%H%M%S")
    out.mkdir(parents=True, exist_ok=True)
    iso = Isolation("soak")
    t0 = time.monotonic()
    sink = Ndjson(out / "soak.ndjson", t0)
    sampler = Sampler(iso, sink, args.sample_every)
    wl_holder: list[Workload] = []

    def cleanup() -> None:
        sampler.stop_event.set()
        for wl in wl_holder:
            if wl.main is not None:
                wl.main.kill()
        killed = terminate_owned(iso.root)
        if killed:
            log(f"cleanup signalled {len(killed)} leftover process(es): {killed}")
        with contextlib.suppress(ValueError):
            sink.write({"kind": "cleanup", "signalled": killed})
        sink.close()
        if not args.keep_temp:
            iso.cleanup()

    install_cleanup(cleanup)

    repo = iso.root / "repo"
    src = (args.repo or repo_root()).resolve()
    log(f"copying {src} -> {repo}")
    if (src / ".git").exists():
        clone_repo(src, repo)
    else:
        subprocess.run(["cp", "-R", str(src), str(repo)], check=True)  # noqa: S607
    sink.write(
        {
            "kind": "meta",
            "binary": str(args.binary),
            "version": binary_version(args.binary, iso),
            "repo_source": str(src),
            "data_home": str(iso.data_home),
            "comms_dir": str(iso.comms_dir),
            "session": iso.session,
            "duration": args.duration,
            "warmup": args.warmup,
            "idle": args.idle,
            "sample_every": args.sample_every,
            "content_period": args.content_period,
            "pid": os.getpid(),
            **host_info(),
        }
    )
    wl = Workload(args, iso, repo, sink, out)
    wl_holder.append(wl)

    log("initial CLI scan (no daemon yet: a direct scan)")
    started = time.monotonic()
    scan = run_cli(args.binary, ["scan"], repo, wl.env, timeout=1800)
    sink.write(
        {
            "kind": "cli",
            "argv": ["scan"],
            "exit_code": scan.returncode,
            "ok": scan.returncode == 0,
            "ms": round((time.monotonic() - started) * 1000),
            "initial": True,
            **({"stderr": scan.stderr[-500:]} if scan.returncode else {}),
        }
    )
    if scan.returncode != 0:
        log(f"initial scan failed: {scan.stderr[-500:]}")

    sampler.start()
    sampler.phase = "warmup"
    wl.main = wl.open_session("main")
    wl.discover_targets(wl.main)
    log(f"targets: {wl.targets}")

    load_start = time.monotonic()
    cycle = 0
    while time.monotonic() - load_start < args.duration:
        sampler.phase = "warmup" if time.monotonic() - load_start < args.warmup else "load"
        sink.write({"kind": "cycle", "cycle": cycle, "phase": sampler.phase})
        wl.sweep(wl.main, cycle)
        wl.edit_cycle(wl.main, cycle)
        if cycle % args.full_scan_every == 0:
            wl.call(wl.main, "index_rescan", {})
        wl.churn_sessions(cycle)
        wl.cli_oneshot(cycle)
        cycle += 1
        if cycle % 5 == 0:
            log(f"cycle {cycle}, {int(time.monotonic() - load_start)}s of {int(args.duration)}s")
    sink.write({"kind": "phase", "name": "load_end", "cycles": cycle})

    log(f"idle phase: {args.idle}s with the main session connected and no calls")
    sampler.phase = "idle"
    sink.write({"kind": "phase", "name": "idle_start"})
    time.sleep(args.idle)
    sink.write({"kind": "phase", "name": "idle_end"})

    log("shutdown checks")
    shutdown_checks(wl, sampler, sink, iso, args)
    sampler.stop_event.set()
    sampler.join(timeout=args.sample_every * 2)
    sink.write({"kind": "end", "skipped_tools": sorted(wl.skipped)})
    log(f"done: {out / 'soak.ndjson'}")
    print(json.dumps({"ndjson": str(out / "soak.ndjson")}))
    return 0


if __name__ == "__main__":
    signal.signal(signal.SIGPIPE, signal.SIG_DFL)
    sys.exit(main())
