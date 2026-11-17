"""Shared plumbing for the hardening scripts: isolation, process ownership, sampling, MCP stdio.

Standard library only. Every basemind process these scripts start runs with a fresh
`BASEMIND_DATA_HOME` and `BASEMIND_COMMS_DIR` inside one `mkdtemp` root per run. Ownership is
decided by that root: a process belongs to the run when its working directory or one of its open
files lies inside the root, or when it descends from such a process. That finds the daemon `serve`
auto-spawns detached (re-parented to init; its cwd is the run's comms dir and it holds the run's
`daemon.lock`), every `serve` and CLI one-shot (cwd: the run's repo copy) and their children. The
test is repeated immediately before any signal, so a process this run did not start, such as
another session's daemon, is never signalled. (macOS no longer exposes another process's
environment, so an environment marker cannot be used for this; `BASEMIND_HARDENING_SESSION` is
still set, for a human reading `ps` output on Linux.)
"""

from __future__ import annotations

import contextlib
import ctypes
import ctypes.util
import json
import os
import platform
import queue
import secrets
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import IO, Any

SESSION_ENV = "BASEMIND_HARDENING_SESSION"
IS_MAC = sys.platform == "darwin"
IS_LINUX = sys.platform.startswith("linux")
MIB = 1024 * 1024
MAX_SOCKET_PATH = 100


def log(msg: str) -> None:
    """Progress line on stderr, so stdout stays machine-readable."""
    sys.stderr.write(f"[{time.strftime('%H:%M:%S')}] {msg}\n")
    sys.stderr.flush()


def default_out_root() -> Path:
    """`<polyrepo root>/reports/hardening` when the script lives in the polyrepo, else a temp dir."""
    here = Path(__file__).resolve()
    for parent in here.parents:
        if (parent / "repos.txt").is_file() and (parent / "mise-tasks").is_dir():
            return parent / "reports" / "hardening"
    return Path(tempfile.gettempdir()) / "basemind-hardening"


def repo_root() -> Path:
    """The basemind checkout this script belongs to."""
    return Path(__file__).resolve().parents[2]


def forbidden_data_homes() -> list[Path]:
    """The real per-user basemind data homes; a run must never resolve to one of them."""
    home = Path.home()
    return [
        home / ".local" / "share" / "basemind",
        home / "Library" / "Application Support" / "basemind",
    ]


@dataclass
class Isolation:
    """A throwaway data home, comms dir and marker for one run."""

    label: str
    root: Path = field(init=False)
    data_home: Path = field(init=False)
    comms_dir: Path = field(init=False)
    session: str = field(init=False)

    def __post_init__(self) -> None:
        base = tempfile.gettempdir()
        probe = Path(base).resolve() / "bmh-xxxxxxxx" / "comms" / "g1" / "comms.sock"
        if len(str(probe)) > MAX_SOCKET_PATH:
            base = "/tmp"  # noqa: S108 - a Unix socket path must stay under ~104 bytes
        self.root = Path(tempfile.mkdtemp(prefix="bmh-", dir=base)).resolve()
        self.data_home = self.root / "data"
        self.comms_dir = self.root / "comms"
        self.data_home.mkdir()
        self.comms_dir.mkdir()
        self.session = f"{self.label}-{os.getpid()}-{secrets.token_hex(6)}"

    def env(self, extra: dict[str, str] | None = None) -> dict[str, str]:
        """Environment for a basemind child: inherited `BASEMIND_*` dropped, isolation set."""
        env = {k: v for k, v in os.environ.items() if not k.startswith("BASEMIND_")}
        env["BASEMIND_DATA_HOME"] = str(self.data_home)
        env["BASEMIND_COMMS_DIR"] = str(self.comms_dir)
        env[SESSION_ENV] = self.session
        if extra:
            env.update(extra)
        check_env(env, self)
        return env

    def cleanup(self) -> None:
        shutil.rmtree(self.root, ignore_errors=True)


def check_env(env: dict[str, str], iso: Isolation) -> None:
    """Refuse to start a basemind process whose environment is not isolated to this run."""
    data, comms = env.get("BASEMIND_DATA_HOME"), env.get("BASEMIND_COMMS_DIR")
    if not data or not comms:
        msg = "refusing to run basemind without BASEMIND_DATA_HOME and BASEMIND_COMMS_DIR"
        raise SystemExit(msg)
    if "BASEMIND_NO_AUTOSPAWN" in env:
        raise SystemExit("refusing to run basemind with BASEMIND_NO_AUTOSPAWN set")
    for value in (data, comms):
        resolved = Path(value).resolve()
        if iso.root not in resolved.parents:
            raise SystemExit(f"refusing: {resolved} is not inside this run's temp root {iso.root}")
        for forbidden in forbidden_data_homes():
            if resolved == forbidden or forbidden in resolved.parents:
                raise SystemExit(f"refusing: {resolved} is the real basemind data home")


# ---------------------------------------------------------------------------------------------------
# Process discovery: argv, cwd and open files of same-user processes (macOS libproc, Linux /proc)
# ---------------------------------------------------------------------------------------------------

_libc = ctypes.CDLL(ctypes.util.find_library("c"), use_errno=True) if (IS_MAC or IS_LINUX) else None
_libproc = ctypes.CDLL("/usr/lib/libproc.dylib") if IS_MAC else None

# <sys/proc_info.h>: proc_pidinfo flavors and struct offsets.
_PROC_PIDLISTFDS = 1
_PROC_PIDVNODEPATHINFO = 9
_PROC_PIDFDVNODEPATHINFO = 2
_PROX_FDTYPE_VNODE = 1
_VNODE_INFO_SIZE = 152
_MAXPATHLEN = 1024
_VNODE_INFO_PATH_SIZE = _VNODE_INFO_SIZE + _MAXPATHLEN
_PROC_FILEINFO_SIZE = 24
_FDINFO_SIZE = 8


def _cstr(raw: bytes) -> str:
    return raw.split(b"\0", 1)[0].decode("utf-8", "replace")


def _argv_mac(pid: int) -> list[str] | None:
    ctl_kern, kern_argmax, kern_procargs2 = 1, 8, 49
    argmax = ctypes.c_int(0)
    size = ctypes.c_size_t(ctypes.sizeof(argmax))
    mib = (ctypes.c_int * 2)(ctl_kern, kern_argmax)
    if _libc.sysctl(mib, 2, ctypes.byref(argmax), ctypes.byref(size), None, 0) != 0:
        return None
    buf = ctypes.create_string_buffer(argmax.value)
    size = ctypes.c_size_t(argmax.value)
    mib3 = (ctypes.c_int * 3)(ctl_kern, kern_procargs2, pid)
    if _libc.sysctl(mib3, 3, buf, ctypes.byref(size), None, 0) != 0:
        return None
    raw = buf.raw[: size.value]
    if len(raw) < 4:  # noqa: PLR2004 - the leading argc int
        return None
    argc = int.from_bytes(raw[:4], sys.byteorder)
    rest = raw[4:]
    pos = rest.find(b"\0")
    while 0 <= pos < len(rest) and rest[pos] == 0:
        pos += 1
    return [p.decode("utf-8", "replace") for p in rest[pos:].split(b"\0")[:argc]]


def _cwd_mac(pid: int) -> str | None:
    buf = ctypes.create_string_buffer(2 * _VNODE_INFO_PATH_SIZE)
    if _libproc.proc_pidinfo(pid, _PROC_PIDVNODEPATHINFO, 0, buf, len(buf)) <= 0:
        return None
    return _cstr(buf.raw[_VNODE_INFO_SIZE:_VNODE_INFO_PATH_SIZE]) or None


def _open_paths_mac(pid: int) -> list[str]:
    need = _libproc.proc_pidinfo(pid, _PROC_PIDLISTFDS, 0, None, 0)
    if need <= 0:
        return []
    buf = ctypes.create_string_buffer(need)
    got = _libproc.proc_pidinfo(pid, _PROC_PIDLISTFDS, 0, buf, need)
    paths = []
    info = ctypes.create_string_buffer(_PROC_FILEINFO_SIZE + _VNODE_INFO_PATH_SIZE)
    for off in range(0, max(got, 0), _FDINFO_SIZE):
        fd = int.from_bytes(buf.raw[off : off + 4], sys.byteorder, signed=True)
        kind = int.from_bytes(buf.raw[off + 4 : off + 8], sys.byteorder)
        if kind != _PROX_FDTYPE_VNODE:
            continue
        if _libproc.proc_pidfdinfo(pid, fd, _PROC_PIDFDVNODEPATHINFO, info, len(info)) > 0:
            paths.append(_cstr(info.raw[_PROC_FILEINFO_SIZE + _VNODE_INFO_SIZE :]))
    return paths


def _argv_linux(pid: int) -> list[str] | None:
    try:
        raw = Path(f"/proc/{pid}/cmdline").read_bytes()
    except OSError:
        return None
    return [a.decode("utf-8", "replace") for a in raw.split(b"\0") if a]


def _cwd_linux(pid: int) -> str | None:
    try:
        return os.readlink(f"/proc/{pid}/cwd")
    except OSError:
        return None


def _open_paths_linux(pid: int) -> list[str]:
    out = []
    try:
        names = os.listdir(f"/proc/{pid}/fd")  # noqa: PTH208 - /proc listing
    except OSError:
        return out
    for name in names:
        with contextlib.suppress(OSError):
            out.append(os.readlink(f"/proc/{pid}/fd/{name}"))
    return out


def proc_argv(pid: int) -> list[str] | None:
    """argv of a same-user process, or None when it is gone or unreadable."""
    return _argv_mac(pid) if IS_MAC else _argv_linux(pid) if IS_LINUX else None


def proc_cwd(pid: int) -> str | None:
    return _cwd_mac(pid) if IS_MAC else _cwd_linux(pid) if IS_LINUX else None


def proc_open_paths(pid: int) -> list[str]:
    return _open_paths_mac(pid) if IS_MAC else _open_paths_linux(pid) if IS_LINUX else []


def all_pids() -> dict[int, int]:
    """pid -> ppid for every process of this user."""
    out = subprocess.run(
        ["ps", "-U", str(os.getuid()), "-o", "pid=,ppid="],  # noqa: S607 - ps from PATH is intended
        capture_output=True,
        text=True,
        check=False,
    ).stdout
    table: dict[int, int] = {}
    for line in out.splitlines():
        fields = line.split()
        if len(fields) == 2:  # noqa: PLR2004 - pid and ppid
            table[int(fields[0])] = int(fields[1])
    return table


def _inside(path: str | None, root: Path) -> bool:
    return bool(path) and (path == str(root) or path.startswith(str(root) + os.sep))


def anchored(pid: int, root: Path, binary_names: tuple[str, ...] = ("basemind",)) -> bool:
    """Whether `pid` has its cwd, or (for a basemind binary) an open file, inside `root`."""
    if _inside(proc_cwd(pid), root):
        return True
    argv = proc_argv(pid) or []
    if argv and Path(argv[0]).name in binary_names:
        return any(_inside(p, root) for p in proc_open_paths(pid))
    return False


def owned_processes(root: Path) -> dict[int, list[str]]:
    """pid -> argv of every live process of the run rooted at `root` (see the module docstring)."""
    table = all_pids()
    me = os.getpid()
    owned = {pid for pid in table if pid != me and anchored(pid, root)}
    grew = True
    while grew:
        grew = False
        for pid, ppid in table.items():
            if ppid in owned and pid not in owned and pid != me:
                owned.add(pid)
                grew = True
    return {pid: proc_argv(pid) or [] for pid in owned}


def still_owned(pid: int, root: Path) -> bool:
    """Re-check ownership of one pid right before signalling it."""
    if anchored(pid, root):
        return True
    ppid = all_pids().get(pid)
    seen = set()
    while ppid and ppid not in seen and ppid > 1:
        if anchored(ppid, root):
            return True
        seen.add(ppid)
        ppid = all_pids().get(ppid)
    return False


def role_of(argv: list[str]) -> str:
    """Classify a basemind process by its argv."""
    args = argv[1:]
    if args[:2] == ["comms", "daemon"]:
        return "daemon"
    if args[:1] == ["serve"]:
        return "serve"
    if argv and Path(argv[0]).name == "basemind":
        return "cli:" + " ".join(args[:2])
    return "child:" + (Path(argv[0]).name if argv else "?")


def pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def terminate_owned(root: Path, grace: float = 5.0) -> list[int]:
    """SIGTERM, then SIGKILL, every process of the run. Returns the pids signalled."""
    signalled: list[int] = []
    for pid in owned_processes(root):
        if still_owned(pid, root):
            with contextlib.suppress(ProcessLookupError):
                os.kill(pid, signal.SIGTERM)
                signalled.append(pid)
    deadline = time.monotonic() + grace
    while time.monotonic() < deadline and any(pid_alive(p) for p in signalled):
        time.sleep(0.1)
    for pid in signalled:
        if pid_alive(pid) and still_owned(pid, root):
            with contextlib.suppress(ProcessLookupError):
                os.kill(pid, signal.SIGKILL)
    return signalled


# ---------------------------------------------------------------------------------------------------
# Per-process sampling
# ---------------------------------------------------------------------------------------------------


@dataclass
class ProcSample:
    rss: int
    footprint: int | None
    vsz: int
    threads: int
    fds: int
    cpu_s: float
    wakeups: int | None


class _RusageV2(ctypes.Structure):
    _fields_ = [  # noqa: RUF012 - ctypes layout
        ("uuid", ctypes.c_uint8 * 16),
        ("user_time", ctypes.c_uint64),
        ("system_time", ctypes.c_uint64),
        ("pkg_idle_wkups", ctypes.c_uint64),
        ("interrupt_wkups", ctypes.c_uint64),
        ("pageins", ctypes.c_uint64),
        ("wired_size", ctypes.c_uint64),
        ("resident_size", ctypes.c_uint64),
        ("phys_footprint", ctypes.c_uint64),
        ("proc_start_abstime", ctypes.c_uint64),
        ("proc_exit_abstime", ctypes.c_uint64),
        ("child_user_time", ctypes.c_uint64),
        ("child_system_time", ctypes.c_uint64),
        ("child_pkg_idle_wkups", ctypes.c_uint64),
        ("child_interrupt_wkups", ctypes.c_uint64),
        ("child_pageins", ctypes.c_uint64),
        ("child_elapsed_abstime", ctypes.c_uint64),
        ("diskio_bytesread", ctypes.c_uint64),
        ("diskio_byteswritten", ctypes.c_uint64),
    ]


class _TaskInfo(ctypes.Structure):
    _fields_ = [  # noqa: RUF012 - ctypes layout
        ("virtual_size", ctypes.c_uint64),
        ("resident_size", ctypes.c_uint64),
        ("total_user", ctypes.c_uint64),
        ("total_system", ctypes.c_uint64),
        ("threads_user", ctypes.c_uint64),
        ("threads_system", ctypes.c_uint64),
        ("policy", ctypes.c_int32),
        ("faults", ctypes.c_int32),
        ("pageins", ctypes.c_int32),
        ("cow_faults", ctypes.c_int32),
        ("messages_sent", ctypes.c_int32),
        ("messages_received", ctypes.c_int32),
        ("syscalls_mach", ctypes.c_int32),
        ("syscalls_unix", ctypes.c_int32),
        ("csw", ctypes.c_int32),
        ("threadnum", ctypes.c_int32),
        ("numrunning", ctypes.c_int32),
        ("priority", ctypes.c_int32),
    ]


class _Timebase(ctypes.Structure):
    _fields_ = [("numer", ctypes.c_uint32), ("denom", ctypes.c_uint32)]  # noqa: RUF012 - ctypes layout


_TIMEBASE = 1.0
if IS_MAC:
    _tb = _Timebase()
    ctypes.CDLL("/usr/lib/libSystem.B.dylib").mach_timebase_info(ctypes.byref(_tb))
    _TIMEBASE = _tb.numer / _tb.denom


def _sample_mac(pid: int) -> ProcSample | None:
    proc_pidtaskinfo, proc_pidlistfds, rusage_info_v2, fdinfo_size = 4, 1, 2, 8
    ru = _RusageV2()
    if _libproc.proc_pid_rusage(pid, rusage_info_v2, ctypes.byref(ru)) != 0:
        return None
    ti = _TaskInfo()
    if _libproc.proc_pidinfo(pid, proc_pidtaskinfo, 0, ctypes.byref(ti), ctypes.sizeof(ti)) <= 0:
        return None
    need = _libproc.proc_pidinfo(pid, proc_pidlistfds, 0, None, 0)
    fds = 0
    if need > 0:
        buf = ctypes.create_string_buffer(need)
        got = _libproc.proc_pidinfo(pid, proc_pidlistfds, 0, buf, need)
        fds = max(got, 0) // fdinfo_size
    cpu_ns = (ru.user_time + ru.system_time) * _TIMEBASE
    return ProcSample(
        rss=int(ru.resident_size),
        footprint=int(ru.phys_footprint),
        vsz=int(ti.virtual_size),
        threads=int(ti.threadnum),
        fds=fds,
        cpu_s=cpu_ns / 1e9,
        wakeups=int(ru.pkg_idle_wkups + ru.interrupt_wkups),
    )


def _sample_linux(pid: int) -> ProcSample | None:
    try:
        status = Path(f"/proc/{pid}/status").read_text()
        stat = Path(f"/proc/{pid}/stat").read_text()
        fds = len(os.listdir(f"/proc/{pid}/fd"))
    except OSError:
        return None
    vals: dict[str, int] = {}
    for line in status.splitlines():
        key, _, rest = line.partition(":")
        parts = rest.split()
        if parts and parts[0].isdigit():
            vals[key] = int(parts[0])
    tail = stat.rsplit(")", 1)[1].split()
    ticks = os.sysconf("SC_CLK_TCK")
    utime, stime = int(tail[11]), int(tail[12])
    return ProcSample(
        rss=vals.get("VmRSS", 0) * 1024,
        footprint=None,
        vsz=vals.get("VmSize", 0) * 1024,
        threads=vals.get("Threads", 0),
        fds=fds,
        cpu_s=(utime + stime) / ticks,
        wakeups=None,
    )


def sample_pid(pid: int) -> ProcSample | None:
    """Resource sample of one process, or None when it is gone."""
    if IS_MAC:
        return _sample_mac(pid)
    if IS_LINUX:
        return _sample_linux(pid)
    return None


def dir_bytes(path: Path) -> tuple[int, int]:
    """(apparent bytes, allocated bytes) of a directory tree."""
    apparent = allocated = 0
    for dirpath, _dirs, files in os.walk(path):
        for name in files:
            try:
                st = os.lstat(os.path.join(dirpath, name))  # noqa: PTH116,PTH118 - os.walk hot loop
            except OSError:
                continue
            apparent += st.st_size
            allocated += getattr(st, "st_blocks", 0) * 512
    return apparent, allocated


# ---------------------------------------------------------------------------------------------------
# NDJSON sink
# ---------------------------------------------------------------------------------------------------


class Ndjson:
    """Thread-safe NDJSON writer; every record gets `t` (seconds since the run started)."""

    def __init__(self, path: Path, t0: float) -> None:
        self._fh: IO[str] = path.open("w", encoding="utf-8")
        self._lock = threading.Lock()
        self.t0 = t0

    def write(self, record: dict[str, Any]) -> None:
        record.setdefault("t", round(time.monotonic() - self.t0, 3))
        line = json.dumps(record, sort_keys=True)
        with self._lock:
            self._fh.write(line + "\n")
            self._fh.flush()

    def close(self) -> None:
        with self._lock:
            self._fh.close()


# ---------------------------------------------------------------------------------------------------
# MCP stdio client
# ---------------------------------------------------------------------------------------------------


class McpError(RuntimeError):
    """Transport-level failure: no answer, a JSON-RPC error, or a dead server."""


class McpSession:
    """One `basemind serve` process driven over newline-delimited JSON-RPC on stdio."""

    PROTOCOL = "2025-06-18"

    def __init__(self, binary: Path, repo: Path, env: dict[str, str], stderr_path: Path, name: str) -> None:
        self.name = name
        self._stderr = stderr_path.open("ab")
        self.proc = subprocess.Popen(  # noqa: S603 - the binary under test
            [str(binary), "serve"],
            cwd=repo,
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self._stderr,
        )
        self.pid = self.proc.pid
        self._next = 0
        self._pending: dict[int, queue.Queue[dict[str, Any]]] = {}
        self._lock = threading.Lock()
        self._write_lock = threading.Lock()
        self._reader = threading.Thread(target=self._read_loop, name=f"mcp-{name}", daemon=True)
        self._reader.start()
        self.tools: list[str] = []

    def _send(self, msg: dict[str, Any]) -> None:
        data = (json.dumps(msg) + "\n").encode()
        with self._write_lock:
            if self.proc.stdin is None or self.proc.stdin.closed:
                raise McpError(f"{self.name}: stdin closed")
            try:
                self.proc.stdin.write(data)
                self.proc.stdin.flush()
            except (BrokenPipeError, OSError) as exc:
                raise McpError(f"{self.name}: write failed: {exc}") from exc

    def _read_loop(self) -> None:
        assert self.proc.stdout is not None
        for raw in self.proc.stdout:
            try:
                msg = json.loads(raw)
            except json.JSONDecodeError:
                continue
            if "method" in msg and "id" in msg:
                self._answer_server_request(msg)
                continue
            if "id" in msg:
                with self._lock:
                    waiter = self._pending.pop(msg["id"], None)
                if waiter is not None:
                    waiter.put(msg)
        with self._lock:
            pending, self._pending = self._pending, {}
        for waiter in pending.values():
            waiter.put({"error": {"code": -1, "message": "server closed stdout"}})

    def _answer_server_request(self, msg: dict[str, Any]) -> None:
        if msg["method"] == "ping":
            reply: dict[str, Any] = {"jsonrpc": "2.0", "id": msg["id"], "result": {}}
        elif msg["method"] == "roots/list":
            reply = {"jsonrpc": "2.0", "id": msg["id"], "result": {"roots": []}}
        else:
            reply = {"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32601, "message": "not supported"}}
        with contextlib.suppress(McpError):
            self._send(reply)

    def request(self, method: str, params: dict[str, Any] | None = None, timeout: float = 120.0) -> dict[str, Any]:
        with self._lock:
            self._next += 1
            rid = self._next
            waiter: queue.Queue[dict[str, Any]] = queue.Queue(maxsize=1)
            self._pending[rid] = waiter
        msg: dict[str, Any] = {"jsonrpc": "2.0", "id": rid, "method": method}
        if params is not None:
            msg["params"] = params
        self._send(msg)
        try:
            reply = waiter.get(timeout=timeout)
        except queue.Empty as exc:
            with self._lock:
                self._pending.pop(rid, None)
            raise McpError(f"{self.name}: {method} timed out after {timeout}s") from exc
        if "error" in reply:
            raise McpError(f"{self.name}: {method}: {reply['error']}")
        return reply["result"]

    def notify(self, method: str, params: dict[str, Any] | None = None) -> None:
        msg: dict[str, Any] = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            msg["params"] = params
        self._send(msg)

    def initialize(self, timeout: float = 120.0) -> dict[str, Any]:
        result = self.request(
            "initialize",
            {
                "protocolVersion": self.PROTOCOL,
                "capabilities": {},
                "clientInfo": {"name": "basemind-hardening", "version": "1"},
            },
            timeout=timeout,
        )
        self.notify("notifications/initialized")
        listed = self.request("tools/list", {}, timeout=timeout)
        self.tools = [t["name"] for t in listed.get("tools", [])]
        return result

    def call(self, tool: str, args: dict[str, Any], timeout: float = 300.0) -> ToolResult:
        started = time.monotonic()
        result = self.request("tools/call", {"name": tool, "arguments": args}, timeout=timeout)
        return ToolResult.from_wire(result, time.monotonic() - started)

    def close(self, timeout: float = 15.0) -> int | None:
        """Close stdin (the host going away cleanly); the exit code, or None if it did not exit."""
        with self._write_lock:
            if self.proc.stdin and not self.proc.stdin.closed:
                with contextlib.suppress(OSError):
                    self.proc.stdin.close()
        try:
            return self.proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            return None
        finally:
            self._stderr.close()

    def kill(self) -> None:
        with contextlib.suppress(ProcessLookupError):
            self.proc.kill()
        with contextlib.suppress(subprocess.TimeoutExpired):
            self.proc.wait(timeout=10)
        self._stderr.close()


@dataclass
class ToolResult:
    is_error: bool
    text: str
    body: Any
    secs: float

    @classmethod
    def from_wire(cls, result: dict[str, Any], secs: float) -> ToolResult:
        text = "".join(c.get("text", "") for c in result.get("content", []) if c.get("type") == "text")
        body = result.get("structuredContent")
        if body is None:
            try:
                body = json.loads(text)
            except (json.JSONDecodeError, ValueError):
                body = None
        return cls(bool(result.get("isError")), text, body, secs)

    def feature_missing(self) -> bool:
        """An `isError` answer naming a Cargo feature this build lacks (`tool_contract::feature_missing`)."""
        low = self.text.lower()
        return self.is_error and (
            "feature" in low and ("not compiled" in low or "built without" in low or "--features" in low)
        )


def run_cli(
    binary: Path,
    args: list[str],
    cwd: Path,
    env: dict[str, str],
    timeout: float = 600.0,
) -> subprocess.CompletedProcess[str]:
    """Run a one-shot basemind CLI command (isolated env already checked by `Isolation.env`)."""
    return subprocess.run(  # noqa: S603 - the binary under test
        [str(binary), *args],
        cwd=cwd,
        env=env,
        capture_output=True,
        text=True,
        timeout=timeout,
        check=False,
    )


def binary_version(binary: Path, iso: Isolation) -> str:
    out = run_cli(binary, ["--version"], Path.cwd(), iso.env(), timeout=30)
    return out.stdout.strip()


def host_info() -> dict[str, Any]:
    return {
        "platform": platform.platform(),
        "machine": platform.machine(),
        "cpus": os.cpu_count(),
        "python": platform.python_version(),
    }


def install_cleanup(callback: Any) -> None:
    """Run `callback` once on normal exit, SIGINT, SIGTERM and SIGHUP."""
    done = threading.Event()

    def run_once() -> None:
        if not done.is_set():
            done.set()
            callback()

    def handler(signum: int, _frame: Any) -> None:
        log(f"signal {signum}: cleaning up")
        run_once()
        sys.exit(128 + signum)

    import atexit  # noqa: PLC0415 - local to keep the module import side-effect free

    atexit.register(run_once)
    for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(sig, handler)
